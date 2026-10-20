use crate::{
    audio::{AudioControls, EncodedAudio},
    protocol::{self, AudioDecoder, Control, ControlDecoder, LeaderClock},
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct State {
    pub name: String,
    pub uuid: String,
    pub master_host: Option<Ipv4Addr>,
    pub master_name: String,
    pub master_uuid: String,
    pub connected: bool,
    pub playing: bool,
    pub playback_revision: u64,
    pub source: Option<i64>,
    pub volume: u8,
    pub muted: bool,
    pub channel: u8,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub leadtime: u32,
    pub message: String,
}

pub struct ClockData {
    origin: Instant,
    pub ready: AtomicBool,
    pub offset: AtomicI64,
    pub rtt: AtomicU64,
}
impl ClockData {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            ready: AtomicBool::new(false),
            offset: AtomicI64::new(0),
            rtt: AtomicU64::new(0),
        }
    }
    pub fn now(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }
    pub fn rtt_ms(&self) -> f64 {
        self.rtt.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

#[derive(Clone)]
pub struct Shared {
    pub state: Arc<Mutex<State>>,
    pub audio: Arc<AudioControls>,
    pub clock: Arc<ClockData>,
    pub leader: watch::Sender<Option<Ipv4Addr>>,
    pub stop: CancellationToken,
    pub output_name: String,
}
impl Shared {
    pub fn new(name: String, uuid: String, volume: u8, master_host: Option<Ipv4Addr>) -> Self {
        let (leader, _) = watch::channel(master_host);
        let audio = Arc::new(AudioControls::new(volume));
        Self {
            state: Arc::new(Mutex::new(State {
                name,
                uuid,
                master_host,
                master_name: String::new(),
                master_uuid: String::new(),
                connected: false,
                playing: false,
                playback_revision: 0,
                source: None,
                volume,
                muted: false,
                channel: 0,
                title: String::new(),
                artist: String::new(),
                album: String::new(),
                leadtime: 800,
                message: "Waiting to join a WiiM group".into(),
            })),
            audio,
            clock: Arc::new(ClockData::new()),
            leader,
            stop: CancellationToken::new(),
            output_name: String::new(),
        }
    }
    pub fn select_master(&self, host: Option<Ipv4Addr>) {
        self.state.lock().unwrap().master_host = host;
        self.leader.send_if_modified(|old| {
            if *old == host {
                false
            } else {
                *old = host;
                true
            }
        });
    }
    pub fn set_volume(&self, volume: u8) {
        self.state.lock().unwrap().volume = volume;
        self.audio.volume.store(volume.min(100), Ordering::Relaxed);
    }
    pub fn set_mute(&self, muted: bool) {
        self.state.lock().unwrap().muted = muted;
        self.audio.muted.store(muted, Ordering::Relaxed);
    }
    pub fn reset_audio(&self) {
        self.audio.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn message(&self, message: impl Into<String>) {
        self.state.lock().unwrap().message = message.into();
    }
    fn sync_output(&self) {
        let s = self.state.lock().unwrap();
        self.audio
            .enabled
            .store(s.playing && s.source == Some(31), Ordering::Release);
        self.audio.channel.store(s.channel, Ordering::Relaxed);
    }
}

pub async fn resolve_leader(host: Ipv4Addr) -> Result<Ipv4Addr> {
    // WiiM devices may serve their local API using a self-signed HTTPS certificate.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()?;
    let mut failure = String::new();
    for scheme in ["https", "http"] {
        let url = format!("{scheme}://{host}/httpapi.asp?command=getStatusEx");
        match client.get(url).send().await {
            Ok(response) => match response.error_for_status()?.json::<Value>().await {
                Ok(info) => {
                    ensure!(
                        info["wmrm_version"].as_str() == Some("4.3"),
                        "{host} does not report WMRM 4.3"
                    );
                    if info["group"].as_str() == Some("1") || info["group"].as_i64() == Some(1) {
                        return info["master_ip"]
                            .as_str()
                            .context("follower did not report master_ip; pass the leader IP")?
                            .parse()
                            .context("invalid leader IPv4 address");
                    }
                    return Ok(host);
                }
                Err(e) => failure = e.to_string(),
            },
            Err(e) => failure = e.to_string(),
        }
    }
    bail!("could not read WiiM status from {host}: {failure}")
}

async fn connect(host: Ipv4Addr, port: u16) -> Result<TcpStream> {
    let service = match port {
        25012 => "Group control",
        24012 => "Group clock",
        26001 => "Group audio",
        _ => "TCP",
    };
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((host, port)))
        .await
        .with_context(|| format!("{service} connection to {host}:{port} timed out"))?
        .with_context(|| format!("{service} connection to {host}:{port} failed"))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}
async fn send(writer: &mut OwnedWriteHalf, value: Value, name: &str, category: u32) -> Result<()> {
    writer
        .write_all(&protocol::encode(value, name, category)?)
        .await?;
    Ok(())
}

pub async fn run(shared: Shared, audio: mpsc::Sender<EncodedAudio>) -> Result<()> {
    let mut leader = shared.leader.subscribe();
    let mut retry = 1;
    while !shared.stop.is_cancelled() {
        let host = *leader.borrow_and_update();
        let Some(host) = host else {
            shared.message("Waiting to join a WiiM group");
            tokio::select! {_=shared.stop.cancelled()=>break,_=leader.changed()=>{}}
            continue;
        };
        match session(host, &shared, &audio, &mut leader).await {
            Ok(()) => retry = 1,
            Err(e) => {
                shared.message(format!("Disconnected: {e:#}. Retrying in {retry}s"));
                tokio::select! {_=shared.stop.cancelled()=>break,_=leader.changed()=>{},_=tokio::time::sleep(Duration::from_secs(retry))=>{}}
                retry = (retry * 2).min(8);
            }
        }
    }
    Ok(())
}

async fn session(
    host: Ipv4Addr,
    shared: &Shared,
    audio: &mpsc::Sender<EncodedAudio>,
    leader: &mut watch::Receiver<Option<Ipv4Addr>>,
) -> Result<()> {
    shared.clock.ready.store(false, Ordering::Release);
    shared.audio.enabled.store(false, Ordering::Release);
    shared.reset_audio();
    shared.audio.ready.store(false, Ordering::Release);
    {
        let mut s = shared.state.lock().unwrap();
        s.playing = false;
        s.source = None;
        s.message = format!("Joining {host}");
    }
    let stream = connect(host, 25012).await?;
    let (reader, mut writer) = stream.into_split();
    let login = {
        let s = shared.state.lock().unwrap();
        json!({"cmd":"login","name":s.name,"uuid":s.uuid,"wmrm_version":"4.3","type":"WiiMu-Mac",
        "fw_version":"wiim-receiver.rust.0.1","has_new_version":0,"channel":s.channel,"volume":s.volume,"mute":u8::from(s.muted),
        "battery_percent":0,"battery_charging":0,"mask":0,"BPS":0,"flags":175,"sub":0,"sub_master_uuid":""})
    };
    send(&mut writer, login, "_linkplay_mrm", 253).await?;
    let result = tokio::select! {
        r=control_loop(reader,&mut writer,shared)=>r,
        r=clock_loop(host,shared)=>r,
        r=audio_loop(host,26001,shared,audio)=>r,
        r=playback_status_loop(host,shared)=>r,
        _=leader.changed()=>Ok(()),
        _=shared.stop.cancelled()=>Ok(()),
    };
    shared.audio.enabled.store(false, Ordering::Release);
    shared.reset_audio();
    let logout = {
        let s = shared.state.lock().unwrap();
        json!({"cmd":"logout","name":s.name,"uuid":s.uuid})
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        send(&mut writer, logout, "_linkplay_mrm", 253),
    )
    .await;
    let _ = writer.shutdown().await;
    {
        let mut s = shared.state.lock().unwrap();
        s.connected = false;
        s.playing = false;
    }
    result
}

async fn control_loop(
    mut reader: OwnedReadHalf,
    writer: &mut OwnedWriteHalf,
    shared: &Shared,
) -> Result<()> {
    let mut decoder = ControlDecoder::default();
    let mut buffer = [0; 65536];
    let mut metadata = String::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(3));
    let start = Instant::now();
    loop {
        tokio::select! {
            count=reader.read(&mut buffer)=> {
                let count=count?;ensure!(count!=0,"leader closed the group control connection");
                for message in decoder.feed(&buffer[..count])? {handle_control(shared,message,&mut metadata)?;}
            }
            _=heartbeat.tick()=>{
                let (connected,playing)={let s=shared.state.lock().unwrap();(s.connected,s.playing)};
                ensure!(connected||start.elapsed()<Duration::from_secs(8),"leader did not confirm registration");
                if connected {
                    let leader_time=i128::from(shared.clock.now())+i128::from(shared.clock.offset.load(Ordering::Relaxed));
                    send(writer,json!({"status":u8::from(playing),"render_pts":shared.audio.rendered_position.load(Ordering::Relaxed),
                        "render_time":leader_time.max(0) as u64,"cache":0}),"_heart_tick",0).await?;
                }
            }
        }
    }
}

