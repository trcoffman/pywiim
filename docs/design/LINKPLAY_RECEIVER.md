# LinkPlay receiver interoperability notes

The controller API provides the discovery and group-command vocabulary. The
receiver additionally implements the follower connections opened by real
WMRM 4.3 devices. These notes describe observed behavior, not a complete public
LinkPlay specification.

## Evidence and sources

- This repository's `GroupAPI.join_slave()` sends
  `ConnectMasterAp:JoinGroupMaster:eth<leader>:wifi0.0.0.0` to a follower.
- [Arylic's HTTP API](https://developer.arylic.com/httpapi/) documents that the
  leader supplies the group's Spotify session and that join commands go to
  the follower. It does not specify the follower's audio connection.
- [LinkPlay firmware research](https://github.com/Jan21493/Linkplay) documents
  publicly distributed firmware images. Static inspection of the public
  2022 A31 image's `libwmrm.so` identified the command envelope, wire encoding,
  clock exchange, and frame layout. The older
  [LS150 research](https://github.com/Crymeiriver/LS150) was useful background;
  its port numbers do not match the tested 4.3 firmware.
- A temporary, explicitly authorized TCP relay between a real follower and
  its leader verified current traffic. The follower was restored after each
  experiment. A separate login with a new Mac UUID verified actual independent
  membership, audio delivery, and logout. No vendor firmware code or binary is
  bundled with the receiver.

## Observed connections

| Leader TCP port | Purpose | Direction |
| --- | --- | --- |
| 24012 | Clock request/reply | Follower opens; bidirectional |
| 25012 | Membership, playback commands, metadata, heartbeat | Follower opens; bidirectional |
| 26001 | Framed AAC audio | Follower opens; leader sends |

An audio connection is accepted only after the source IP has registered through
the command connection. Merely opening the audio port does not create a client.
The command login contains a unique UUID, display name, type, WMRM version,
volume, channel, and capability flags. A `group_info` message containing the
Mac UUID confirms membership. `logout` removes only this UUID. Client and
leader heartbeats keep the command channel alive.

Command bytes use 3DES ECB with a common WMRM wire-encoding constant, followed
by NUL padding to the next eight-byte boundary, including a full padding block
for aligned messages. This is protocol obfuscation, not a Spotify credential.
The 4.2 encoding constant also decodes the observed 4.3 messages. The plaintext
is JSON with `name`, `category`, `priprity` (the wire spelling), `seq`,
`timestamp`, and `value`. `value` is itself a serialized JSON string. TCP may
split an encrypted block or combine several padded messages in a single read.

Clock messages are 32-byte little-endian `<IIQQQ>` records: sequence, request/
reply flag, echoed client monotonic nanoseconds, leader monotonic nanoseconds,
and a reserved field. Replies use flag 1. The receiver chooses the smallest
round-trip sample in a rolling window to estimate the clock offset.

## Audio and synchronization

The observed header is 56 bytes, described by `<IHHIIQQQIIQ>`:

| Offset | Field |
| --- | --- |
| 0 | `0xFEDCBA98` magic, uint32 |
| 4 | Total frame size including header, uint16 |
| 6 | Codec identifier, uint16; observed AAC is 10 |
| 8 | Format word; channels in low nibble, sample rate in bits 8..23 |
| 12 | Sequence number, uint32 |
| 16 | First sample's position, uint64 |
| 24 | Reference sample position, uint64 |
| 32 | Reference leader clock time in nanoseconds, uint64 |
| 40 | Additional state, uint32 |
| 44 | Reference sequence, uint32 |
| 48 | Additional reference position, uint64 |

AAC payloads include ADTS headers and are decoded independently of Spotify.
Only the fields verified by the receiver are interpreted. The leader also
sends `playback_sync` with status, sample position, host time, and `leadtime` in
milliseconds. The observed lead time was 800 ms. A packet's presentation time
is its clock reference plus the sample-position delta and that lead time.

The receiver maps that timestamp to the local monotonic clock. PortAudio's DAC
clock is separately calibrated against the same local clock. The output callback
reads the decoded timeline at the DAC's presentation timestamps, interpolating
fractional sample offsets to compensate drift. Past audio is discarded, missing
audio becomes silence, and the decoded queue is bounded. AAC decoding stays
outside the output callback. `--latency-ms` applies an explicit hardware trim.

### Native Rust playback

The Rust receiver under `rust/receiver` shares the observed wire protocol and
speaker identity derivation with the Python reference. Symphonia decodes ADTS
AAC on a dedicated thread. A preallocated SPSC ring stores fixed-size PCM
frames with leader timestamps, source positions, and reset generations. The
CPAL output callback owns its queue consumer and uses atomics for controls and
diagnostic counters; it performs no allocation, locking, decoding, or logging.

Playback uses CPAL's predicted DAC timestamps, calibrated to the receiver's
monotonic epoch. The callback interpolates across packet boundaries at the
output device's native sample rate, slews clock offset changes at up to 200 ppm,
and applies short gain ramps. Seek, stop, source change, and reconnect increment
a generation so old frames are discarded. The queue and per-callback discard
work are bounded, and decoder backpressure occurs on the worker thread.
Missing audio and late frames are counted separately from backend errors.

Live tracing also found header timestamp-reference changes of hundreds of
microseconds between consecutive AAC packets. Applying every reference directly
to its packet creates small PCM gaps or overlaps even with a full queue. A source
timeline now schedules consecutive samples continuously and absorbs small
reference corrections through a bounded rate adjustment. Missing source positions,
format changes, and timestamp jumps of 50 ms or more establish a new timeline.

The Rust implementation is standalone; the Python controller APIs and Python
receiver remain available for comparison. Both derive the same default UUID
from the hostname, so running both simultaneously requires distinct `--uuid`
values. Each process still needs a distinct HTTP port if discovery is enabled.

## Deliberate compatibility boundaries

The implementation reuses the controller only to resolve an initial follower's
leader. It does not modify `Player`, its role caches, or any existing controller
API. Audio and wire-codec dependencies are installed by default. Only WMRM 4.3 and
AAC mode 10 have been tested on current hardware. Capability flags, fixed ports,
other codecs, and future firmware may require further interoperability work.

SSDP/HTTP advertisement is a minimal discovery and group-control surface. It
does not promise a fully conforming UPnP/DLNA renderer or universal WiiM Home app
pairing. Acoustic synchronization, recovery from every hardware/network failure,
and every remote control command have not been exhaustively validated.
