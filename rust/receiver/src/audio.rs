//! Only fixed-size frames, atomics, and arithmetic reach the realtime callback.
//! Decoding, allocation, device creation, networking, and logging stay off it.
use crate::{
    client::ClockData,
    output_clock::OutputClock,
    protocol::{adts_frames, AudioPacket},
    Args,
};
use anyhow::{bail, ensure, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, SizedSample,
};
use rtrb::{Consumer, RingBuffer};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC},
    formats::Packet,
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_FRAMES: usize = 192_000;

pub struct AudioControls {
    pub enabled: AtomicBool,
    pub ready: AtomicBool,
    pub muted: AtomicBool,
    pub volume: AtomicU8,
    pub channel: AtomicU8,
    pub epoch: AtomicU64,
    pub rendered_position: AtomicU64,
    shutdown: AtomicBool,
    produced: AtomicU64,
    consumed: AtomicU64,
    source_rate: AtomicU32,
    decoded: AtomicU64,
    gaps: AtomicU64,
    missing_frames: AtomicU64,
    late_frames: AtomicU64,
    device_errors: AtomicU64,
    decode_errors: AtomicU64,
}
pub struct Stats {
    pub decoded: u64,
    pub gaps: u64,
    pub missing_frames: u64,
    pub late_frames: u64,
    pub device_errors: u64,
    pub decode_errors: u64,
    pub buffered_ms: f64,
}
impl AudioControls {
    pub fn new(volume: u8) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            volume: AtomicU8::new(volume),
            channel: AtomicU8::new(0),
            epoch: AtomicU64::new(0),
            rendered_position: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            produced: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            source_rate: AtomicU32::new(44100),
            decoded: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            missing_frames: AtomicU64::new(0),
            late_frames: AtomicU64::new(0),
            device_errors: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
        }
    }
    pub fn snapshot(&self) -> Stats {
        let queued = self
            .produced
            .load(Ordering::Relaxed)
            .saturating_sub(self.consumed.load(Ordering::Relaxed));
        Stats {
            decoded: self.decoded.load(Ordering::Relaxed),
            gaps: self.gaps.load(Ordering::Relaxed),
            missing_frames: self.missing_frames.load(Ordering::Relaxed),
            late_frames: self.late_frames.load(Ordering::Relaxed),
            device_errors: self.device_errors.load(Ordering::Relaxed),
            decode_errors: self.decode_errors.load(Ordering::Relaxed),
            buffered_ms: queued as f64 * 1000.0
                / self.source_rate.load(Ordering::Relaxed).max(1) as f64,
        }
    }
}

pub enum EncodedAudio {
    Data {
        packet: AudioPacket,
        presentation: i64,
        epoch: u64,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug)]
struct AudioFrame {
    time_ns: i64,
    position: u64,
    epoch: u64,
    rate: u32,
    sample: [f32; 2],
}

#[derive(Default)]
struct SourceTimeline {
    next_position: Option<u64>,
    next_time: f64,
    rate: u32,
}
impl SourceTimeline {
    fn schedule(&mut self, packet: &AudioPacket, presentation: i64, frames: usize) -> (f64, f64) {
        let nominal = 1_000_000_000.0 / packet.rate as f64;
        let error = presentation as f64 - self.next_time;
        let contiguous = self.next_position == Some(packet.position) && self.rate == packet.rate;
        let (start, step) = if contiguous && error.abs() < 50_000_000.0 {
            // Captured leaders revise their sample/clock reference by hundreds
            // of microseconds between AAC packets. Keep contiguous PCM on one
            // timeline and spread small corrections over time instead of
            // inserting gaps or overlaps at every new reference.
            let correction =
                (error * 0.01 / (frames.max(1) as f64 * nominal)).clamp(-0.0002, 0.0002);
            (self.next_time, nominal * (1.0 + correction))
        } else {
            // Preserve actual seeks, missing packets, format changes, or large
            // clock discontinuities; never fabricate the missing source audio.
            (presentation as f64, nominal)
        };
        self.next_position = Some(packet.position.saturating_add(frames as u64));
        self.next_time = start + frames as f64 * step;
        self.rate = packet.rate;
        (start, step)
    }
}

