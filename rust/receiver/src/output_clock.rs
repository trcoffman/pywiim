//! Map stream timestamps to the same local epoch used by the group clock.
use cpal::StreamInstant;

pub struct OutputClock {
    stream_origin: StreamInstant,
    local_origin_ns: i64,
}
impl OutputClock {
    pub fn new(stream_origin: StreamInstant, local_origin_ns: i64) -> Self {
        Self {
            stream_origin,
            local_origin_ns,
        }
    }

    pub fn local_ns(&self, timestamp: StreamInstant) -> i64 {
        let delta = if let Some(elapsed) = timestamp.duration_since(&self.stream_origin) {
            elapsed.as_nanos() as i128
        } else {
            -(self
                .stream_origin
                .duration_since(&timestamp)
                .unwrap()
                .as_nanos() as i128)
        };
        (i128::from(self.local_origin_ns) + delta).clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64
    }

    #[cfg(target_os = "macos")]
    pub fn calibrate(clock: &crate::client::ClockData) -> anyhow::Result<Self> {
        use mach2::mach_time::{mach_absolute_time, mach_timebase_info};
        let mut timebase = mach_timebase_info::default();
        // SAFETY: mach_timebase_info writes a caller-owned, correctly sized
        // structure. mach_absolute_time has no parameters or mutable state.
        let status = unsafe { mach2::mach_time::mach_timebase_info(&mut timebase) };
        anyhow::ensure!(
            status == 0 && timebase.denom != 0,
            "could not read the CoreAudio host timebase"
        );
        let mut best = None;
        for _ in 0..16 {
            let before = clock.now();
            let ticks = unsafe { mach_absolute_time() };
            let after = clock.now();
            let ns = u128::from(ticks) * u128::from(timebase.numer) / u128::from(timebase.denom);
            let stream =
                StreamInstant::new((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32);
            let local = (before + (after - before) / 2) as i64;
            let duration = after - before;
            if best
                .as_ref()
                .is_none_or(|(best_duration, _, _)| duration < *best_duration)
            {
                best = Some((duration, stream, local));
            }
        }
        let (_, stream, local) = best.unwrap();
        Ok(Self::new(stream, local))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduled_callback_time_is_not_treated_as_wall_clock_now() {
        let clock = OutputClock::new(StreamInstant::new(100, 0), 1_000_000_000);
        // The callback may arrive before its timestamp; anchoring that future
        // timestamp to invocation time would lose the device's output lead.
        assert_eq!(
            clock.local_ns(StreamInstant::new(100, 20_000_000)),
            1_020_000_000
        );
        assert_eq!(
            clock.local_ns(StreamInstant::new(100, 45_000_000)),
            1_045_000_000
        );
        assert_eq!(
            clock.local_ns(StreamInstant::new(99, 990_000_000)),
            990_000_000
        );
    }
}
