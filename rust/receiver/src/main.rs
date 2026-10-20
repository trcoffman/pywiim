mod audio;
mod client;
mod protocol;
mod server;

use anyhow::{bail, Context, Result};
use clap::Parser;
use client::Shared;
use crossterm::{cursor, event, execute, terminal};
use std::{
    io::{self, IsTerminal, Write},
    net::Ipv4Addr,
    time::{Duration, Instant},
};

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about = "Native macOS Spotify speaker for a WiiM WMRM 4.3 group"
)]
pub struct Args {
    /// IP of the leader or an existing follower; omit to wait for group commands
    speaker: Option<Ipv4Addr>,
    #[arg(long)]
    name: Option<String>,
    /// Stable 24-character hexadecimal speaker identity
    #[arg(long)]
    uuid: Option<String>,
    /// Audio output name or index; defaults to the OS default device
    #[arg(long)]
    output: Option<String>,
    #[arg(long)]
    list_outputs: bool,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u8).range(0..=100))]
    volume: u8,
    /// Positive values delay this Mac relative to the group
    #[arg(long, default_value_t = 0.0)]
    latency_ms: f64,
    /// Requested hardware callback buffer; larger values give scheduling headroom
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(64..=8192))]
    buffer_frames: u32,
    #[arg(long)]
    interface: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 49152, value_parser = clap::value_parser!(u16).range(1..))]
    http_port: u16,
    #[arg(long)]
    no_discovery: bool,
    #[arg(long)]
    no_tui: bool,
    #[arg(long)]
    duration: Option<f64>,
    #[arg(long)]
    verbose: bool,
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
    }
}

