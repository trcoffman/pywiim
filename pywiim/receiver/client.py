"""An independently registered WMRM follower with clocked AAC playback."""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import socket
import time
import uuid
from dataclasses import dataclass
from typing import Any
from xml.etree import ElementTree

from .audio import AudioOutput
from .protocol import (
    CLOCK_PACKET_SIZE,
    AudioDecoder,
    ControlDecoder,
    ControlMessage,
    LeaderClock,
    ProtocolError,
    clock_request,
    encode_control,
)

_LOGGER = logging.getLogger(__name__)


@dataclass
class ReceiverState:
    """Live terminal and HTTP status for the virtual speaker."""

    name: str
    uuid: str
    master_host: str | None = None
    master_name: str = ""
    master_uuid: str = ""
    connected: bool = False
    playing: bool = False
    source: int | None = None
    volume: int = 30
    muted: bool = False
    channel: int = 0
    title: str = ""
    artist: str = ""
    album: str = ""
    leadtime_ms: int = 800
    message: str = "Waiting to join a WiiM group"


class Receiver:
    """Join a leader using this Mac's own identity; reconnect until stopped."""

    def __init__(
        self,
        name: str,
        *,
        device_uuid: str | None = None,
        volume: int = 30,
        output: AudioOutput | None = None,
    ) -> None:
        stable_id = uuid.uuid5(uuid.NAMESPACE_DNS, f"pywiim.receiver:{socket.gethostname()}")
        self.state = ReceiverState(name, device_uuid or stable_id.hex[:24].upper(), volume=volume)
        self.output = output or AudioOutput()
        self.clock = LeaderClock()
        self.stop = asyncio.Event()
        self._changed = asyncio.Event()
        self._registered = asyncio.Event()
        self._clock_ready = asyncio.Event()
        self._audio_ready = asyncio.Event()
        self._writer: asyncio.StreamWriter | None = None
        self._metadata = ""

    def select_master(self, host: str | None) -> None:
        """Join a leader, change leaders, or leave without changing other speakers."""
        if host != self.state.master_host:
            self.state.master_host = host
            self._changed.set()

    def set_volume(self, volume: int) -> None:
        """Change this receiver's output gain, never the Mac's system volume."""
        if not 0 <= volume <= 100:
            raise ValueError("volume must be between 0 and 100")
        self.state.volume = volume
        self._update_output()

    def set_mute(self, muted: bool) -> None:
        """Mute this receiver only."""
        self.state.muted = muted
        self._update_output()

    def _update_output(self) -> None:
        if self.output.buffer:
            self.output.buffer.volume = self.state.volume
            self.output.buffer.muted = self.state.muted
            self.output.buffer.channel = self.state.channel
            self.output.buffer.enabled = self.state.playing and self.state.source == 31

    def _login(self) -> dict[str, Any]:
        return {
            "cmd": "login",
            "name": self.state.name,
            "uuid": self.state.uuid,
            "wmrm_version": "4.3",
            "type": "WiiMu-Mac",
            "fw_version": "pywiim.receiver.0.1",
            "has_new_version": 0,
            "channel": self.state.channel,
            "volume": self.state.volume,
            "mute": int(self.state.muted),
            "battery_percent": 0,
            "battery_charging": 0,
            "mask": 0,
            "BPS": 0,
            "flags": 175,  # Capabilities observed on the AAC-capable 4.3 follower.
            "sub": 0,
            "sub_master_uuid": "",
        }

    async def _send(self, value: dict[str, Any], *, name: str = "_linkplay_mrm", category: int = 253) -> None:
        if self._writer is not None:
            self._writer.write(encode_control(value, name=name, category=category))
            await self._writer.drain()

    async def run(self) -> None:
        """Maintain membership until stop is set, with bounded reconnect backoff."""
        retry_delay = 1.0
        while not self.stop.is_set():
            self._changed.clear()
            host = self.state.master_host
            if not host:
                self.state.message = "Waiting to join a WiiM group"
                await self._wait_for_change()
                continue
            try:
                await self._session(host)
                retry_delay = 1
            except (OSError, TimeoutError, ProtocolError, ValueError) as exc:
                self.state.message = f"Disconnected: {exc}. Retrying in {retry_delay:g}s"
                _LOGGER.warning("%s", self.state.message)
                await self._wait_for_change(retry_delay)
                retry_delay = min(retry_delay * 2, 8)

    async def _wait_for_change(self, timeout: float | None = None) -> None:
        tasks = [asyncio.create_task(self._changed.wait()), asyncio.create_task(self.stop.wait())]
        try:
            await asyncio.wait(tasks, timeout=timeout, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def _session(self, host: str) -> None:
        self.clock = LeaderClock()
        self._registered.clear()
        self._clock_ready.clear()
        self._audio_ready.clear()
        self.state.source = None
        self.state.playing = False
        self.state.message = f"Joining {host}"
        tasks: list[asyncio.Task] = []
        writer = None
        try:
            reader, writer = await asyncio.wait_for(asyncio.open_connection(host, 25012), 5)
            self._writer = writer
            await self._send(self._login())
            tasks = [
                asyncio.create_task(self._control_loop(reader)),
                asyncio.create_task(self._clock_loop(host)),
                asyncio.create_task(self._audio_loop(host)),
                asyncio.create_task(self._heartbeat_loop()),
                asyncio.create_task(self._changed.wait()),
                asyncio.create_task(self.stop.wait()),
                asyncio.create_task(self._registration_timeout()),
            ]
            done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            # The registration guard remains pending after success; only an
            # actual transport exit, selection change, or stop ends a session.
            for task in done:
                task.result()
        finally:
            if writer and not writer.is_closing():
                with contextlib.suppress(OSError, TimeoutError):
                    await asyncio.wait_for(
                        self._send({"cmd": "logout", "name": self.state.name, "uuid": self.state.uuid}), 1
                    )
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            if writer:
                writer.close()
                with contextlib.suppress(OSError):
                    await writer.wait_closed()
            self._writer = None
            self.state.connected = False
            self.state.playing = False
            self._metadata = ""
            self.output.close()
            self.output.reset()

    async def _registration_timeout(self) -> None:
        await asyncio.wait_for(self._registered.wait(), 8)
        await asyncio.Future()  # Success is not a reason to end the session.

    async def _control_loop(self, reader: asyncio.StreamReader) -> None:
        decoder = ControlDecoder()
        while data := await reader.read(65536):
            for message in decoder.feed(data):
                self._handle_control(message)
        raise ConnectionError("WiiM closed the group control connection")

    def _handle_control(self, message: ControlMessage) -> None:
        value = message.value
        cmd = value.get("cmd", "")
        if message.name == "_heart_tick":
            return
        if cmd == "update":
            self.state.master_name = str(value.get("name", ""))
            self.state.master_uuid = str(value.get("uuid", ""))
        elif cmd == "group_info":
            group = json.loads(value.get("body", "{}"))
            if any(node.get("uuid") == self.state.uuid for node in group.get("list", [])):
                self._registered.set()
                self.state.connected = True
                self.state.message = "Joined; waiting for Spotify"
            elif self._registered.is_set():
                raise ConnectionError("Mac was removed from the WiiM group")
        elif cmd == "playback_sync":
            self.state.playing = value.get("status") == 1
            self.state.leadtime_ms = int(value.get("leadtime", 800))
            if not 0 <= self.state.leadtime_ms <= 10000:
                raise ProtocolError("Invalid group playback lead time")
            self._audio_ready.set()
            if not self.state.playing:
                self.output.reset()
        elif cmd in ("playback_start", "playback_resume"):
            self.state.playing = True
            self._audio_ready.set()
        elif cmd in ("playback_stop", "playback_pause", "playback_flush", "playback_prepare"):
            self.state.playing = False
            self.output.reset()
        elif cmd == "metaData":
            if "metaDataSplit" in value:
                self._metadata += str(value["metaDataSplit"])
            if "metaDataEnd" in value:
                self._metadata += str(value["metaDataEnd"])
                self._parse_metadata()
                self._metadata = ""
            if len(self._metadata) > 131072:
                self._metadata = ""
                raise ProtocolError("Group metadata exceeds 128 KiB")
            if "playmode" in value:
                self.state.source = int(value["playmode"])
                if self.state.source != 31:
                    self.state.message = "Waiting for Spotify (other sources are muted)"
                    self.output.reset()
        elif cmd == "control":
            operation = value.get("operation")
            setting = str(value.get("value", ""))
            if operation in ("volume", "vol"):
                self.set_volume(int(setting))
            elif operation == "mute":
                self.set_mute(setting == "1")
            elif operation == "channel":
                channel = int(setting)
                if channel not in (0, 1, 2):
                    raise ProtocolError("Invalid speaker channel")
                self.state.channel = channel
        self._update_output()

    def _parse_metadata(self) -> None:
        try:
            root = ElementTree.fromstring(self._metadata)
        except ElementTree.ParseError:
            return
        for node in root.iter():
            field = node.tag.rsplit("}", 1)[-1]
            text = "".join(c for c in (node.text or "") if c.isprintable())
            if field == "title":
                self.state.title = text
            elif field in ("artist", "creator"):
                self.state.artist = text
            elif field == "album":
                self.state.album = text

    async def _clock_loop(self, host: str) -> None:
        reader, writer = await asyncio.wait_for(asyncio.open_connection(host, 24012), 5)
        try:
            sequence = 0
            while True:
                sent = time.monotonic_ns()
                writer.write(clock_request(sequence, sent))
                await writer.drain()
                response = await asyncio.wait_for(reader.readexactly(CLOCK_PACKET_SIZE), 3)
                self.clock.observe(response, sequence, sent, time.monotonic_ns())
                self._clock_ready.set()
                sequence += 1
                await asyncio.sleep(0.2)
        except asyncio.IncompleteReadError as exc:
            raise ConnectionError("WiiM closed the clock connection") from exc
        finally:
            writer.close()
            with contextlib.suppress(OSError):
                await writer.wait_closed()

    async def _heartbeat_loop(self) -> None:
        await self._registered.wait()
        while True:
            buffer = self.output.buffer
            local_ns = time.monotonic_ns()
            await self._send(
                {
                    "status": int(self.state.playing),
                    "render_pts": buffer.rendered_position if buffer else 0,
                    "render_time": local_ns + (self.clock.offset_ns or 0),
                    "cache": 0,
                },
                name="_heart_tick",
                category=0,
            )
            await asyncio.sleep(3)

    async def _audio_loop(self, host: str) -> None:
        await self._registered.wait()
        await self._clock_ready.wait()
        await self._audio_ready.wait()
        while True:
            writer = None
            try:
                reader, writer = await asyncio.wait_for(asyncio.open_connection(host, 26001), 5)
                decoder = AudioDecoder()
                while data := await reader.read(65536):
                    for packet in decoder.feed(data):
                        if self.state.source != 31 or not self.state.playing:
                            continue
                        # A leader can send prefetched audio faster than real
                        # time. Let TCP apply backpressure rather than evicting
                        # samples that have not reached their DAC time yet.
                        while self.output.buffer and self.output.buffer.buffered_ms > 2000:
                            await asyncio.sleep(0.02)
                        if self.state.source != 31 or not self.state.playing:
                            continue
                        target_ns = self.clock.local_ns(packet.presentation_ns(self.state.leadtime_ms))
                        self.output.feed(packet, target_ns)
                        self._update_output()
                        self.state.message = "Playing Spotify"
            except (OSError, TimeoutError) as exc:
                self.state.message = f"Waiting for group audio: {exc}"
            finally:
                if writer:
                    writer.close()
                    with contextlib.suppress(OSError):
                        await writer.wait_closed()
            self.output.reset()
            await asyncio.sleep(1)