struct Renderer {
    queue: Consumer<AudioFrame>,
    channels: usize,
    output_rate: u32,
    trim_ns: i64,
    previous: Option<AudioFrame>,
    next: Option<AudioFrame>,
    epoch: u64,
    clock_initialized: bool,
    offset: f64,
    gain: f32,
    coverage: f32,
    last_sample: [f32; 2],
    started: bool,
    in_gap: bool,
}
impl Renderer {
    fn new(queue: Consumer<AudioFrame>, channels: usize, output_rate: u32, trim_ns: i64) -> Self {
        Self {
            queue,
            channels,
            output_rate,
            trim_ns,
            previous: None,
            next: None,
            epoch: u64::MAX,
            clock_initialized: false,
            offset: 0.0,
            gain: 0.0,
            coverage: 0.0,
            last_sample: [0.0; 2],
            started: false,
            in_gap: false,
        }
    }

    fn pull(&mut self, budget: &mut usize, popped: &mut u64) {
        if self.next.is_some() {
            return;
        }
        while *budget > 0 {
            let Ok(frame) = self.queue.pop() else {
                break;
            };
            *budget -= 1;
            *popped += 1;
            if frame.epoch == self.epoch {
                self.next = Some(frame);
                break;
            }
        }
    }

    fn sample(
        &mut self,
        time: f64,
        budget: &mut usize,
        popped: &mut u64,
        late: &mut u64,
    ) -> Option<[f32; 2]> {
        self.pull(budget, popped);
        while self.next.is_some_and(|f| f.time_ns as f64 <= time) {
            let frame = self.next.take().unwrap();
            if time - frame.time_ns as f64 > 2_000_000_000.0 / frame.rate as f64 {
                *late += 1;
            }
            self.previous = Some(frame);
            self.pull(budget, popped);
        }
        let a = self.previous?;
        let period = 1_000_000_000.0 / a.rate as f64;
        let elapsed = time - a.time_ns as f64;
        if elapsed < 0.0 || elapsed >= period * 1.5 {
            return None;
        }
        if let Some(b) = self.next {
            // Interpolate across AAC frame boundaries as well as within frames.
            let span = (b.time_ns - a.time_ns) as f64;
            if b.position == a.position + 1
                && b.rate == a.rate
                && span > 0.0
                && span <= period * 1.5
            {
                let fraction = (elapsed / span).clamp(0.0, 1.0) as f32;
                return Some([
                    a.sample[0] + (b.sample[0] - a.sample[0]) * fraction,
                    a.sample[1] + (b.sample[1] - a.sample[1]) * fraction,
                ]);
            }
        }
        if elapsed < period {
            Some(a.sample)
        } else {
            None
        }
    }