pub fn handle_control(shared: &Shared, message: Control, metadata: &mut String) -> Result<()> {
    if message.name == "_heart_tick" {
        return Ok(());
    }
    let v = message.value;
    let cmd = v["cmd"].as_str().unwrap_or("");
    match cmd {
        "update" => {
            let mut s = shared.state.lock().unwrap();
            s.master_name = text(&v["name"]);
            s.master_uuid = text(&v["uuid"]);
        }
        "group_info" => {
            let group: Value = serde_json::from_str(v["body"].as_str().unwrap_or("{}"))?;
            let mut s = shared.state.lock().unwrap();
            let found = group["list"]
                .as_array()
                .is_some_and(|nodes| nodes.iter().any(|n| n["uuid"].as_str() == Some(&s.uuid)));
            ensure!(found || !s.connected, "Mac was removed from the WiiM group");
            if found {
                s.connected = true;
                s.message = "Joined; waiting for Spotify".into();
            }
        }
        "playback_sync" => {
            let playing = number(&v["status"])? == 1;
            let lead = number(&v["leadtime"]).unwrap_or(800);
            ensure!((0..=10000).contains(&lead), "invalid playback lead time");
            {
                let mut s = shared.state.lock().unwrap();
                s.playing = playing;
                s.leadtime = lead as u32;
            }
            shared.audio.ready.store(true, Ordering::Release);
            if !playing {
                shared.reset_audio();
                shared.message("Joined; waiting for Spotify");
            }
        }
        "playback_start" | "playback_resume" => {
            shared.state.lock().unwrap().playing = true;
            shared.audio.ready.store(true, Ordering::Release);
        }
        "playback_stop" | "playback_pause" | "playback_flush" | "playback_prepare" => {
            shared.state.lock().unwrap().playing = false;
            shared.reset_audio();
            shared.message("Joined; waiting for Spotify");
        }
        "metaData" => {
            for field in ["metaDataSplit", "metaDataEnd"] {
                if let Some(part) = v[field].as_str() {
                    metadata.push_str(part);
                }
            }
            ensure!(metadata.len() <= 131072, "group metadata exceeds 128 KiB");
            if v.get("metaDataEnd").is_some() {
                parse_metadata(shared, metadata);
                metadata.clear();
            }
            if v.get("playmode").is_some() {
                let source = number(&v["playmode"])?;
                shared.state.lock().unwrap().source = Some(source);
                if source != 31 {
                    shared.message("Waiting for Spotify (other sources are muted)");
                    shared.reset_audio();
                }
            }
        }
        "control" => match v["operation"].as_str().unwrap_or("") {
            "volume" | "vol" => {
                let volume = number(&v["value"])?;
                ensure!((0..=100).contains(&volume), "invalid group volume");
                shared.set_volume(volume as u8);
            }
            "mute" => shared.set_mute(number(&v["value"])? == 1),
            "channel" => {
                let channel = number(&v["value"])?;
                ensure!((0..=2).contains(&channel), "invalid speaker channel");
                shared.state.lock().unwrap().channel = channel as u8;
            }
            _ => {}
        },
        _ => {}
    }
    if matches!(
        cmd,
        "playback_sync"
            | "playback_start"
            | "playback_resume"
            | "playback_stop"
            | "playback_pause"
            | "playback_flush"
            | "playback_prepare"
    ) || cmd == "metaData" && v.get("playmode").is_some()
    {
        let mut state = shared.state.lock().unwrap();
        state.playback_revision = state.playback_revision.wrapping_add(1);
    }
    shared.sync_output();
    Ok(())
}

