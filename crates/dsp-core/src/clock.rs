//! Monotonic timing helpers for the realtime audio path. Everything here is
//! `Instant`-based (a portable monotonic clock — `QueryPerformanceCounter`
//! backed on Windows, `CLOCK_MONOTONIC` on Linux) — never wall-clock — and
//! does no allocation, so it is safe to call from the audio callback.
//!
//! Ported from the reference Linux project's
//! `clearnai-core/src/clock.rs` (`FrameDeadline`, `FrameConfig`). The
//! reference computed `frame_samples()`/`frame_duration()` purely from
//! `sample_rate_hz`/`frame_ms` — nothing PipeWire-specific — so this port is
//! a direct, faithful copy with no adaptation needed.

use std::time::{Duration, Instant};

/// A single frame's deadline: the frame must be fully processed before
/// `deadline` elapses, or it counts as a deadline miss.
#[derive(Clone, Copy)]
pub struct FrameDeadline {
    pub arrival: Instant,
    pub deadline: Instant,
}

impl FrameDeadline {
    pub fn start(frame_duration: Duration) -> Self {
        let arrival = Instant::now();
        Self {
            arrival,
            deadline: arrival + frame_duration,
        }
    }

    /// Nanoseconds spent so far since the frame arrived.
    pub fn elapsed_ns(&self) -> u64 {
        self.arrival.elapsed().as_nanos() as u64
    }

    /// True if `Instant::now()` is already past this frame's deadline.
    pub fn missed(&self) -> bool {
        Instant::now() > self.deadline
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameConfig {
    pub sample_rate_hz: u32,
    pub frame_ms: u32,
}

impl FrameConfig {
    pub fn frame_samples(&self) -> usize {
        (self.sample_rate_hz as u64 * self.frame_ms as u64 / 1000) as usize
    }

    pub fn frame_duration(&self) -> Duration {
        Duration::from_micros(self.frame_ms as u64 * 1000)
    }
}

/// This crate's fixed frame config, matching `FRAME_SAMPLES`/`SAMPLE_RATE_HZ`
/// in `lib.rs`: 480 samples @ 48kHz = 10ms.
pub const DEFAULT_FRAME_CONFIG: FrameConfig = FrameConfig {
    sample_rate_hz: crate::SAMPLE_RATE_HZ,
    frame_ms: 10,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_ms_at_48k_is_480_samples() {
        let cfg = FrameConfig {
            sample_rate_hz: 48_000,
            frame_ms: 10,
        };
        assert_eq!(cfg.frame_samples(), 480);
    }

    #[test]
    fn default_frame_config_matches_frame_samples_constant() {
        assert_eq!(DEFAULT_FRAME_CONFIG.frame_samples(), crate::FRAME_SAMPLES);
        assert_eq!(DEFAULT_FRAME_CONFIG.frame_duration(), Duration::from_millis(10));
    }

    #[test]
    fn deadline_not_missed_immediately() {
        let deadline = FrameDeadline::start(Duration::from_millis(10));
        assert!(!deadline.missed());
        assert!(deadline.elapsed_ns() < 10_000_000);
    }
}