    fn render<T: SizedSample + FromSample<f32>>(
        &mut self,
        out: &mut [T],
        dac_ns: i64,
        controls: &AudioControls,
        clock: &ClockData,
    ) {
        let (channels, rate, trim_ns) = (self.channels, self.output_rate, self.trim_ns);
        let epoch = controls.epoch.load(Ordering::Acquire);
        if epoch != self.epoch {
            self.epoch = epoch;
            self.previous = None;
            self.next = None;
            self.started = false;
            self.in_gap = false;
            self.clock_initialized = false;
        }
        let enabled =
            controls.enabled.load(Ordering::Acquire) && clock.ready.load(Ordering::Acquire);
        let target_offset = clock.offset.load(Ordering::Acquire) as f64;
        if enabled && !self.clock_initialized {
            self.offset = target_offset;
            self.clock_initialized = true;
        }
        let frames = out.len() / channels;
        let ns_per_frame = 1_000_000_000.0 / rate as f64;
        // Slew at <=200 ppm instead of stepping through the waveform on a new clock estimate.
        let max_slew = frames as f64 * ns_per_frame * 0.0002;
        let offset_step =
            (target_offset - self.offset).clamp(-max_slew, max_slew) / frames.max(1) as f64;
        let target_gain = if enabled && !controls.muted.load(Ordering::Relaxed) {
            controls.volume.load(Ordering::Relaxed) as f32 / 100.0
        } else {
            0.0
        };
        let gain_step = 1.0 / (rate as f32 * 0.005);
        let fade_step = 1.0 / (rate as f32 * 0.003);
        let channel = controls.channel.load(Ordering::Relaxed);
        let (mut popped, mut late, mut missing, mut gaps) = (0, 0, 0, 0);
        let mut budget = 16384;
        for (index, output) in out.chunks_mut(channels).enumerate() {
            self.offset += offset_step;
            let time = dac_ns as f64 + index as f64 * ns_per_frame + self.offset - trim_ns as f64;
            let sample = self.sample(time, &mut budget, &mut popped, &mut late);
            if let Some(sample) = sample.filter(|_| enabled) {
                self.last_sample = sample;
                self.coverage = (self.coverage + fade_step).min(1.0);
                self.started = true;
                self.in_gap = false;
                if let Some(frame) = self.previous {
                    controls
                        .rendered_position
                        .store(frame.position, Ordering::Relaxed);
                }
            } else {
                if enabled && self.started {
                    missing += 1;
                    if !self.in_gap {
                        gaps += 1;
                        self.in_gap = true;
                    }
                }
                self.coverage = (self.coverage - fade_step).max(0.0);
                if !enabled {
                    self.started = false;
                    self.in_gap = false;
                }
            }
            self.gain += (target_gain - self.gain).clamp(-gain_step, gain_step);
            let mut stereo = self.last_sample.map(|s| s * self.gain * self.coverage);
            if channel == 1 {
                stereo[1] = stereo[0];
            } else if channel == 2 {
                stereo[0] = stereo[1];
            }
            for (channel, target) in output.iter_mut().enumerate() {
                let sample = if channels == 1 {
                    (stereo[0] + stereo[1]) * 0.5
                } else {
                    stereo.get(channel).copied().unwrap_or(0.0)
                };
                *target = T::from_sample(sample.clamp(-1.0, 1.0));
            }
        }
        controls.consumed.fetch_add(popped, Ordering::Relaxed);
        controls.late_frames.fetch_add(late, Ordering::Relaxed);
        controls
            .missing_frames
            .fetch_add(missing, Ordering::Relaxed);
        controls.gaps.fetch_add(gaps, Ordering::Relaxed);
    }
}

fn select_output(selection: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(selection) = selection else {
        return host
            .default_output_device()
            .context("OS default audio output is unavailable");
    };
    let devices: Vec<_> = host.output_devices()?.collect();
    if let Ok(index) = selection.parse::<usize>() {
        return devices
            .into_iter()
            .nth(index)
            .context("output index not found; use --list-outputs");
    }
    let mut matches = devices.into_iter().filter(|d| {
        d.name()
            .is_ok_and(|name| name.eq_ignore_ascii_case(selection))
    });
    let device = matches
        .next()
        .context("output device not found; use --list-outputs")?;
    ensure!(
        matches.next().is_none(),
        "multiple devices have that name; use the output index"
    );
    Ok(device)
}
pub fn list_outputs() -> Result<()> {
    let host = cpal::default_host();
    let default = host.default_output_device().and_then(|d| d.name().ok());
    for (index, device) in host.output_devices()?.enumerate() {
        let name = device.name()?;
        let config = device.default_output_config()?;
        println!(
            "{index}: {name} ({} Hz){}",
            config.sample_rate().0,
            if Some(&name) == default.as_ref() {
                " [OS default]"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut renderer: Renderer,
    controls: Arc<AudioControls>,
    clock: Arc<ClockData>,
) -> Result<cpal::Stream> {
    let errors = controls.clone();
    #[cfg(target_os = "macos")]
    let mut output_clock = Some(OutputClock::calibrate(&clock)?);
    #[cfg(not(target_os = "macos"))]
    let mut output_clock: Option<OutputClock> = None;
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
            let stamp = info.timestamp();
            let mapping = output_clock
                .get_or_insert_with(|| OutputClock::new(stamp.callback, clock.now() as i64));
            let dac = mapping.local_ns(stamp.playback);
            renderer.render(out, dac, &controls, &clock);
        },
        move |_error| {
            errors.device_errors.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )?)
}

fn open_output(
    args: &Args,
    consumer: Consumer<AudioFrame>,
    controls: Arc<AudioControls>,
    clock: Arc<ClockData>,
) -> Result<(cpal::Stream, String)> {
    let device = select_output(args.output.as_deref())?;
    let name = device.name()?;
    let supported = device.default_output_config()?;
    let mut config = supported.config();
    if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
        config.buffer_size = cpal::BufferSize::Fixed(args.buffer_frames.clamp(*min, *max));
    }
    let trim = (args.latency_ms * 1_000_000.0).round() as i64;
    let renderer = Renderer::new(
        consumer,
        config.channels as usize,
        config.sample_rate.0,
        trim,
    );
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            build_stream::<f32>(&device, &config, renderer, controls, clock)?
        }
        cpal::SampleFormat::I16 => {
            build_stream::<i16>(&device, &config, renderer, controls, clock)?
        }
        cpal::SampleFormat::U16 => {
            build_stream::<u16>(&device, &config, renderer, controls, clock)?
        }
        format => bail!("unsupported output format {format}; select another output device"),
    };
    stream.play()?;
    Ok((
        stream,
        format!(
            "{name} ({} Hz, {:?} buffer)",
            config.sample_rate.0, config.buffer_size
        ),
    ))
}