fn text(v: &Value) -> String {
    v.as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}
fn number(v: &Value) -> Result<i64> {
    v.as_i64()
        .or_else(|| v.as_str()?.parse().ok())
        .ok_or_else(|| anyhow!("expected integer, got {v}"))
}
fn parse_metadata(shared: &Shared, metadata: &str) {
    use quick_xml::{events::Event, Reader};
    let mut reader = Reader::from_str(metadata);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let field = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if !matches!(field.as_str(), "title" | "artist" | "creator" | "album") {
                    continue;
                }
                if let Ok(decoded) = reader.read_text(e.name()) {
                    let unescaped = quick_xml::escape::unescape(&decoded)
                        .unwrap_or_else(|_| std::borrow::Cow::Borrowed(decoded.as_ref()));
                    let text = unescaped.chars().filter(|c| !c.is_control()).collect();
                    let mut s = shared.state.lock().unwrap();
                    match field.as_str() {
                        "title" => s.title = text,
                        "artist" | "creator" => s.artist = text,
                        "album" => s.album = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
}

async fn playback_status_loop(host: Ipv4Addr, shared: &Shared) -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let revision = {
            let state = shared.state.lock().unwrap();
            if !state.connected {
                continue;
            }
            state.playback_revision
        };
        for scheme in ["https", "http"] {
            let url = format!("{scheme}://{host}/httpapi.asp?command=getPlayerStatus");
            let status = async {
                client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Value>()
                    .await
            }
            .await;
            if let Ok(status) = status {
                // This read-only fallback repairs missed WMRM start/source
                // events. HTTP failures do not interrupt the control session.
                let _ = apply_playback_status(shared, &status, revision);
                break;
            }
        }
    }
}

