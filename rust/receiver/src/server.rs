use crate::{client::Shared, Args};
use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

#[derive(Clone)]
struct Advertised {
    shared: Shared,
    address: Ipv4Addr,
    udn: String,
    location: String,
}

pub struct DeviceServer {
    stop: CancellationToken,
    http: JoinHandle<std::io::Result<()>>,
    discovery: JoinHandle<std::io::Result<()>>,
}
impl DeviceServer {
    pub async fn start(shared: Shared, args: &Args) -> Result<Self> {
        let address = match args.interface {
            Some(address) => address,
            None => {
                let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
                let leader = shared
                    .state
                    .lock()
                    .unwrap()
                    .master_host
                    .unwrap_or(Ipv4Addr::new(192, 168, 1, 1));
                socket.connect((leader, 9))?;
                match socket.local_addr()?.ip() {
                    std::net::IpAddr::V4(address) => address,
                    _ => unreachable!(),
                }
            }
        };
        let listener = TcpListener::bind((address, args.http_port))
            .await
            .with_context(|| {
                format!(
                    "HTTP listener at {address}:{} could not start; use another --http-port",
                    args.http_port
                )
            })?;
        let port = listener.local_addr()?.port();
        let udp=Arc::new(ssdp_socket(address,1900).context("SSDP discovery could not bind UDP port 1900; quit the conflicting app or supply a speaker IP with --no-discovery")?);
        let uuid = shared.state.lock().unwrap().uuid.clone();
        let info = Advertised {
            shared: shared.clone(),
            address,
            udn: format!(
                "uuid:{}",
                uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, uuid.as_bytes())
            ),
            location: format!("http://{address}:{port}/description.xml"),
        };
        let router = Router::new()
            .route("/description.xml", get(description))
            .route("/httpapi.asp", get(command))
            .with_state(info.clone());
        let stop = shared.stop.child_token();
        let http_stop = stop.clone();
        let http = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(http_stop.cancelled_owned())
                .await
        });
        let discovery_stop = stop.clone();
        let discovery =
            tokio::spawn(async move { discovery_loop(udp, info, discovery_stop).await });
        Ok(Self {
            stop,
            http,
            discovery,
        })
    }
    pub async fn close(&mut self) {
        self.stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut self.http).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut self.discovery).await;
    }
}
impl Drop for DeviceServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn ssdp_socket(interface: Ipv4Addr, port: u16) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "macos")]
    socket.set_reuse_port(true)?;
    socket.set_multicast_if_v4(&interface)?;
    socket.set_multicast_ttl_v4(2)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    socket.join_multicast_v4(&GROUP, &interface)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

fn records(info: &Advertised) -> [String; 3] {
    ["upnp:rootdevice".into(), info.udn.clone(), RENDERER.into()]
}
fn usn(info: &Advertised, kind: &str) -> String {
    if kind == info.udn {
        info.udn.clone()
    } else {
        format!("{}::{kind}", info.udn)
    }
}
fn responses(info: &Advertised, bytes: &[u8]) -> Vec<String> {
    if bytes.len() > 8192 {
        return vec![];
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return vec![];
    };
    let mut lines = text.split("\r\n");
    if !lines
        .next()
        .is_some_and(|line| line.eq_ignore_ascii_case("M-SEARCH * HTTP/1.1"))
    {
        return vec![];
    }
    let headers: HashMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    if headers.get("man") != Some(&"\"ssdp:discover\"") {
        return vec![];
    }
    let target = headers.get("st").copied().unwrap_or("");
    records(info).into_iter().filter(|kind|target.eq_ignore_ascii_case(kind)||target.eq_ignore_ascii_case("ssdp:all"))
        .map(|kind|format!("HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=180\r\nEXT:\r\nLOCATION: {}\r\nSERVER: Darwin UPnP/1.0 WiiM/rust\r\nST: {kind}\r\nUSN: {}\r\n\r\n",info.location,usn(info,&kind))).collect()
}
async fn announce(socket: &UdpSocket, info: &Advertised, alive: bool) {
    for kind in records(info) {
        let mut text = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {GROUP}:1900\r\nNT: {kind}\r\nNTS: ssdp:{}\r\nUSN: {}\r\n",
            if alive { "alive" } else { "byebye" },
            usn(info, &kind)
        );
        if alive {
            text.push_str(&format!("LOCATION: {}\r\nCACHE-CONTROL: max-age=180\r\nSERVER: Darwin UPnP/1.0 WiiM/rust\r\n",info.location));
        }
        text.push_str("\r\n");
        let _ = socket.send_to(text.as_bytes(), (GROUP, 1900)).await;
    }
}
async fn discovery_loop(
    socket: Arc<UdpSocket>,
    info: Advertised,
    stop: CancellationToken,
) -> std::io::Result<()> {
    let mut buffer = [0; 8193];
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _=stop.cancelled()=>{announce(&socket,&info,false).await;return Ok(());}
            _=tick.tick()=>announce(&socket,&info,true).await,
            packet=socket.recv_from(&mut buffer)=>{let (len,peer)=packet?;for response in responses(&info,&buffer[..len]){socket.send_to(response.as_bytes(),peer).await?;}}
        }
    }
}

async fn description(State(info): State<Advertised>) -> impl IntoResponse {
    let name = info.shared.state.lock().unwrap().name.clone();
    let name = quick_xml::escape::escape(&name);
    let xml=format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><device><deviceType>{RENDERER}</deviceType><friendlyName>{name}</friendlyName><manufacturer>pywiim</manufacturer><modelName>Mac WMRM Receiver</modelName><modelDescription>Spotify group speaker</modelDescription><UDN>{}</UDN></device></root>",info.udn);
    ([(header::CONTENT_TYPE, "text/xml; charset=utf-8")], xml)
}