struct AacDecoder {
    decoder: Option<Box<dyn Decoder>>,
    config: Option<[u8; 2]>,
}
impl AacDecoder {
    fn new() -> Self {
        Self {
            decoder: None,
            config: None,
        }
    }
    fn decode(&mut self, packet: &AudioPacket) -> Result<Vec<[f32; 2]>> {
        let mut pcm = Vec::with_capacity(2048);
        for (data, config) in adts_frames(&packet.payload)? {
            if self.config != Some(config) {
                let mut params = CodecParameters::new();
                params
                    .for_codec(CODEC_TYPE_AAC)
                    .with_sample_rate(packet.rate);
                params.extra_data = Some(Box::new(config));
                self.decoder = Some(
                    symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?,
                );
                self.config = Some(config);
            }
            let decoded = self
                .decoder
                .as_mut()
                .unwrap()
                .decode(&Packet::new_from_slice(0, 0, 1024, data))?;
            ensure!(
                decoded.spec().channels.count() == 2 && decoded.spec().rate == packet.rate,
                "AAC format does not match WMRM framing"
            );
            let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            buffer.copy_interleaved_ref(decoded);
            pcm.extend(buffer.samples().chunks_exact(2).map(|s| [s[0], s[1]]));
        }
        Ok(pcm)
    }
}

