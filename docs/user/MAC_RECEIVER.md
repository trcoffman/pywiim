# Mac Spotify group receiver

`wiim-receiver` makes the Mac a separate follower in a WiiM multiroom group.
The existing WiiM leader handles Spotify Connect; the Mac receives and plays
the group's audio with its own name and persistent speaker ID.

## Run

From this checkout:

```sh
uv run wiim-receiver --list-outputs
uv run wiim-receiver SPEAKER_IP --name 'My Mac'
```

Receiver dependencies are installed automatically; no extra is needed.
You can also install with `pip install -e .` and run `wiim-receiver` directly.

Pass the IP of either the leader or an existing follower. The receiver resolves
a follower's `master_ip` automatically. Select the existing WiiM group in
Spotify, then start music. No Spotify credentials or additional Connect session
are needed on the Mac.

The default output is the Mac's current system output. To choose another:

```sh
uv run wiim-receiver SPEAKER_IP --name 'My Mac' --output 'MacBook Pro Speakers'
```

The terminal shows group membership, track information, local volume, clock
round-trip time, buffered audio, and output underruns. Keys:

- `+` / `-`: change the Mac receiver's gain.
- `m`: mute or unmute the Mac receiver.
- `q` or Ctrl+C: log out and close the Mac's connections.

The receiver never changes the system volume or removes other speakers.
It retries a disconnected leader with bounded backoff. The default initial
receiver gain is 30%; override with `--volume 20`.

For timing adjustment, `--latency-ms 25` delays the Mac by 25 ms;
`--latency-ms -25` advances it. This trims output-device latency in addition to
the group clock synchronization. Acoustic alignment across different DACs and
headphones still needs a listening check.

Headless runs can use `--no-tui`; `--duration 30` leaves after 30 seconds.
Use `--verbose --no-tui` for diagnostic logs.

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
uv run wiim-receiver SPEAKER_IP --name 'My Mac' --no-discovery
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

Hardware verification used a WiiM Amp Pro leader running
`Linkplay.5.2.830031` and a WiiM Sound Lite follower running
`Linkplay.5.2.829446`, both reporting WMRM 4.3. The Mac registered independently
as an additional follower, received and decoded 44.1 kHz stereo AAC through
CoreAudio, and logged out while preserving the original four followers. A
20-second playback check reported zero output underruns. Timing is scheduled
against the leader and DAC clocks; no acoustic measurement is claimed.

Unit tests cover actual captured control framing, arbitrary TCP fragmentation,
clock validation, presentation timestamps, callback scheduling, volume/mute,
source gating, HTTP control, and SSDP responses:

```sh
uv pip install --python .venv/bin/python -e '.[dev]'
.venv/bin/pytest tests/unit/receiver -q
```

Protocol details and research sources: [LinkPlay receiver](../design/LINKPLAY_RECEIVER.md).