async fn display(shared: Shared, args: &Args) -> Result<()> {
    let tui =
        !args.no_tui && !args.verbose && io::stdin().is_terminal() && io::stdout().is_terminal();
    let _guard = if tui {
        terminal::enable_raw_mode()?;
        let guard = TerminalGuard;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Some(guard)
    } else {
        None
    };
    let started = Instant::now();
    let mut previous = String::new();
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let mut diagnostics = Instant::now();
    loop {
        tokio::select! {
            _ = shared.stop.cancelled() => break,
            _ = interval.tick() => {}
        }
        let state = shared.state.lock().unwrap().clone();
        let stats = shared.audio.snapshot();
        let message = format!(
            "{}: {} · {} — {} · volume {}%{}",
            state.name,
            state.message,
            state.artist,
            state.title,
            state.volume,
            if state.muted { " (muted)" } else { "" }
        );
        if tui {
            let lines = [
                format!("WiiM Rust Receiver · {}", state.name),
                String::new(),
                state.message.clone(),
                format!("Group: {}", state.master_name),
                format!("Track: {} — {}", state.artist, state.title),
                format!("Output: {}", shared.output_name),
                format!(
                    "Volume: {}%{}",
                    state.volume,
                    if state.muted { " (muted)" } else { "" }
                ),
                format!(
                    "Clock RTT: {:.2} ms · queued: {:.0} ms",
                    shared.clock.rtt_ms(),
                    stats.buffered_ms
                ),
                format!(
                    "Decoded: {} · audio gaps: {} ({} frames)",
                    stats.decoded, stats.gaps, stats.missing_frames
                ),
                format!(
                    "Late frames: {} · device errors: {} · decode errors: {}",
                    stats.late_frames, stats.device_errors, stats.decode_errors
                ),
                String::new(),
                "+ / - volume   m mute   q / Ctrl+C quit".into(),
            ];
            let mut stdout = io::stdout();
            let (columns, rows) = terminal::size()?;
            execute!(
                stdout,
                cursor::MoveTo(0, 0),
                terminal::Clear(terminal::ClearType::All)
            )?;
            for line in lines.iter().take(rows as usize) {
                let safe: String = line
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(columns.saturating_sub(1) as usize)
                    .collect();
                write!(stdout, "{safe}\r\n")?;
            }
            stdout.flush()?;
            while event::poll(Duration::ZERO)? {
                if let event::Event::Key(key) = event::read()? {
                    use event::{KeyCode, KeyModifiers};
                    match key.code {
                        KeyCode::Char('q' | 'Q') => shared.stop.cancel(),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            shared.stop.cancel()
                        }
                        KeyCode::Char('+' | '=') => {
                            shared.set_volume(state.volume.saturating_add(5).min(100))
                        }
                        KeyCode::Char('-') => shared.set_volume(state.volume.saturating_sub(5)),
                        KeyCode::Char('m' | 'M') => shared.set_mute(!state.muted),
                        _ => {}
                    }
                }
            }
        } else {
            if message != previous {
                println!("{message}");
                previous = message;
            }
            if args.verbose && diagnostics.elapsed() >= Duration::from_secs(2) {
                println!("Audio: decoded={} queued={:.0}ms gaps={} missing_frames={} late_frames={} device_errors={} decode_errors={} clock_rtt={:.2}ms",
                    stats.decoded, stats.buffered_ms, stats.gaps, stats.missing_frames, stats.late_frames, stats.device_errors, stats.decode_errors, shared.clock.rtt_ms());
                diagnostics = Instant::now();
            }
        }
        if args
            .duration
            .is_some_and(|d| started.elapsed().as_secs_f64() >= d)
        {
            shared.stop.cancel();
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    if args.list_outputs {
        return audio::list_outputs();
    }
    if !args.latency_ms.is_finite() || args.latency_ms.abs() > 5000.0 {
        bail!("--latency-ms must be between -5000 and 5000");
    }
    if args.duration.is_some_and(|d| !d.is_finite() || d <= 0.0) {
        bail!("--duration must be positive and finite");
    }
    if args.no_discovery && args.speaker.is_none() {
        bail!("provide a speaker IP with --no-discovery");
    }
    let hostname = hostname::get()?.to_string_lossy().into_owned();
    let name = args
        .name
        .take()
        .unwrap_or_else(|| hostname.split('.').next().unwrap_or("Mac").to_owned());
    if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
        bail!("--name must contain 1..63 printable UTF-8 bytes");
    }
    let uuid = args.uuid.clone().unwrap_or_else(|| {
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            format!("pywiim.receiver:{hostname}").as_bytes(),
        )
        .simple()
        .to_string()[..24]
            .to_uppercase()
    });
    if uuid.len() != 24 || !uuid.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("--uuid must contain exactly 24 hexadecimal characters");
    }
    let leader = match args.speaker {
        Some(ip) => Some(client::resolve_leader(ip).await?),
        None => None,
    };
    let shared = Shared::new(name, uuid.to_uppercase(), args.volume, leader);
    let mut audio =
        audio::AudioWorker::start(&args, shared.audio.clone(), shared.clock.clone()).await?;
    let shared = Shared {
        output_name: audio.output_name.clone(),
        ..shared
    };
    let mut server = if args.no_discovery {
        None
    } else {
        Some(server::DeviceServer::start(shared.clone(), &args).await?)
    };
    let result = {
        let session = client::run(shared.clone(), audio.sender.clone());
        tokio::pin!(session);
        let display = display(shared.clone(), &args);
        tokio::pin!(display);
        let (result, session_done) = tokio::select! {
            result = &mut session => (result, true),
            result = &mut display => (result, false),
            result = tokio::signal::ctrl_c() => (result.context("installing Ctrl+C handler"), false),
            error = audio.errors.recv() => (Err(anyhow::anyhow!(error.unwrap_or_else(|| "audio worker stopped".into()))), false),
        };
        shared.stop.cancel();
        // Poll the session through logout before closing the audio and discovery services.
        if !session_done {
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut session).await;
        }
        result
    };
    if let Some(server) = server.as_mut() {
        server.close().await;
    }
    audio.close().await;
    let stats = shared.audio.snapshot();
    println!("Left group; decoded {} AAC packets. Audio gaps: {} ({} frames), late frames: {}, device errors: {}, decode errors: {}.",
        stats.decoded, stats.gaps, stats.missing_frames, stats.late_frames, stats.device_errors, stats.decode_errors);
    result
}