fn apply_playback_status(shared: &Shared, status: &Value, revision: u64) -> Result<()> {
    let source = number(&status["mode"])?;
    let playing = match status["status"].as_str() {
        Some("play" | "playing") => true,
        Some("stop" | "pause" | "paused") => false,
        _ => return Ok(()),
    };
    let reset;
    {
        let mut state = shared.state.lock().unwrap();
        // A control update received during the request is newer than its HTTP
        // snapshot, and must not be overwritten by a delayed response.
        if !state.connected || state.playback_revision != revision {
            return Ok(());
        }
        reset = state.source != Some(source) || state.playing && !playing;
        let was_enabled = state.playing && state.source == Some(31);
        state.source = Some(source);
        state.playing = playing;
        let state = &mut *state;
        for (field, target) in [
            ("Title", &mut state.title),
            ("Artist", &mut state.artist),
            ("Album", &mut state.album),
        ] {
            if let Some(raw) = status[field].as_str() {
                let decoded = hex::decode(raw)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .unwrap_or_else(|| raw.into());
                *target = decoded.chars().filter(|c| !c.is_control()).collect();
            }
        }
        if playing && source == 31 {
            shared.audio.ready.store(true, Ordering::Release);
            if !was_enabled {
                state.message = "Spotify active; waiting for group audio".into();
            }
        } else {
            state.message = if playing {
                "Waiting for Spotify (other sources are muted)"
            } else {
                "Joined; waiting for Spotify"
            }
            .into();
        }
    }
    if reset {
        shared.reset_audio();
    }
    shared.sync_output();
    Ok(())
}

