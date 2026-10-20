# Mac Spotify group receiver

`wiim-receiver` makes the Mac a separate follower in a WiiM multiroom group.
The existing WiiM leader handles Spotify Connect; the Mac receives and plays
the group's audio with its own name and persistent speaker ID.

## Run

The native Rust receiver is recommended for playback. It decodes AAC on its own
thread and feeds the native audio callback through a bounded, lock-free queue.
The callback does not allocate, acquire mutexes, or call Python.

Install Rust 1.91 or later using [rustup](https://rustup.rs/), then run from this checkout:

```sh
cargo run --release -- --list-outputs
cargo run --release -- SPEAKER_IP --name 'My Mac'
```

The first run builds the receiver. Subsequent launches can use the binary directly:

```sh
./target/release/wiim-receiver SPEAKER_IP --name 'My Mac'
```

To install a standalone command, use `cargo install --locked --path rust/receiver`.
The executable requires neither Python nor FFmpeg. Use a release build for listening.
The earlier Python implementation remains available with
`uv run wiim-receiver SPEAKER_IP --name 'My Mac'`; its dependencies are included
in the default Python install.

Pass the IP of either the leader or an existing follower. The receiver resolves
a follower's `master_ip` automatically. Select the existing WiiM group in
Spotify, then start music. No Spotify credentials or additional Connect session
are needed on the Mac.

The default output is the Mac's system output selected at startup. To choose another:

```sh
./target/release/wiim-receiver SPEAKER_IP --name 'My Mac' --output 'MacBook Pro Speakers'
```

The terminal shows group membership, track information, local volume, clock
round-trip time, buffered audio, audio gaps, late frames, device errors, and
decoder errors. Keys:

- `+` / `-`: change the Mac receiver's gain.
- `m`: mute or unmute the Mac receiver.
- `q` or Ctrl+C: log out and close the Mac's connections.

The receiver never changes the system volume or removes other speakers.
It retries a disconnected leader with bounded backoff. The default initial
receiver gain is 30%; override with `--volume 20`.

The receiver stays joined while Spotify is stopped and probes the audio transport
after registration and clock synchronization. Playback remains gated to Spotify;
waiting for a source update before connecting can stall playback startup.
The leader may keep its audio port closed while idle.
Temporary audio connection refusals or closures retry without leaving the group;
control and clock connections continue running. Connection errors include the
leader IP and port to distinguish audio transport failures from membership failures.
The receiver also refreshes the leader's read-only playback status once per second
to recover missing start/source updates. A response is discarded if a newer group
control update arrived during the request, so a delayed response cannot undo a pause.

For timing adjustment, `--latency-ms 25` delays the Mac by 25 ms;
`--latency-ms -25` advances it. This trims output-device latency in addition to
the group clock synchronization. Acoustic alignment across different DACs and
headphones still needs a listening check.

Headless runs can use `--no-tui`; `--duration 30` leaves after 30 seconds.
Use `--verbose --no-tui` for diagnostic logs.

## Popping and timing diagnostics

The Rust decoder keeps consecutive PCM samples on a continuous timeline, smoothing
the small timestamp-reference jumps observed between packets from the leader.
The callback interpolates across AAC packet boundaries and resamples to
the output device's native sample rate. Clock offset updates slew gradually
rather than stepping the playback position; volume, mute, and missing-audio
transitions have short fades. The requested output buffer defaults to 1024
frames (about 21 ms at 48 kHz), clamped to the device's supported range.
`--buffer-frames 2048` gives the hardware callback more scheduling headroom.
`--latency-ms` adjusts group alignment; it is not a jitter-buffer size.

Run with `--verbose --no-tui` to print counters every two seconds:

- **Audio gaps / missing frames:** decoded audio was unavailable at its playback
  deadline after playback had begun. Initial waiting and intentional pauses are excluded.
- **Late frames:** queued samples arrived behind the playback clock.
- **Device errors:** errors reported by the native audio backend.
- **Decode errors:** AAC frames that could not be decoded.

Zero device errors alone does not establish uninterrupted playback. Network
gaps, source discontinuities, and output hardware can still cause artifacts;
audible synchronization and popping need a listening check on the affected Mac.

## Discovery and grouping

The process advertises an SSDP MediaRenderer identity and serves device info
and group/volume HTTP commands on port **49152**, which does not require root.
Change it with `--http-port`. `--interface` selects the Mac's advertised LAN
IPv4 address. `--no-discovery` disables these listeners while retaining direct
group membership and group control over the WMRM connection.

If startup reports an HTTP address already in use, choose a different port with
`--http-port 8088`. Discovery also listens on UDP port **1900**; changing the
HTTP port does not change that port. On macOS, the receiver enables multicast
port sharing with other apps that permit sharing. If another app reserves the
discovery port exclusively, quit that app or join directly without discovery:

```sh
./target/release/wiim-receiver SPEAKER_IP --name 'My Mac' --no-discovery
```

Direct CLI grouping is the verified path. Initial pairing from the WiiM Home
app has not been validated; an app that insists on HTTP port 80 or HTTPS port
443 may not discover/control the virtual device through its advertised port.
This does not prevent direct CLI joining or registration in the leader's
`multiroom:getSlaveList`. The HTTP surface implements group selection, local
volume/mute, and status; it does not offer a general DLNA player.

## Current scope and verification

This is an **experimental WMRM 4.3 receiver** supporting the observed stereo AAC
group transport, gated to Spotify playback mode 31. Other input sources are
muted. It does not advertise an independent Spotify Connect endpoint while
ungrouped, decode Spotify directly, or implement legacy Wi-Fi Direct grouping.

Initial Python hardware verification used a WiiM Amp Pro leader running
`Linkplay.5.2.830031` and a WiiM Sound Lite follower running
`Linkplay.5.2.829446`, both reporting WMRM 4.3. The Mac registered independently
as an additional follower, received and decoded 44.1 kHz stereo AAC through
CoreAudio, and logged out while preserving the original four followers. A
20-second playback check reported zero output underruns. Timing is scheduled
against the leader and DAC clocks; no acoustic measurement is claimed.

Native Rust checks received live Spotify through the OS default LG HDR 4K
output at 48 kHz with a separate test UUID and zero receiver gain. The receiver
logged out cleanly, preserving all five existing followers. A 10-second playback
state recovery check decoded 442 AAC packets with zero reported gaps, late frames,
decoder errors, or native backend errors. These are software diagnostics;
elimination of audible artifacts and acoustic alignment are not claimed without
a listening check on the affected Mac.

Unit tests cover actual captured control framing, arbitrary TCP fragmentation,
clock validation, presentation timestamps, callback scheduling, volume/mute,
source gating, HTTP control, and SSDP responses:

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

The generated AAC fixture under `rust/receiver/tests/fixtures` is a short
synthetic sine tone used to exercise the Rust decoder, not captured music.

The Python reference implementation's tests remain available:

```sh
uv pip install --python .venv/bin/python -e '.[dev]'
.venv/bin/pytest tests/unit/receiver -q
```

Protocol details and research sources: [LinkPlay receiver](../design/LINKPLAY_RECEIVER.md).
