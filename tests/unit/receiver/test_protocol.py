"""Protocol tests include an actual anonymous WiiM heartbeat wire fixture."""

import struct

import pytest

pytest.importorskip("Crypto")

from pywiim.receiver.protocol import (  # noqa: E402
    AUDIO_HEADER_SIZE,
    AudioDecoder,
    ControlDecoder,
    LeaderClock,
    ProtocolError,
    clock_request,
    encode_control,
)

# Captured from a WMRM 4.3 leader, containing only playback counters and clock
# ticks; no account credentials, names, network addresses, or audio.
HEARTBEAT = bytes.fromhex(
    "03b4ec8c48b5ee8ccd167c993f15dcc677b0b997de411f4ca3334de728c9703ac1e2"
    "62db00d384efab99b4a4f6775c79d039b5196ee9fad447233134dc7f25ca22459d945"
    "bf22051da389fc01a04c3f8b85c0bb7c927fc26062b5d57bcaf3fc63edd8d8778038"
    "39b2be6d0c2ee69218c7cbe08cab5a23ac851600ea1d176da674a915f40a0ea65267"
    "5133c926a63bf3466b3fbe3b94078a937dca93cdc43e2837988f02f633d24d2"
)
HEARTBEAT_VALUE = {"status": 1, "render_pts": 25336832, "render_time": 263499335809825, "cache": 0}


@pytest.mark.parametrize("fragment_size", [1, 3, 7, 8, 9, 17, 4096])
def test_actual_heartbeat_with_arbitrary_tcp_fragments(fragment_size):
    decoder = ControlDecoder()
    messages = []
    for offset in range(0, len(HEARTBEAT), fragment_size):
        messages.extend(decoder.feed(HEARTBEAT[offset : offset + fragment_size]))
    assert len(messages) == 1
    assert messages[0].name == "_heart_tick"
    assert messages[0].category == 0
    assert messages[0].value == HEARTBEAT_VALUE


def test_coalesced_messages_and_json_string_escaping():
    login = {"cmd": "login", "name": 'Mac "Living room" 🎵', "uuid": "000011112222333344445555"}
    encoded = encode_control(login) + HEARTBEAT + encode_control({"cmd": "logout"})
    decoded = ControlDecoder().feed(encoded)
    assert [message.value for message in decoded] == [login, HEARTBEAT_VALUE, {"cmd": "logout"}]


def test_reject_corrupt_control_envelope():
    with pytest.raises(ProtocolError, match="Malformed"):
        ControlDecoder().feed(b"\0" * 80)


def audio_frame(payload=b"AAC", *, kind=10, position=1024):
    return (
        struct.pack(
            "<IHHIIQQQIIQ",
            0xFEDCBA98,
            AUDIO_HEADER_SIZE + len(payload),
            kind,
            0x01AC4422,
            12,
            position,
            2048,
            10_000_000_000,
            0,
            13,
            2048,
        )
        + payload
    )


@pytest.mark.parametrize("fragment_size", [1, 3, 55, 56, 57, 65536])
def test_audio_fragmentation_and_timestamp_mapping(fragment_size):
    decoder = AudioDecoder()
    wire = audio_frame() + audio_frame(b"second", position=2048)
    packets = []
    for offset in range(0, len(wire), fragment_size):
        packets.extend(decoder.feed(wire[offset : offset + fragment_size]))
    assert [packet.payload for packet in packets] == [b"AAC", b"second"]
    assert (packets[0].sample_rate, packets[0].channels, packets[0].kind) == (44100, 2, 10)
    assert packets[1].presentation_ns(800) == 10_800_000_000
    assert packets[0].presentation_ns(800) == 10_800_000_000 - (1024 * 1_000_000_000 + 44099) // 44100


@pytest.mark.parametrize("corrupt", [b"WRNG" + audio_frame()[4:], struct.pack("<IH", 0xFEDCBA98, 1) + b"\0" * 60])
def test_reject_invalid_audio_frame(corrupt):
    with pytest.raises(ProtocolError):
        AudioDecoder().feed(corrupt)


def test_clock_uses_best_recent_round_trip():
    clock = LeaderClock()
    with pytest.raises(ProtocolError, match="not synchronized"):
        clock.local_ns(1)
    assert clock_request(7, 1000) == struct.pack("<IIQQQ", 7, 0, 1000, 0, 0)
    clock.observe(struct.pack("<IIQQQ", 7, 1, 1000, 1110, 0), 7, 1000, 1020)
    assert clock.offset_ns == 100
    assert clock.local_ns(5100) == 5000
    clock.observe(struct.pack("<IIQQQ", 8, 1, 2000, 2250, 0), 8, 2000, 2200)
    assert clock.offset_ns == 100  # A slower reply must not shift the clock.


@pytest.mark.parametrize("response", [b"short", struct.pack("<IIQQQ", 99, 1, 1000, 1110, 0)])
def test_reject_unmatched_clock_response(response):
    with pytest.raises(ProtocolError):
        LeaderClock().observe(response, 7, 1000, 1020)