pub struct AudioWorker {
    pub sender: mpsc::Sender<EncodedAudio>,
    pub errors: mpsc::UnboundedReceiver<String>,
    pub output_name: String,
    thread: Option<thread::JoinHandle<()>>,
    controls: Arc<AudioControls>,
}
impl AudioWorker {
    pub async fn start(
        args: &Args,
        controls: Arc<AudioControls>,
        clock: Arc<ClockData>,
    ) -> Result<Self> {
        let (sender, mut receiver) = mpsc::channel(32);
        let (errors_tx, errors) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let args = args.clone();
        let control = controls.clone();
        let thread = thread::Builder::new()
            .name("wiim-aac-decoder".into())
            .spawn(move || {
                let (mut producer, consumer) = RingBuffer::new(QUEUE_FRAMES);
                let output = open_output(&args, consumer, control.clone(), clock);
                let (_stream, name) = match output {
                    Ok(output) => output,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                if ready_tx.send(Ok(name)).is_err() {
                    return;
                }
                let mut decoder = AacDecoder::new();
                let mut epoch = u64::MAX;
                let mut timeline = SourceTimeline::default();
                while let Some(message) = receiver.blocking_recv() {
                    let EncodedAudio::Data {
                        packet,
                        presentation,
                        epoch: packet_epoch,
                    } = message
                    else {
                        break;
                    };
                    if control.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if packet_epoch != control.epoch.load(Ordering::Acquire) {
                        continue;
                    }
                    if epoch != packet_epoch {
                        decoder = AacDecoder::new();
                        epoch = packet_epoch;
                        timeline = SourceTimeline::default();
                    }
                    match decoder.decode(&packet) {
                        Ok(samples) => {
                            control.source_rate.store(packet.rate, Ordering::Relaxed);
                            let (start, step) =
                                timeline.schedule(&packet, presentation, samples.len());
                            for (offset, sample) in samples.into_iter().enumerate() {
                                let frame = AudioFrame {
                                    time_ns: (start + offset as f64 * step).round() as i64,
                                    position: packet.position + offset as u64,
                                    epoch,
                                    rate: packet.rate,
                                    sample,
                                };
                                loop {
                                    if control.shutdown.load(Ordering::Acquire)
                                        || epoch != control.epoch.load(Ordering::Acquire)
                                    {
                                        break;
                                    }
                                    if producer.push(frame).is_ok() {
                                        control.produced.fetch_add(1, Ordering::Relaxed);
                                        break;
                                    }
                                    thread::sleep(Duration::from_millis(2));
                                }
                            }
                            control.decoded.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            control.decode_errors.fetch_add(1, Ordering::Relaxed);
                            decoder = AacDecoder::new();
                        }
                    }
                }
                drop(errors_tx);
            })?;
        let output_name = ready_rx
            .await
            .context("audio thread stopped during startup")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            sender,
            errors,
            output_name,
            thread: Some(thread),
            controls,
        })
    }
    pub async fn close(&mut self) {
        self.controls.shutdown.store(true, Ordering::Release);
        let _ = self.sender.send(EncodedAudio::Shutdown).await;
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.controls.shutdown.store(true, Ordering::Release);
        let _ = self.sender.try_send(EncodedAudio::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::Producer;
    #[test]
    fn noisy_packet_clock_references_do_not_create_pcm_gaps() {
        let (mut producer, consumer) = RingBuffer::new(8192);
        let mut renderer = Renderer::new(consumer, 2, 48000, 0);
        let controls = AudioControls::new(100);
        controls.enabled.store(true, Ordering::Relaxed);
        let clock = ClockData::new();
        clock.ready.store(true, Ordering::Relaxed);
        let mut timeline = SourceTimeline::default();
        let mut previous_end: Option<f64> = None;
        for block in 0..8 {
            let position = block * 1024;
            let jitter = if block == 0 {
                0
            } else if block % 2 == 0 {
                500_000
            } else {
                -400_000
            };
            let presentation =
                (position as f64 * 1_000_000_000.0 / 44100.0).round() as i64 + jitter;
            let packet = AudioPacket {
                rate: 44100,
                position,
                reference_position: 0,
                reference_ns: 0,
                payload: vec![],
            };
            let (start, step) = timeline.schedule(&packet, presentation, 1024);
            if let Some(end) = previous_end {
                assert!((start - end).abs() < 1.0);
            }
            assert!((step / (1_000_000_000.0 / 44100.0) - 1.0).abs() <= 0.00020001);
            for offset in 0..1024 {
                producer
                    .push(AudioFrame {
                        time_ns: (start + offset as f64 * step).round() as i64,
                        position: position + offset,
                        epoch: 0,
                        rate: 44100,
                        sample: [0.25; 2],
                    })
                    .unwrap();
            }
            previous_end = Some(start + 1024.0 * step);
        }
        let mut output = vec![0.0_f32; 8700 * 2];
        renderer.render(&mut output, 0, &controls, &clock);
        assert_eq!(controls.snapshot().gaps, 0);
        assert_eq!(controls.snapshot().missing_frames, 0);
        // A missing AAC packet must remain a genuine discontinuity.
        let packet = AudioPacket {
            rate: 44100,
            position: 10 * 1024,
            reference_position: 0,
            reference_ns: 0,
            payload: vec![],
        };
        let (start, _) = timeline.schedule(&packet, 300_000_000, 1024);
        assert_eq!(start, 300_000_000.0);
    }
    #[test]
    fn decodes_aac_fixture_without_ffmpeg_or_python() {
        let packet = AudioPacket {
            rate: 44100,
            position: 0,
            reference_position: 0,
            reference_ns: 0,
            payload: include_bytes!("../tests/fixtures/tone.aac").to_vec(),
        };
        let samples = AacDecoder::new().decode(&packet).unwrap();
        assert!(samples.len() >= 4096);
        assert!(samples.iter().flatten().all(|s| s.is_finite()));
        assert!(samples.iter().flatten().any(|s| s.abs() > 0.01));
    }
    fn renderer() -> (Producer<AudioFrame>, Renderer, AudioControls, ClockData) {
        let (producer, consumer) = RingBuffer::new(256);
        let mut renderer = Renderer::new(consumer, 2, 1000, 0);
        renderer.gain = 1.0;
        renderer.coverage = 1.0;
        let controls = AudioControls::new(100);
        controls.enabled.store(true, Ordering::Relaxed);
        let clock = ClockData::new();
        clock.ready.store(true, Ordering::Relaxed);
        (producer, renderer, controls, clock)
    }
    fn frame(position: u64, sample: f32) -> AudioFrame {
        AudioFrame {
            time_ns: position as i64 * 1_000_000,
            position,
            epoch: 0,
            rate: 1000,
            sample: [sample; 2],
        }
    }
    #[test]
    fn interpolates_across_packet_boundaries_and_resamples() {
        let (mut producer, mut renderer, controls, clock) = renderer();
        for position in 0..10 {
            producer
                .push(frame(position, position as f32 / 10.0))
                .unwrap();
        }
        let mut out = [0.0_f32; 6];
        renderer.output_rate = 2000;
        renderer.render(&mut out, 500_000, &controls, &clock);
        for (actual, expected) in out.chunks(2).zip([0.05, 0.1, 0.15]) {
            assert!((actual[0] - expected).abs() < 1e-6);
        }
    }
    #[test]
    fn counts_gaps_and_silences_expired_audio() {
        let (mut producer, mut renderer, controls, clock) = renderer();
        for position in 0..3 {
            producer.push(frame(position, 0.5)).unwrap();
        }
        let mut out = [0.0_f32; 40];
        renderer.render(&mut out, 0, &controls, &clock);
        let stats = controls.snapshot();
        assert_eq!(stats.gaps, 1);
        assert!(stats.missing_frames >= 16);
        assert_eq!(&out[38..], [0.0, 0.0]);
    }
    #[test]
    fn epoch_flush_discards_old_audio_and_mute_ramps() {
        let (mut producer, mut renderer, controls, clock) = renderer();
        for position in 0..10 {
            producer.push(frame(position, 0.5)).unwrap();
        }
        controls.epoch.store(1, Ordering::Relaxed);
        for position in 0..10 {
            let mut frame = frame(position, 0.2);
            frame.epoch = 1;
            producer.push(frame).unwrap();
        }
        let mut out = [0.0_f32; 4];
        renderer.render(&mut out, 0, &controls, &clock);
        assert!((out[0] - 0.2).abs() < 1e-6);
        controls.muted.store(true, Ordering::Relaxed);
        let mut out = [0.0_f32; 16];
        renderer.render(&mut out, 2_000_000, &controls, &clock);
        assert!(out[0] > 0.0 && out[0] < 0.2);
        assert_eq!(out[14], 0.0);
    }
    #[test]
    fn clock_estimate_changes_are_slewed() {
        let (mut producer, mut renderer, controls, clock) = renderer();
        for position in 0..200 {
            producer.push(frame(position, 0.5)).unwrap();
        }
        let mut out = [0.0_f32; 20];
        renderer.render(&mut out, 0, &controls, &clock);
        clock.offset.store(1_000_000, Ordering::Relaxed);
        renderer.render(&mut out, 10_000_000, &controls, &clock);
        assert!(renderer.offset > 0.0 && renderer.offset <= 2000.01);
    }
}
