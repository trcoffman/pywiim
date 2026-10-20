"""LinkPlay WMRM wire formats, verified against a WiiM 4.3 group.

This is an independent implementation based on observed device traffic and
static inspection of publicly distributed LinkPlay 4.2 firmware. No vendor
binary is required at runtime. See docs/design/LINKPLAY_RECEIVER.md.
"""

from __future__ import annotations

import json
import struct
from collections import deque
from dataclasses import dataclass
from typing import Any

from Crypto.Cipher import DES3

# WMRM's common wire-encoding constant, also used by protocol 4.3. This is
# protocol obfuscation, not a Spotify credential or a device-specific secret.
_WIRE_KEY = bytes.fromhex("4107df53132d810517b85fa0d844449333986344d619bfc3")
_AUDIO_HEADER = struct.Struct("<IHHIIQQQIIQ")
_CLOCK = struct.Struct("<IIQQQ")
AUDIO_MAGIC = 0xFEDCBA98
AUDIO_HEADER_SIZE = _AUDIO_HEADER.size
CLOCK_PACKET_SIZE = _CLOCK.size
MAX_CONTROL_SIZE = 1024 * 1024


class ProtocolError(ValueError):
    """A peer sent an unsupported or malformed WMRM packet."""


@dataclass(frozen=True)
class ControlMessage:
    """A decoded WMRM command or heartbeat."""

    name: str
    category: int
    value: dict[str, Any]


def encode_control(value: dict[str, Any], *, name: str = "_linkplay_mrm", category: int = 253) -> bytes:
    """Encode one command as a JSON envelope with a JSON-string value and NUL padding."""
    inner = json.dumps(value, separators=(",", ":"), ensure_ascii=True)
    envelope = {
        "name": name,
        "category": category,
        "priprity": 1,  # The misspelling is part of the protocol.
        "seq": 0,
        "timestamp": 0,
        "value": inner,
    }
    plaintext = json.dumps(envelope, separators=(",", ":"), ensure_ascii=True).encode("utf-8")
    plaintext += b"\0" * (8 - len(plaintext) % 8)
    return DES3.new(_WIRE_KEY, DES3.MODE_ECB).encrypt(plaintext)


class ControlDecoder:
    """Decode arbitrary TCP fragments, including coalesced encrypted commands."""

    def __init__(self) -> None:
        self._cipher = DES3.new(_WIRE_KEY, DES3.MODE_ECB)
        self._encrypted = bytearray()
        self._plain = bytearray()

    def feed(self, data: bytes) -> list[ControlMessage]:
        """Return complete messages while retaining partial blocks and messages."""
        self._encrypted.extend(data)
        length = len(self._encrypted) // 8 * 8
        if length:
            self._plain.extend(self._cipher.decrypt(bytes(self._encrypted[:length])))
            del self._encrypted[:length]
        if len(self._plain) > MAX_CONTROL_SIZE:
            raise ProtocolError("WMRM control message exceeds 1 MiB")
        messages = []
        while (end := self._plain.find(b"\0")) >= 0:
            blob = bytes(self._plain[:end])
            del self._plain[: end + 1]
            if not blob:
                continue
            try:
                outer = json.loads(blob)
                inner = outer["value"]
                value = json.loads(inner)
                if not isinstance(value, dict):
                    raise ValueError("command value must be an object")
                messages.append(ControlMessage(str(outer["name"]), int(outer["category"]), value))
            except (ValueError, KeyError, TypeError) as exc:
                raise ProtocolError("Malformed WMRM control envelope") from exc
        if self._plain and self._plain[0] != ord("{"):
            raise ProtocolError("Malformed WMRM control envelope prefix")
        return messages


@dataclass(frozen=True)
class AudioPacket:
    """One WMRM audio frame and its leader clock reference (nanoseconds)."""

    kind: int
    format_word: int
    sequence: int
    sample_position: int
    reference_position: int
    reference_time_ns: int
    payload: bytes

    @property
    def sample_rate(self) -> int:
        """Rate encoded in bits 8..23 of the observed format word."""
        return (self.format_word >> 8) & 0xFFFF

    @property
    def channels(self) -> int:
        """Channel count encoded in the low nibble."""
        return self.format_word & 0xF

    def presentation_ns(self, leadtime_ms: int) -> int:
        """Map this frame's first sample onto the leader's presentation clock."""
        if self.sample_rate == 0:
            raise ProtocolError("WMRM audio packet has a zero sample rate")
        delta = (self.sample_position - self.reference_position) * 1_000_000_000 // self.sample_rate
        return self.reference_time_ns + delta + leadtime_ms * 1_000_000


class AudioDecoder:
    """Split an audio TCP stream into bounded WMRM frames."""

    def __init__(self) -> None:
        self._buffer = bytearray()

    def feed(self, data: bytes) -> list[AudioPacket]:
        """Return complete frames without assuming TCP preserves packet boundaries."""
        self._buffer.extend(data)
        packets = []
        while len(self._buffer) >= AUDIO_HEADER_SIZE:
            magic, size, kind, fmt, seq, pos, ref, ns, _, _, _ = _AUDIO_HEADER.unpack_from(self._buffer)
            if magic != AUDIO_MAGIC:
                raise ProtocolError(f"Unrecognized WMRM audio magic 0x{magic:08x}")
            if size < AUDIO_HEADER_SIZE:
                raise ProtocolError(f"Invalid WMRM audio packet size {size}")
            if len(self._buffer) < size:
                break
            payload = bytes(self._buffer[AUDIO_HEADER_SIZE:size])
            del self._buffer[:size]
            packets.append(AudioPacket(kind, fmt, seq, pos, ref, ns, payload))
        return packets


def clock_request(sequence: int, sent_ns: int) -> bytes:
    """Build a clock query using the client's monotonic nanosecond clock."""
    return _CLOCK.pack(sequence & 0xFFFFFFFF, 0, sent_ns, 0, 0)


class LeaderClock:
    """Estimate leader-to-local clock offset from the lowest-RTT recent query."""

    def __init__(self) -> None:
        self._samples: deque[tuple[int, int]] = deque(maxlen=30)
        self.offset_ns: int | None = None
        self.rtt_ns: int | None = None

    def observe(self, response: bytes, sequence: int, sent_ns: int, received_ns: int) -> None:
        """Validate an echoed query and update the clock mapping."""
        if len(response) != CLOCK_PACKET_SIZE:
            raise ProtocolError("Invalid WMRM clock response length")
        seq, reply, echoed, leader_ns, _ = _CLOCK.unpack(response)
        if seq != sequence & 0xFFFFFFFF or reply != 1 or echoed != sent_ns or received_ns < sent_ns:
            raise ProtocolError("WMRM clock response does not match its request")
        rtt = received_ns - sent_ns
        offset = leader_ns - (sent_ns + received_ns) // 2
        self._samples.append((rtt, offset))
        self.rtt_ns, self.offset_ns = min(self._samples, key=lambda sample: sample[0])

    def local_ns(self, leader_ns: int) -> int:
        """Translate a leader timestamp once at least one query has succeeded."""
        if self.offset_ns is None:
            raise ProtocolError("Leader clock is not synchronized")
        return leader_ns - self.offset_ns