async fn clock_loop(host: Ipv4Addr, shared: &Shared) -> Result<()> {
    let mut stream = connect(host, 24012).await?;
    let mut clock = LeaderClock::default();
    let mut sequence = 0u32;
    loop {
        let sent = shared.clock.now();
        stream
            .write_all(&protocol::clock_request(sequence, sent))
            .await?;
        let mut response = [0; 32];
        tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut response)).await??;
        let (offset, rtt) = clock.observe(&response, sequence, sent, shared.clock.now())?;
        shared.clock.offset.store(offset, Ordering::Release);
        shared.clock.rtt.store(rtt, Ordering::Relaxed);
        shared.clock.ready.store(true, Ordering::Release);
        sequence = sequence.wrapping_add(1);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn audio_loop(
    host: Ipv4Addr,
    port: u16,
    shared: &Shared,
    audio: &mpsc::Sender<EncodedAudio>,
) -> Result<()> {
    loop {
        loop {
            let connected = shared.state.lock().unwrap().connected;
            if connected
                && shared.clock.ready.load(Ordering::Acquire)
                && shared.audio.ready.load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        match receive_audio(host, port, shared, audio).await {
            Ok(()) => audio_waiting_message(shared, host, port, None),
            Err(error) => {
                if error.downcast_ref::<std::io::Error>().is_none()
                    && error
                        .downcast_ref::<tokio::time::error::Elapsed>()
                        .is_none()
                {
                    return Err(error);
                }
                // The leader can close or refuse the audio transport while
                // idle or while Spotify is starting. Keep control membership,
                // heartbeats, and clock synchronization alive during retries.
                audio_waiting_message(shared, host, port, Some(&error));
            }
        }
        shared.reset_audio();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn audio_waiting_message(
    shared: &Shared,
    host: Ipv4Addr,
    port: u16,
    error: Option<&anyhow::Error>,
) {
    if shared.audio.enabled.load(Ordering::Acquire) {
        let detail = error
            .map(|error| format!(": {error:#}"))
            .unwrap_or_default();
        shared.message(format!("Waiting for group audio at {host}:{port}{detail}"));
    } else {
        shared.message("Joined; waiting for Spotify");
    }
}

async fn receive_audio(
    host: Ipv4Addr,
    port: u16,
    shared: &Shared,
    audio: &mpsc::Sender<EncodedAudio>,
) -> Result<()> {
    let mut stream = connect(host, port).await?;
    let mut decoder = AudioDecoder::default();
    let mut buffer = [0; 65536];
    loop {
        let n = stream
            .read(&mut buffer)
            .await
            .with_context(|| format!("Group audio read from {host}:{port} failed"))?;
        if n == 0 {
            return Ok(());
        }
        for packet in decoder.feed(&buffer[..n])? {
            if !shared.audio.enabled.load(Ordering::Acquire) {
                continue;
            }
            let presentation = packet.presentation_ns()?;
            let frame = EncodedAudio::Data {
                packet,
                presentation,
                epoch: shared.audio.epoch.load(Ordering::Acquire),
            };
            audio.send(frame).await.context("audio decoder stopped")?;
            shared.message("Playing Spotify");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn registered_receiver() -> Shared {
        let shared = Shared::new("Mac".into(), "000011112222333344445555".into(), 30, None);
        shared.state.lock().unwrap().connected = true;
        shared.clock.ready.store(true, Ordering::Relaxed);
        shared.audio.ready.store(true, Ordering::Relaxed);
        shared
    }

    #[test]
    fn leader_status_repairs_missing_start_events_and_respects_newer_control_updates() {
        let shared = registered_receiver();
        shared.audio.ready.store(false, Ordering::Relaxed);
        let status = json!({"mode":"31", "status":"play", "Title":hex::encode("Track"), "Artist":hex::encode("Artist")});
        apply_playback_status(&shared, &status, 0).unwrap();
        assert!(shared.audio.enabled.load(Ordering::Relaxed));
        assert!(shared.audio.ready.load(Ordering::Relaxed));
        assert_eq!(shared.state.lock().unwrap().title, "Track");
        let revision = shared.state.lock().unwrap().playback_revision;
        handle_control(
            &shared,
            message(json!({"cmd":"playback_pause"})),
            &mut String::new(),
        )
        .unwrap();
        apply_playback_status(&shared, &status, revision).unwrap();
        assert!(!shared.audio.enabled.load(Ordering::Relaxed));
        let revision = shared.state.lock().unwrap().playback_revision;
        apply_playback_status(&shared, &status, revision).unwrap();
        assert!(shared.audio.enabled.load(Ordering::Relaxed));
        apply_playback_status(&shared, &json!({"mode":"40", "status":"play"}), revision).unwrap();
        assert!(!shared.audio.enabled.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn audio_can_connect_before_spotify_state_arrives_without_playing_other_sources() {
        let shared = registered_receiver();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, mut receiver) = mpsc::channel(1);
        let worker = shared.clone();
        let task =
            tokio::spawn(
                async move { audio_loop(Ipv4Addr::LOCALHOST, port, &worker, &sender).await },
            );
        // Source/playback updates may arrive after the audio socket connects.
        // Gate PCM delivery, not the transport's ability to connect.
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(!task.is_finished());
        assert!(shared.state.lock().unwrap().connected);
        let frame = audio_frame();
        connection.write_all(&frame).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), receiver.recv())
                .await
                .is_err()
        );
        let mut metadata = String::new();
        handle_control(
            &shared,
            message(json!({"cmd":"metaData", "playmode":31})),
            &mut metadata,
        )
        .unwrap();
        handle_control(
            &shared,
            message(json!({"cmd":"playback_start"})),
            &mut metadata,
        )
        .unwrap();
        connection.write_all(&frame).await.unwrap();
        let decoded = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(decoded, EncodedAudio::Data { packet, .. } if packet.payload == b"AAC"));
        assert!(shared.state.lock().unwrap().connected);
        task.abort();
        let _ = task.await;
    }

    fn audio_frame() -> Vec<u8> {
        let mut frame = vec![0; protocol::AUDIO_HEADER_SIZE];
        frame[..4].copy_from_slice(&0xfedcba98u32.to_le_bytes());
        frame[4..6].copy_from_slice(&((protocol::AUDIO_HEADER_SIZE + 3) as u16).to_le_bytes());
        frame[6..8].copy_from_slice(&10u16.to_le_bytes());
        frame[8..12].copy_from_slice(&0x01ac4422u32.to_le_bytes());
        frame.extend_from_slice(b"AAC");
        frame
    }

    #[tokio::test]
    async fn refused_audio_connection_retries_without_losing_membership_and_recovers_after_eof() {
        let shared = registered_receiver();
        shared.audio.enabled.store(true, Ordering::Relaxed);
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let (sender, mut receiver) = mpsc::channel(1);
        let worker = shared.clone();
        let task =
            tokio::spawn(
                async move { audio_loop(Ipv4Addr::LOCALHOST, port, &worker, &sender).await },
            );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !shared.state.lock().unwrap().message.contains("failed") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        assert!(shared.state.lock().unwrap().connected);
        assert!(shared
            .state
            .lock()
            .unwrap()
            .message
            .contains(&format!("127.0.0.1:{port}")));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let frame = audio_frame();
        connection.write_all(&frame).await.unwrap();
        let decoded = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(decoded, EncodedAudio::Data { packet, .. } if packet.payload == b"AAC"));
        drop(connection);
        let (_connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(shared.state.lock().unwrap().connected);
        task.abort();
        let _ = task.await;
    }

    fn message(value: Value) -> Control {
        Control {
            name: "_linkplay_mrm".into(),
            value,
        }
    }
    #[test]
    fn source_gating_pause_resume_and_metadata_preserve_control_behavior() {
        let shared = Shared::new("Mac".into(), "000011112222333344445555".into(), 30, None);
        let mut metadata = String::new();
        handle_control(
            &shared,
            message(json!({"cmd":"playback_sync","status":1,"leadtime":800})),
            &mut metadata,
        )
        .unwrap();
        assert!(!shared.audio.enabled.load(Ordering::Relaxed));
        handle_control(&shared, message(json!({"cmd":"metaData","playmode":31,"metaDataSplit":"<DIDL-Lite><item><title>A &amp;"})), &mut metadata).unwrap();
        handle_control(&shared, message(json!({"cmd":"metaData","metaDataEnd":" B</title><artist>Artist</artist></item></DIDL-Lite>"})), &mut metadata).unwrap();
        assert!(shared.audio.enabled.load(Ordering::Relaxed));
        assert_eq!(shared.state.lock().unwrap().title, "A & B");
        let epoch = shared.audio.epoch.load(Ordering::Relaxed);
        handle_control(
            &shared,
            message(json!({"cmd":"playback_pause"})),
            &mut metadata,
        )
        .unwrap();
        assert!(!shared.audio.enabled.load(Ordering::Relaxed));
        assert!(shared.audio.epoch.load(Ordering::Relaxed) > epoch);
        handle_control(
            &shared,
            message(json!({"cmd":"playback_resume"})),
            &mut metadata,
        )
        .unwrap();
        assert!(shared.audio.enabled.load(Ordering::Relaxed));
        handle_control(
            &shared,
            message(json!({"cmd":"metaData","playmode":1})),
            &mut metadata,
        )
        .unwrap();
        assert!(!shared.audio.enabled.load(Ordering::Relaxed));
    }
}
