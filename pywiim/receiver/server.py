"""LAN discovery and the small HTTP control surface needed by a virtual WiiM."""

from __future__ import annotations

import asyncio
import contextlib
import ipaddress
import re
import socket
import sys
import uuid
from typing import Any
from xml.etree import ElementTree

from aiohttp import web

from .client import Receiver

_RENDERER = "urn:schemas-upnp-org:device:MediaRenderer:1"
_SSDP = ("239.255.255.250", 1900)


class _Discovery(asyncio.DatagramProtocol):
    def __init__(self, location: str, udn: str) -> None:
        self.location = location
        self.udn = udn
        self.transport: asyncio.DatagramTransport | None = None

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport  # type: ignore[assignment]

    def datagram_received(self, data: bytes, addr: tuple[str, int]) -> None:
        if len(data) > 8192:
            return
        lines = data.decode("ascii", errors="ignore").split("\r\n")
        if not lines or lines[0].upper() != "M-SEARCH * HTTP/1.1":
            return
        headers = dict(line.lower().split(":", 1) for line in lines[1:] if ":" in line)
        target = headers.get("st", "").strip()
        if headers.get("man", "").strip() != '"ssdp:discover"':
            return
        for kind in ("upnp:rootdevice", self.udn, _RENDERER):
            if target in (kind.lower(), "ssdp:all"):
                usn = self.udn if kind == self.udn else f"{self.udn}::{kind}"
                response = (
                    "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=180\r\nEXT:\r\n"
                    f"LOCATION: {self.location}\r\nSERVER: Darwin UPnP/1.0 WiiM/pywiim\r\n"
                    f"ST: {kind}\r\nUSN: {usn}\r\n\r\n"
                )
                if self.transport:
                    self.transport.sendto(response.encode("ascii"), addr)

    def announce(self, alive: bool) -> None:
        for kind in ("upnp:rootdevice", self.udn, _RENDERER):
            usn = self.udn if kind == self.udn else f"{self.udn}::{kind}"
            packet = (
                f"NOTIFY * HTTP/1.1\r\nHOST: {_SSDP[0]}:{_SSDP[1]}\r\n"
                f"NT: {kind}\r\nNTS: ssdp:{'alive' if alive else 'byebye'}\r\nUSN: {usn}\r\n"
            )
            if alive:
                packet += (
                    f"LOCATION: {self.location}\r\nCACHE-CONTROL: max-age=180\r\n"
                    "SERVER: Darwin UPnP/1.0 WiiM/pywiim\r\n"
                )
            if self.transport:
                self.transport.sendto((packet + "\r\n").encode("ascii"), _SSDP)


