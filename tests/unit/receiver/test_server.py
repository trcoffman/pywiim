"""Exercise the HTTP commands and discovery response without a physical speaker."""

import asyncio
import errno
import socket
import sys
from unittest.mock import MagicMock
from xml.etree import ElementTree

import pytest
from aiohttp.test_utils import TestClient, TestServer

pytest.importorskip("Crypto")
pytest.importorskip("av")
pytest.importorskip("numpy")

from pywiim.receiver.client import Receiver  # noqa: E402
from pywiim.receiver.protocol import ControlMessage  # noqa: E402
from pywiim.receiver.server import DeviceServer, _Discovery  # noqa: E402


def make_receiver(name="Mac"):
    output = MagicMock()
    output.buffer = None
    return Receiver(name, device_uuid="000011112222333344445555", output=output)


@pytest.fixture
async def device_server():
    receiver = make_receiver()
    server = DeviceServer(receiver, "192.168.1.2", 8080)
    async with TestClient(TestServer(server.application())) as client:
        yield receiver, client


async def test_join_is_pending_until_leader_confirms_membership(device_server):
    receiver, client = device_server
    response = await client.get(
        "/httpapi.asp", params={"command": "ConnectMasterAp:JoinGroupMaster:eth192.168.1.99:wifi0.0.0.0"}
    )
    assert await response.text() == "OK"
    assert receiver.state.master_host == "192.168.1.99"
    status = await (await client.get("/httpapi.asp", params={"command": "getStatusEx"})).json()
    assert status["group"] == "0"
    receiver._handle_control(
        ControlMessage(
            "_linkplay_mrm", 252, {"cmd": "group_info", "body": '{"list":[{"uuid":"000011112222333344445555"}]}'}
        )
    )
    status = await (await client.get("/httpapi.asp", params={"command": "getStatusEx"})).json()
    assert status["group"] == "1"
    assert status["master_ip"] == "192.168.1.99"
    await client.get("/httpapi.asp", params={"command": "multiroom:Ungroup"})
    assert receiver.state.master_host is None


async def test_receiver_only_volume_and_validation(device_server):
    receiver, client = device_server
    for command, status in (("setPlayerCmd:vol:75", 200), ("setPlayerCmd:vol:101", 400), ("setPlayerCmd:vol:nan", 400)):
        response = await client.get("/httpapi.asp", params={"command": command})
        assert response.status == status
    assert receiver.state.volume == 75
    await client.get("/httpapi.asp", params={"command": "setPlayerCmd:mute:1"})
    assert receiver.state.muted
    response = await client.get("/httpapi.asp", params={"command": "reboot"})
    assert await response.text() == "unknown command"


async def test_description_xml_escapes_names():
    receiver = make_receiver("Mac <Friends & Family>")
    server = DeviceServer(receiver, "192.168.1.2", 8080)
    async with TestClient(TestServer(server.application())) as client:
        root = ElementTree.fromstring(await (await client.get("/description.xml")).read())
        assert root.find(".//{*}friendlyName").text == receiver.state.name


def test_spotify_source_gate_and_metadata():
    receiver = make_receiver()
    receiver.output.buffer = MagicMock()
    receiver.state.playing = True
    receiver._handle_control(ControlMessage("_linkplay_mrm", 253, {"cmd": "metaData", "playmode": 31}))
    assert receiver.output.buffer.enabled is True
    receiver._handle_control(ControlMessage("_linkplay_mrm", 253, {"cmd": "metaData", "playmode": 1}))
    assert receiver.output.buffer.enabled is False
    receiver.output.reset.assert_called()


def test_ssdp_media_renderer_search_and_no_response_to_other_targets():
    discovery = _Discovery("http://192.168.1.2/description.xml", "uuid:abcd")
    discovery.transport = MagicMock()
    discovery.datagram_received(
        b'M-SEARCH * HTTP/1.1\r\nMAN: "ssdp:discover"\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n',
        ("192.168.1.3", 1234),
    )
    packet, addr = discovery.transport.sendto.call_args.args
    assert b"LOCATION: http://192.168.1.2/description.xml" in packet
    assert b"USN: uuid:abcd::urn:schemas-upnp-org:device:MediaRenderer:1" in packet
    assert addr == ("192.168.1.3", 1234)
    discovery.transport.reset_mock()
    discovery.datagram_received(b"M-SEARCH * HTTP/1.1\r\nST: unrelated\r\n\r\n", ("192.168.1.3", 1234))
    discovery.transport.sendto.assert_not_called()


@pytest.mark.skipif(sys.platform != "darwin", reason="macOS multicast port sharing")
async def test_discovery_receives_searches_when_another_app_shares_its_port(monkeypatch):
    group = "239.255.255.250"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as other_app:
        other_app.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        other_app.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        other_app.bind(("", 0))
        address = (group, other_app.getsockname()[1])
        membership = socket.inet_aton(group) + socket.inet_aton("127.0.0.1")
        other_app.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
        monkeypatch.setattr("pywiim.receiver.server._SSDP", address)
        server = DeviceServer(make_receiver(), "127.0.0.1", 0)
        try:
            await server.start()
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as search:
                search.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton("127.0.0.1"))
                search.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
                search.bind(("127.0.0.1", 0))
                search.setblocking(False)
                loop = asyncio.get_running_loop()
                await loop.sock_sendto(
                    search,
                    b'M-SEARCH * HTTP/1.1\r\nMAN: "ssdp:discover"\r\n'
                    b"ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n",
                    address,
                )
                packet, _ = await asyncio.wait_for(loop.sock_recvfrom(search, 8192), 3)
                assert packet.startswith(b"HTTP/1.1 200 OK\r\n")
                assert server.udn.encode() in packet
        finally:
            await server.close()


@pytest.mark.parametrize("listener", ["HTTP", "SSDP"])
async def test_occupied_port_identifies_listener_and_cleans_up(monkeypatch, listener):
    kind = socket.SOCK_STREAM if listener == "HTTP" else socket.SOCK_DGRAM
    with socket.socket(socket.AF_INET, kind) as blocker:
        blocker.bind(("127.0.0.1" if listener == "HTTP" else "", 0))
        port = blocker.getsockname()[1]
        if listener == "HTTP":
            blocker.listen()
        else:
            monkeypatch.setattr("pywiim.receiver.server._SSDP", ("239.255.255.250", port))
        server = DeviceServer(make_receiver(), "127.0.0.1", port if listener == "HTTP" else 0)
        with pytest.raises(OSError, match=f"{listener}.*{port}") as error:
            await server.start()
        assert error.value.errno == errno.EADDRINUSE
        assert server._runner is None
