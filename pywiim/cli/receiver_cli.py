"""Run the macOS Spotify group receiver: python -m pywiim.cli.receiver_cli."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import logging
import socket
import sys
import time
from typing import Any


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Make this Mac a Spotify speaker in a WiiM WMRM 4.3 group.")
    parser.add_argument("speaker", nargs="?", help="IP of a WiiM leader or any member of its group")
    parser.add_argument("--name", default=socket.gethostname().split(".")[0], help="Virtual speaker's display name")
    parser.add_argument("--uuid", help="Override the stable 24-character hexadecimal speaker ID")
    parser.add_argument("--output", help="CoreAudio output device name or index; defaults to the system output")
    parser.add_argument("--list-outputs", action="store_true", help="List output device names and exit")
    parser.add_argument("--volume", type=int, default=30, help="Initial receiver volume, 0..100 (default: 30)")
    parser.add_argument("--latency-ms", type=float, default=0, help="Audio alignment trim; positive delays this Mac")
    parser.add_argument("--interface", help="Mac's LAN IPv4 address to advertise; normally detected automatically")
    parser.add_argument("--http-port", type=int, default=49152, help="HTTP discovery/control port (default: 49152)")
    parser.add_argument(
        "--no-discovery", action="store_true", help="Join directly without opening an HTTP/SSDP listener"
    )
    parser.add_argument("--no-tui", action="store_true", help="Print status changes instead of the full-screen display")
    parser.add_argument("--duration", type=float, help="Stop and leave after this many seconds")
    parser.add_argument("--verbose", action="store_true", help="Enable diagnostic logging")
    return parser


def _local_address(target: str) -> str:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.connect((target, 9))  # Route lookup only; no datagram is transmitted.
        return str(sock.getsockname()[0])


async def _resolve_leader(host: str) -> str:
    from pywiim.client import WiiMClient

    client = WiiMClient(host, timeout=3, capabilities={"retry_count": 1})
    try:
        info = await client.get_device_info()
        if info.get("wmrm_version") != "4.3":
            raise ValueError(f"{host} does not report WMRM 4.3; only that version has been verified")
        if str(info.get("group")) == "1":
            leader = info.get("master_ip")
            if not leader:
                raise ValueError("This follower does not report its leader IP; pass the leader's address instead")
            return str(leader)
        return host
    finally:
        await client.close()


async def _display(receiver: Any, args: argparse.Namespace, screen: Any = None) -> None:
    start = time.monotonic()
    previous = None
    if screen is not None:
        screen.nodelay(True)
        with contextlib.suppress(Exception):
            import curses

            curses.curs_set(0)
    while not receiver.stop.is_set():
        state = receiver.state
        buffer = receiver.output.buffer
        rtt = (receiver.clock.rtt_ns or 0) / 1_000_000
        snapshot = (state.message, state.title, state.artist, state.volume, state.muted)
        if screen is not None:
            import curses

            rows, columns = screen.getmaxyx()
            lines = [
                f"  WiiM Mac Receiver  ·  {state.name}",
                "",
                f"  {state.message}",
                f"  Group: {state.master_name or state.master_host or 'none'}",
                f"  Track: {state.title or '—'}",
                f"  Artist: {state.artist or '—'}",
                "",
                f"  Volume: {state.volume}%{' (muted)' if state.muted else ''}",
                f"  Audio: AAC / {buffer.sample_rate if buffer else '—'} Hz / stereo",
                f"  Decoded: {receiver.output.decoded_packets} packets",
                f"  Clock RTT: {rtt:.2f} ms    Buffered: {buffer.buffered_ms if buffer else 0:.0f} ms",
                f"  Alignment trim: {args.latency_ms:+g} ms    Output underruns: {receiver.output.underruns}",
                "",
                "  + / - volume     m mute     q leave and quit",
            ]
            screen.erase()
            for index, line in enumerate(lines[:rows]):
                with contextlib.suppress(curses.error):
                    screen.addnstr(index, 0, line, max(0, columns - 1))
            screen.refresh()
            key = screen.getch()
            if key in (ord("q"), ord("Q")):
                receiver.stop.set()
            elif key in (ord("m"), ord("M")):
                receiver.set_mute(not state.muted)
            elif key in (ord("+"), ord("=")):
                receiver.set_volume(min(100, state.volume + 5))
            elif key == ord("-"):
                receiver.set_volume(max(0, state.volume - 5))
        elif snapshot != previous:
            track = f" · {state.artist} — {state.title}" if state.title else ""
            print(f"{state.name}: {state.message}{track} · volume {state.volume}%", flush=True)
            previous = snapshot
        if args.duration is not None and time.monotonic() - start >= args.duration:
            receiver.stop.set()
        await asyncio.sleep(0.1)


async def _run(args: argparse.Namespace, screen: Any = None) -> None:
    from pywiim.receiver.audio import AudioOutput
    from pywiim.receiver.client import Receiver
    from pywiim.receiver.server import DeviceServer

    output_device = int(args.output) if args.output and args.output.isdigit() else args.output
    output = AudioOutput(output_device, args.latency_ms)
    receiver = Receiver(args.name, device_uuid=args.uuid, volume=args.volume, output=output)
    leader = await _resolve_leader(args.speaker) if args.speaker else None
    receiver.select_master(leader)
    server = None
    tasks = []
    try:
        if not args.no_discovery:
            address = args.interface or _local_address(leader or "192.168.1.1")
            server = DeviceServer(receiver, address, args.http_port)
            await server.start()
        tasks = [asyncio.create_task(receiver.run()), asyncio.create_task(_display(receiver, args, screen))]
        done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            task.result()
        receiver.stop.set()
        await tasks[0]
    finally:
        receiver.stop.set()
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if server:
            await server.close()
        output.close()
        if screen is None:
            print(f"Left group; decoded {output.decoded_packets} AAC packets. Output underruns: {output.underruns}.")


def cli_main() -> None:
    """Console entry point with optional dependencies and readable startup errors."""
    parser = _parser()
    args = parser.parse_args()
    if not args.name or len(args.name.encode()) > 63 or not args.name.isprintable():
        parser.error("--name must be printable and contain 1..63 UTF-8 bytes")
    if not 0 <= args.volume <= 100:
        parser.error("--volume must be between 0 and 100")
    if not 1 <= args.http_port <= 65535:
        parser.error("--http-port must be between 1 and 65535")
    if args.duration is not None and args.duration <= 0:
        parser.error("--duration must be positive")
    if not -5000 <= args.latency_ms <= 5000:
        parser.error("--latency-ms must be between -5000 and 5000")
    if args.uuid and (len(args.uuid) != 24 or any(c not in "0123456789abcdefABCDEF" for c in args.uuid)):
        parser.error("--uuid must contain exactly 24 hexadecimal characters")
    if args.uuid:
        args.uuid = args.uuid.upper()
    if args.no_discovery and not args.speaker and not args.list_outputs:
        parser.error("provide a speaker IP when using --no-discovery")
    try:
        import sounddevice as sd

        from pywiim.receiver.client import Receiver  # noqa: F401
    except ImportError:
        parser.error('Receiver dependencies are missing. Install with: pip install -e ".[receiver]"')
    if args.list_outputs:
        for index, device in enumerate(sd.query_devices()):
            if device["max_output_channels"]:
                print(f"{index}: {device['name']} ({device['default_samplerate']:g} Hz)")
        return
    use_tui = not args.no_tui and not args.verbose and sys.stdin.isatty() and sys.stdout.isatty()
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.WARNING,
        filename=None if not use_tui else "/dev/null",
        format="%(levelname)s: %(message)s",
    )
    try:
        if use_tui:
            import curses

            curses.wrapper(lambda screen: asyncio.run(_run(args, screen)))
        else:
            asyncio.run(_run(args))
    except KeyboardInterrupt:
        pass
    except (OSError, ValueError, sd.PortAudioError) as exc:
        parser.exit(1, f"wiim-receiver: {exc}\n")


if __name__ == "__main__":
    cli_main()
