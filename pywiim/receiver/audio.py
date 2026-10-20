"""AAC decoding and playback scheduled against the CoreAudio output clock."""

from __future__ import annotations

import threading
import time
from collections import deque
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import av
import numpy as np

if TYPE_CHECKING:
    import sounddevice as sd

from .protocol import AudioPacket, ProtocolError


@dataclass
class ScheduledAudio:
    """Decoded interleaved float samples and their local presentation time."""

    start_ns: int
    samples: np.ndarray
    sample_position: int


class PlaybackBuffer:
    """Bounded timeline shared by the network task and the audio callback.

    Each callback samples the incoming timeline at the DAC's actual timestamps.
    This corrects clock drift instead of accumulating a fixed FIFO delay.
    Missing or late audio produces silence rather than playing stale music.
    """

    def __init__(self, sample_rate: int, channels: int = 2) -> None:
        self.sample_rate = sample_rate
        self.channels = channels
        self.volume = 30
        self.muted = False
        self.enabled = False
        self.channel = 0
        self.rendered_position = 0
        self.late_frames = 0
        self._chunks: deque[ScheduledAudio] = deque()
        self._lock = threading.Lock()

    def append(self, chunk: ScheduledAudio) -> None:
        """Keep at most five seconds of decoded audio, including on a stalled DAC."""
        with self._lock:
            self._chunks.append(chunk)
            while self._chunks and (
                chunk.start_ns - self._chunks[0].start_ns > 5_000_000_000 or len(self._chunks) > 512
            ):
                self._chunks.popleft()

    def clear(self) -> None:
        """Discard audio after a seek, stop, disconnect, or source change."""
        with self._lock:
            self._chunks.clear()

    @property
    def buffered_ms(self) -> float:
        """Decoded audio duration currently held in memory."""
        with self._lock:
            return sum(len(chunk.samples) for chunk in self._chunks) * 1000 / self.sample_rate

    def render(self, output: np.ndarray, dac_ns: int) -> None:
        """Fill one output buffer at the requested audible presentation time."""
        output.fill(0)
        frames = len(output)
        end_ns = dac_ns + frames * 1_000_000_000 / self.sample_rate
        with self._lock:
            while self._chunks:
                first = self._chunks[0]
                if first.start_ns + len(first.samples) * 1_000_000_000 / self.sample_rate > dac_ns:
                    break
                self._chunks.popleft()
            if not self.enabled:
                return
            for chunk in self._chunks:
                if chunk.start_ns >= end_ns:
                    break
                relative = (dac_ns - chunk.start_ns) * self.sample_rate / 1_000_000_000
                begin = max(0, int(np.ceil(-relative)))
                end = min(frames, int(np.ceil(len(chunk.samples) - relative)))
                if end <= begin:
                    continue
                positions = relative + np.arange(begin, end)
                source_grid = np.arange(len(chunk.samples))
                for channel in range(self.channels):
                    output[begin:end, channel] = np.interp(positions, source_grid, chunk.samples[:, channel])
                self.rendered_position = chunk.sample_position + min(len(chunk.samples), int(positions[-1]) + 1)
        if self.muted:
            output.fill(0)
        else:
            output *= self.volume / 100
            if self.channel in (1, 2) and self.channels == 2:
                output[:, 1 if self.channel == 1 else 0] = output[:, self.channel - 1]


class AudioOutput:
    """Decode one AAC access unit at a time and schedule its PCM on CoreAudio."""

    def __init__(self, device: int | str | None = None, latency_ms: float = 0) -> None:
        self.device = device
        self.latency_ms = latency_ms
        self.buffer: PlaybackBuffer | None = None
        self.decoded_packets = 0
        self.underruns = 0
        self._codec: Any = av.CodecContext.create("aac", "r")
        self._resampler: Any = None
        self._stream: sd.OutputStream | None = None
        self._portaudio_offset_ns: int | None = None

    def _open(self, sample_rate: int, channels: int) -> None:
        import sounddevice as sd

        self.close()
        self.buffer = PlaybackBuffer(sample_rate, channels)
        self._resampler = av.AudioResampler(format="flt", layout="stereo", rate=sample_rate)
        self._stream = sd.OutputStream(
            samplerate=sample_rate,
            channels=channels,
            dtype="float32",
            device=self.device,
            blocksize=0,
            latency="low",
            callback=self._callback,
        )
        self._stream.start()
        # PortAudio and Python use different monotonic clock epochs. Calibrate
        # with the shortest measurement, then use the DAC's timestamps directly.
        measurements = []
        for _ in range(8):
            before = time.monotonic_ns()
            pa_ns = round(self._stream.time * 1_000_000_000)
            after = time.monotonic_ns()
            measurements.append((after - before, (before + after) // 2 - pa_ns))
        self._portaudio_offset_ns = min(measurements)[1]

    def _callback(self, output: np.ndarray, frames: int, timing: Any, status: Any) -> None:
        if status.output_underflow:
            self.underruns += 1
        if self.buffer is None or self._portaudio_offset_ns is None:
            output.fill(0)
            return
        dac_ns = round(timing.outputBufferDacTime * 1_000_000_000) + self._portaudio_offset_ns
        self.buffer.render(output, dac_ns)

    def feed(self, packet: AudioPacket, presentation_ns: int) -> None:
        """Decode a verified AAC frame; presentation_ns refers to its first sample."""
        if packet.kind != 10:
            raise ProtocolError(f"Unsupported WMRM codec {packet.kind}; this receiver currently supports AAC (10)")
        if packet.channels != 2 or packet.sample_rate not in (32000, 44100, 48000):
            raise ProtocolError(f"Unsupported WMRM format: {packet.sample_rate} Hz, {packet.channels} channels")
        if self.buffer is None or self.buffer.sample_rate != packet.sample_rate:
            self._open(packet.sample_rate, packet.channels)
        assert self.buffer is not None
        sample_offset = 0
        for decoded in self._codec.decode(av.Packet(packet.payload)):
            for frame in self._resampler.resample(decoded):
                pcm = frame.to_ndarray().reshape(-1, 2).copy()
                start_ns = presentation_ns + round(sample_offset * 1_000_000_000 / packet.sample_rate)
                start_ns += round(self.latency_ms * 1_000_000)
                self.buffer.append(ScheduledAudio(start_ns, pcm, packet.sample_position + sample_offset))
                sample_offset += len(pcm)
        self.decoded_packets += 1

    def reset(self) -> None:
        """Discard queued samples and decoder history after a discontinuity."""
        if self.buffer:
            self.buffer.clear()
        self._codec = av.CodecContext.create("aac", "r")

    def close(self) -> None:
        """Release only this application's audio output stream."""
        if self._stream:
            self._stream.stop()
            self._stream.close()
            self._stream = None
        if self.buffer:
            self.buffer.clear()
        self.buffer = None
        self._portaudio_offset_ns = None
