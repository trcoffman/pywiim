"""Test the actual callback timeline using synthetic PCM, without an audio device."""

import pytest

np = pytest.importorskip("numpy")
pytest.importorskip("av")
pytest.importorskip("Crypto")

from pywiim.receiver.audio import PlaybackBuffer, ScheduledAudio  # noqa: E402


def ready_buffer():
    buffer = PlaybackBuffer(1000)
    buffer.enabled = True
    buffer.volume = 100
    return buffer


def test_wait_for_scheduled_dac_time_and_drop_expired_audio():
    buffer = ready_buffer()
    buffer.append(ScheduledAudio(1_100_000_000, np.ones((100, 2), dtype="float32"), 500))
    output = np.empty((200, 2), dtype="float32")
    buffer.render(output, 1_000_000_000)
    np.testing.assert_array_equal(output[:100], 0)
    np.testing.assert_array_equal(output[100:], 1)
    assert buffer.rendered_position == 600
    buffer.render(output, 1_300_000_000)
    np.testing.assert_array_equal(output, 0)
    assert buffer.buffered_ms == 0


def test_fractional_clock_phase_interpolates_samples():
    buffer = ready_buffer()
    pcm = np.repeat(np.arange(10, dtype="float32")[:, None], 2, axis=1)
    buffer.append(ScheduledAudio(0, pcm, 0))
    output = np.empty((3, 2), dtype="float32")
    buffer.render(output, 500_000)
    np.testing.assert_allclose(output[:, 0], [0.5, 1.5, 2.5])


def test_volume_mute_pause_and_channel_selection():
    buffer = ready_buffer()
    buffer.append(ScheduledAudio(0, np.tile([0.5, 1.0], (100, 1)), 0))
    buffer.volume = 50
    output = np.empty((10, 2), dtype="float32")
    buffer.render(output, 0)
    np.testing.assert_allclose(output, np.tile([0.25, 0.5], (10, 1)))
    buffer.channel = 1
    buffer.render(output, 0)
    np.testing.assert_allclose(output, 0.25)
    buffer.muted = True
    buffer.render(output, 0)
    np.testing.assert_array_equal(output, 0)
    buffer.muted = False
    buffer.enabled = False
    buffer.render(output, 0)
    np.testing.assert_array_equal(output, 0)


def test_buffer_is_bounded_and_clear_removes_pending_audio():
    buffer = ready_buffer()
    for second in range(100):
        buffer.append(ScheduledAudio(second * 1_000_000_000, np.ones((10, 2)), second * 10))
    assert buffer.buffered_ms <= 60
    buffer.clear()
    assert buffer.buffered_ms == 0