async fn command(
    State(info): State<Advertised>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let command = query.get("command").map(String::as_str).unwrap_or("");
    match command {
        "getStatus" | "getStatusEx" => {
            let s = info.shared.state.lock().unwrap();
            let mut result = json!({"DeviceName":s.name,"GroupName":s.name,"uuid":s.uuid,"upnp_uuid":info.udn,
                "firmware":"wiim-receiver.rust.0.1","project":"WiiM_Mac_Virtual","hardware":"macOS","wmrm_version":"4.3",
                "group":if s.connected{"1"}else{"0"},"apcli0":info.address.to_string(),"eth2":"0.0.0.0","internet":"1","netstat":"2",
                "spotify_active":"0","communication_port":"0","uart_pass_port":"0"});
            if s.connected {
                result["master_ip"] = json!(s.master_host);
                result["master_uuid"] = json!(s.master_uuid);
            }
            Json(result).into_response()
        }
        "getPlayerStatus" | "getPlayerStatusEx" | "getPlayerStatusEx2" => {
            let s = info.shared.state.lock().unwrap();
            Json(json!({"type":if s.connected{"1"}else{"0"},"mode":if s.connected{"99"}else{"0"},
                "status":if s.playing&&s.source==Some(31){"play"}else{"stop"},"vol":s.volume.to_string(),"mute":u8::from(s.muted).to_string(),
                "ch":s.channel.to_string(),"Title":hex::encode(&s.title),"Artist":hex::encode(&s.artist),"Album":hex::encode(&s.album)})).into_response()
        }
        "multiroom:getSlaveList" => Json(json!({"slaves":0,"wmrm_version":"4.3"})).into_response(),
        "multiroom:Ungroup" => {
            info.shared.select_master(None);
            "OK".into_response()
        }
        "setPlayerCmd:mute:0" | "setPlayerCmd:mute:1" => {
            info.shared.set_mute(command.ends_with('1'));
            "OK".into_response()
        }
        _ if command.starts_with("setPlayerCmd:vol:") => {
            match command
                .trim_start_matches("setPlayerCmd:vol:")
                .parse::<u8>()
            {
                Ok(volume) if volume <= 100 => {
                    info.shared.set_volume(volume);
                    "OK".into_response()
                }
                _ => (
                    StatusCode::BAD_REQUEST,
                    "Volume must be an integer between 0 and 100",
                )
                    .into_response(),
            }
        }
        _ if command.starts_with("ConnectMasterAp:JoinGroupMaster:eth") => {
            let leader = command
                .strip_prefix("ConnectMasterAp:JoinGroupMaster:eth")
                .and_then(|tail| tail.split_once(":wifi"))
                .and_then(|(eth, wifi)| {
                    Some((
                        eth.parse::<Ipv4Addr>().ok()?,
                        wifi.parse::<Ipv4Addr>().ok()?,
                    ))
                })
                .map(|(eth, wifi)| if eth.is_unspecified() { wifi } else { eth })
                .filter(|ip| (ip.is_private() || ip.is_link_local()) && *ip != info.address);
            match leader {
                Some(leader) => {
                    info.shared.select_master(Some(leader));
                    "OK".into_response()
                }
                None => {
                    (StatusCode::BAD_REQUEST, "Invalid group leader IPv4 address").into_response()
                }
            }
        }
        _ => "unknown command".into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info() -> Advertised {
        Advertised {
            shared: Shared::new(
                "Mac <test>".into(),
                "000011112222333344445555".into(),
                30,
                None,
            ),
            address: Ipv4Addr::LOCALHOST,
            udn: "uuid:abcd".into(),
            location: "http://127.0.0.1:8088/description.xml".into(),
        }
    }
    #[test]
    fn discoverable_as_renderer_and_rejects_unrelated_searches() {
        let info = info();
        let packets=responses(&info,b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n");
        assert_eq!(packets.len(), 1);
        assert!(packets[0].contains(&info.location));
        assert!(packets[0].contains("uuid:abcd::urn:"));
        assert!(responses(&info, b"M-SEARCH * HTTP/1.1\r\nST: unrelated\r\n\r\n").is_empty());
    }
    #[tokio::test]
    async fn http_grouping_volume_and_validation() {
        let info = info();
        for (cmd, status) in [
            ("setPlayerCmd:vol:75", 200),
            ("setPlayerCmd:vol:101", 400),
            (
                "ConnectMasterAp:JoinGroupMaster:eth192.168.1.99:wifi0.0.0.0",
                200,
            ),
        ] {
            let response = command(
                State(info.clone()),
                Query(HashMap::from([("command".into(), cmd.into())])),
            )
            .await;
            assert_eq!(response.status().as_u16(), status);
        }
        let state = info.shared.state.lock().unwrap();
        assert_eq!(state.volume, 75);
        assert_eq!(state.master_host, Some(Ipv4Addr::new(192, 168, 1, 99)));
        assert!(!state.connected);
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn discovery_can_share_an_existing_multicast_listener() {
        let first = ssdp_socket(Ipv4Addr::LOCALHOST, 0).unwrap();
        let port = first.local_addr().unwrap().port();
        let second = ssdp_socket(Ipv4Addr::LOCALHOST, port).unwrap();
        let sender = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sender.set_multicast_loop_v4(true).unwrap();
        let sender = Socket::from(sender);
        sender.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
        sender
            .send_to(
                b"discovery sharing",
                &std::net::SocketAddr::from((GROUP, port)).into(),
            )
            .unwrap();
        let mut buffer = [0; 64];
        let received = tokio::time::timeout(Duration::from_secs(2), second.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..received.0], b"discovery sharing");
    }
}