class DeviceServer:
    """Advertise discovery and accept native group/volume HTTP commands.

    The service intentionally exposes no DLNA playback services: audio comes
    from the registered WMRM group connection, not SetAVTransportURI.
    """

    def __init__(self, receiver: Receiver, address: str, port: int = 49152) -> None:
        self.receiver = receiver
        self.address = str(ipaddress.IPv4Address(address))
        self.port = port
        self.udn = f"uuid:{uuid.uuid5(uuid.NAMESPACE_DNS, receiver.state.uuid)}"
        self._runner: web.AppRunner | None = None
        self._discovery: _Discovery | None = None
        self._announcer: asyncio.Task | None = None

    def application(self) -> web.Application:
        """Construct the HTTP application, also usable for local protocol tests."""
        app = web.Application(client_max_size=8192)
        app.router.add_get("/description.xml", self._description)
        app.router.add_get("/httpapi.asp", self._command)
        return app

    async def start(self) -> None:
        self._runner = web.AppRunner(self.application(), access_log=None)
        await self._runner.setup()
        try:
            site = web.TCPSite(self._runner, self.address, self.port)
            try:
                await site.start()
            except OSError as exc:
                raise OSError(
                    exc.errno,
                    f"HTTP listener at {self.address}:{self.port} could not start: {exc.strerror or exc}. "
                    "Choose another port with --http-port.",
                ) from exc
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
            try:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                if sys.platform == "darwin":
                    # macOS requires this as well to share a multicast port.
                    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
                sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(self.address))
                sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
                try:
                    sock.bind(("", _SSDP[1]))
                except OSError as exc:
                    raise OSError(
                        exc.errno,
                        f"SSDP discovery listener on UDP port {_SSDP[1]} could not bind: {exc.strerror or exc}. "
                        "Quit the conflicting discovery app, or supply a speaker IP with --no-discovery.",
                    ) from exc
                membership = socket.inet_aton(_SSDP[0]) + socket.inet_aton(self.address)
                sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
                sock.setblocking(False)
                discovery = _Discovery(f"http://{self.address}:{self.port}/description.xml", self.udn)
                self._discovery = discovery
                await asyncio.get_running_loop().create_datagram_endpoint(lambda: discovery, sock=sock)
            except BaseException:
                sock.close()
                raise
            self._announcer = asyncio.create_task(self._announce())
        except BaseException:
            await self.close()
            raise

    async def _announce(self) -> None:
        while self._discovery:
            self._discovery.announce(True)
            await asyncio.sleep(60)

    async def close(self) -> None:
        """Withdraw only this virtual speaker's discovery records and HTTP listener."""
        if self._announcer:
            self._announcer.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._announcer
            self._announcer = None
        if self._discovery:
            self._discovery.announce(False)
            if self._discovery.transport:
                self._discovery.transport.close()
            self._discovery = None
        if self._runner:
            await self._runner.cleanup()
            self._runner = None

    async def _description(self, request: web.Request) -> web.Response:
        root = ElementTree.Element("root", xmlns="urn:schemas-upnp-org:device-1-0")
        version = ElementTree.SubElement(root, "specVersion")
        ElementTree.SubElement(version, "major").text = "1"
        ElementTree.SubElement(version, "minor").text = "0"
        device = ElementTree.SubElement(root, "device")
        for name, value in {
            "deviceType": _RENDERER,
            "friendlyName": self.receiver.state.name,
            "manufacturer": "pywiim",
            "modelName": "Mac WMRM Receiver",
            "modelDescription": "Experimental Spotify group speaker",
            "UDN": self.udn,
        }.items():
            ElementTree.SubElement(device, name).text = value
        return web.Response(
            body=ElementTree.tostring(root, encoding="utf-8", xml_declaration=True), content_type="text/xml"
        )

    def device_status(self) -> dict[str, Any]:
        state = self.receiver.state
        result: dict[str, Any] = {
            "DeviceName": state.name,
            "GroupName": state.name,
            "uuid": state.uuid,
            "upnp_uuid": self.udn,
            "firmware": "pywiim.receiver.0.1",
            "project": "WiiM_Mac_Virtual",
            "hardware": "macOS",
            "wmrm_version": "4.3",
            "group": "1" if state.connected else "0",
            "apcli0": self.address,
            "eth2": "0.0.0.0",
            "internet": "1",
            "netstat": "2",
            "spotify_active": "0",  # Spotify Connect is supplied by the group leader.
            "communication_port": "0",
            "uart_pass_port": "0",
        }
        if state.connected:
            result.update(master_ip=state.master_host, master_uuid=state.master_uuid)
        return result

    async def _command(self, request: web.Request) -> web.Response:
        command = request.query.get("command", "")
        state = self.receiver.state
        if command in ("getStatusEx", "getStatus"):
            return web.json_response(self.device_status())
        if command in ("getPlayerStatus", "getPlayerStatusEx", "getPlayerStatusEx2"):
            return web.json_response(
                {
                    "type": "1" if state.connected else "0",
                    "mode": "99" if state.connected else "0",
                    "status": "play" if state.playing and state.source == 31 else "stop",
                    "vol": str(state.volume),
                    "mute": str(int(state.muted)),
                    "ch": str(state.channel),
                    "Title": state.title.encode().hex(),
                    "Artist": state.artist.encode().hex(),
                    "Album": state.album.encode().hex(),
                }
            )
        if command == "multiroom:getSlaveList":
            return web.json_response({"slaves": 0, "wmrm_version": "4.3"})
        if command == "multiroom:Ungroup":
            self.receiver.select_master(None)
        elif match := re.fullmatch(r"ConnectMasterAp:JoinGroupMaster:eth([^:]+):wifi([^:]+)", command):
            try:
                eth, wifi = (ipaddress.IPv4Address(value) for value in match.groups())
                leader = str(eth if not eth.is_unspecified else wifi)
                if leader == "0.0.0.0" or not ipaddress.IPv4Address(leader).is_private or leader == self.address:
                    raise ValueError("invalid group leader")
                self.receiver.select_master(leader)
            except ValueError:
                raise web.HTTPBadRequest(text="Invalid group leader IPv4 address") from None
        elif command.startswith("setPlayerCmd:vol:"):
            try:
                self.receiver.set_volume(int(command.rsplit(":", 1)[1]))
            except ValueError:
                raise web.HTTPBadRequest(text="Volume must be an integer between 0 and 100") from None
        elif command in ("setPlayerCmd:mute:0", "setPlayerCmd:mute:1"):
            self.receiver.set_mute(command.endswith(":1"))
        else:
            return web.Response(text="unknown command")
        return web.Response(text="OK")
