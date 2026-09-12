//! Realtime-safe metrics collection.
//!
//! The audio thread only ever does `AtomicU64` increments/stores here —
//! never allocates, locks, or does I/O. Percentile computation (which sorts
//! a snapshot) happens off the audio thread (e.g. a GUI/monitor thread).
//!
//! Ported from the reference Linux project's `clearnai-core/src/metrics.rs`
//! (`LatencyLog`, `compute_percentiles`, `classify_status`,
//! `RealtimeStatus`, `FrameCounters`). Field names, the percentile formula,
//! and the status thresholds (`p95 < deadline` => RealTime, `p99 < 2x
//! deadline` => Degraded, else Overloaded) are copied as-is from the
//! reference — none of it depended on anything PipeWire/Linux-specific, so
//! no adaptation was needed beyond using this crate's own
//! `FRAME_SAMPLES`/`SAMPLE_RATE_HZ`-derived deadline in the doc examples
//! and tests below.
//!
//! Known inherited non-hot-path allocation: `LatencyLog::snapshot()`
//! allocates a `Vec` on each call, exactly like the reference. This is not
//! called from the audio thread (only from a GUI/monitor thread reading the
//! log), so it is fine — flagged here only because the task asked to note
//! it rather than silently "improve" it. `record_ns`, the actual per-frame
//! hot-path call, is allocation-free (fixed-size `Box<[AtomicU64]>`
//! pre-allocated in `LatencyLog::new`, atomic store per call).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Fixed-capacity circular log of per-frame latency samples, in nanoseconds.
/// Single producer (the audio thread) writes; any number of readers can
/// snapshot it. Each slot is a single atomic write, so a reader never sees a
/// torn value — at worst a slightly stale one, which is fine for metrics.
pub struct LatencyLog {
    samples: Box<[AtomicU64]>,
    capacity: usize,
    write_idx: AtomicUsize,
    written: AtomicU64,
}

impl LatencyLog {
    pub fn new(capacity: usize) -> Self {
        let mut v = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            v.push(AtomicU64::new(0));
        }
        Self {
            samples: v.into_boxed_slice(),
            capacity,
            write_idx: AtomicUsize::new(0),
            written: AtomicU64::new(0),
        }
    }

    /// Record one frame's latency, in nanoseconds. Wraps around once full —
    /// this is a rolling window, not an ever-growing log. Allocation-free:
    /// safe to call from the realtime audio callback.
    pub fn record_ns(&self, value_ns: u64) {
        let idx = self.write_idx.fetch_add(1, Ordering::Relaxed) % self.capacity;
        self.samples[idx].store(value_ns, Ordering::Relaxed);
        self.written.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the currently-valid samples (oldest-first is not guaranteed;
    /// order doesn't matter for percentile computation). Allocates a `Vec` —
    /// not called from the audio thread.
    pub fn snapshot(&self) -> Vec<u64> {
        let written = self.written.load(Ordering::Relaxed);
        let count = written.min(self.capacity as u64) as usize;
        self.samples[..count]
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Percentiles {
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
    pub sample_count: usize,
}

pub fn compute_percentiles(mut samples: Vec<u64>) -> Percentiles {
    if samples.is_empty() {
        return Percentiles::default();
    }
    samples.sort_unstable();
    let n = samples.len();
    let at = |p: f64| -> u64 {
        let idx = ((p * (n as f64 - 1.0)).round() as usize).min(n - 1);
        samples[idx]
    };
    let sum: u64 = samples.iter().sum();
    Percentiles {
        p50_ns: at(0.50),
        p90_ns: at(0.90),
        p95_ns: at(0.95),
        p99_ns: at(0.99),
        max_ns: *samples.last().unwrap(),
        mean_ns: sum / n as u64,
        sample_count: n,
    }
}

/// Status classification. `deadline_ns` is the frame period (e.g. 10ms =
/// 10_000_000ns for this crate's `FRAME_SAMPLES`/`SAMPLE_RATE_HZ` = 480/48000);
/// classification is based on measured *processing time* percentiles against
/// that deadline, never on RTF or average-only comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeStatus {
    RealTime,
    Degraded,
    Overloaded,
    Bypass,
}

pub fn classify_status(processing: &Percentiles, deadline_ns: u64, bypass: bool) -> RealtimeStatus {
    if bypass {
        return RealtimeStatus::Bypass;
    }
    if processing.sample_count == 0 {
        return RealtimeStatus::RealTime;
    }
    if processing.p95_ns < deadline_ns {
        RealtimeStatus::RealTime
    } else if processing.p99_ns < deadline_ns * 2 {
        RealtimeStatus::Degraded
    } else {
        RealtimeStatus::Overloaded
    }
}

/// Frame-level counters, all realtime-safe atomic increments.
#[derive(Default)]
pub struct FrameCounters {
    pub frames_processed: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub deadline_misses: AtomicU64,
    pub input_underruns: AtomicU64,
    pub input_overruns: AtomicU64,
    pub output_underruns: AtomicU64,
    pub output_overruns: AtomicU64,
    /// Frames dropped from an optional side-tap (e.g. a live transcript or
    /// meter feed): this only ever affects that side channel, never the
    /// main audio path, which has its own separate counters above.
    pub transcript_dropped: AtomicU64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_SAMPLES, SAMPLE_RATE_HZ};

    /// This crate's frame deadline: 480 samples @ 48kHz = 10ms = 10_000_000ns.
    const DEADLINE_NS: u64 = (FRAME_SAMPLES as u64 * 1_000_000_000) / SAMPLE_RATE_HZ as u64;

    #[test]
    fn deadline_ns_is_ten_ms() {
        assert_eq!(DEADLINE_NS, 10_000_000);
    }

    #[test]
    fn percentiles_of_uniform_samples() {
        let samples: Vec<u64> = (1..=100).collect();
        let p = compute_percentiles(samples);
        assert_eq!(p.max_ns, 100);
        assert!(p.p50_ns == 50 || p.p50_ns == 51);
        assert!(p.p95_ns >= 94 && p.p95_ns <= 96);
    }

    #[test]
    fn percentiles_hand_checked_small_dataset() {
        // n = 10, values 10..=100 step 10. idx(p) = round(p * 9).
        let samples: Vec<u64> = (1..=10).map(|i| i * 10).collect();
        let p = compute_percentiles(samples);
        // p50: round(0.50*9)=round(4.5)=5 (f64::round is half-away-from-zero) -> idx 5 -> value 60
        // sorted: [10,20,30,40,50,60,70,80,90,100], idx5 = 60
        assert_eq!(p.p50_ns, 60);
        // p90: round(0.90*9)=round(8.1)=8 -> idx8 = 90
        assert_eq!(p.p90_ns, 90);
        // p95: round(0.95*9)=round(8.55)=9 -> idx9 = 100
        assert_eq!(p.p95_ns, 100);
        // p99: round(0.99*9)=round(8.91)=9 -> idx9 = 100
        assert_eq!(p.p99_ns, 100);
        assert_eq!(p.max_ns, 100);
        assert_eq!(p.mean_ns, 55); // sum=550, n=10 -> 55
        assert_eq!(p.sample_count, 10);
    }

    #[test]
    fn percentiles_of_empty_samples_are_default() {
        let p = compute_percentiles(Vec::new());
        assert_eq!(p.sample_count, 0);
        assert_eq!(p.max_ns, 0);
        assert_eq!(p.mean_ns, 0);
    }

    #[test]
    fn log_wraps_without_growing() {
        let log = LatencyLog::new(8);
        for i in 0..1000u64 {
            log.record_ns(i);
        }
        assert_eq!(log.snapshot().len(), 8);
    }

    #[test]
    fn log_snapshot_before_full_reflects_partial_writes() {
        let log = LatencyLog::new(8);
        log.record_ns(1);
        log.record_ns(2);
        log.record_ns(3);
        assert_eq!(log.snapshot().len(), 3);
    }

    #[test]
    fn status_classification_realtime() {
        let good = Percentiles {
            p95_ns: 2_000_000,
            p99_ns: 3_000_000,
            sample_count: 100,
            ..Default::default()
        };
        assert_eq!(classify_status(&good, DEADLINE_NS, false), RealtimeStatus::RealTime);
    }

    #[test]
    fn status_classification_degraded() {
        // p95 at/over deadline, p99 still under 2x deadline.
        let degraded = Percentiles {
            p95_ns: DEADLINE_NS,
            p99_ns: DEADLINE_NS + 1,
            sample_count: 100,
            ..Default::default()
        };
        assert_eq!(
            classify_status(&degraded, DEADLINE_NS, false),
            RealtimeStatus::Degraded
        );
    }

    #[test]
    fn status_classification_overloaded() {
        let overloaded = Percentiles {
            p95_ns: 25_000_000,
            p99_ns: 40_000_000,
            sample_count: 100,
            ..Default::default()
        };
        assert_eq!(
            classify_status(&overloaded, DEADLINE_NS, false),
            RealtimeStatus::Overloaded
        );
    }

    #[test]
    fn status_classification_boundary_exactly_at_thresholds() {
        // p95_ns < deadline_ns is strict: exactly at deadline is NOT RealTime.
        let at_deadline = Percentiles {
            p95_ns: DEADLINE_NS,
            p99_ns: 0,
            sample_count: 1,
            ..Default::default()
        };
        assert_eq!(
            classify_status(&at_deadline, DEADLINE_NS, false),
            RealtimeStatus::Degraded
        );

        // p99_ns < 2*deadline_ns is strict: exactly at 2x deadline is Overloaded, not Degraded.
        let at_double_deadline = Percentiles {
            p95_ns: DEADLINE_NS,
            p99_ns: DEADLINE_NS * 2,
            sample_count: 1,
            ..Default::default()
        };
        assert_eq!(
            classify_status(&at_double_deadline, DEADLINE_NS, false),
            RealtimeStatus::Overloaded
        );

        // Just under the deadline is still RealTime.
        let just_under = Percentiles {
            p95_ns: DEADLINE_NS - 1,
            p99_ns: DEADLINE_NS - 1,
            sample_count: 1,
            ..Default::default()
        };
        assert_eq!(
            classify_status(&just_under, DEADLINE_NS, false),
            RealtimeStatus::RealTime
        );
    }

    #[test]
    fn status_classification_bypass_overrides_everything() {
        let overloaded = Percentiles {
            p95_ns: 999_000_000,
            p99_ns: 999_000_000,
            sample_count: 100,
            ..Default::default()
        };
        assert_eq!(
            classify_status(&overloaded, DEADLINE_NS, true),
            RealtimeStatus::Bypass
        );
    }

    #[test]
    fn status_classification_no_samples_is_realtime() {
        let empty = Percentiles::default();
        assert_eq!(classify_status(&empty, DEADLINE_NS, false), RealtimeStatus::RealTime);
    }

    #[test]
    fn frame_counters_default_atomic_increments() {
        let counters = FrameCounters::default();
        counters.frames_processed.fetch_add(10, Ordering::Relaxed);
        counters.dropped_frames.fetch_add(2, Ordering::Relaxed);
        counters.deadline_misses.fetch_add(1, Ordering::Relaxed);
        assert_eq!(counters.frames_processed.load(Ordering::Relaxed), 10);
        assert_eq!(counters.dropped_frames.load(Ordering::Relaxed), 2);
        assert_eq!(counters.deadline_misses.load(Ordering::Relaxed), 1);
        assert_eq!(counters.input_underruns.load(Ordering::Relaxed), 0);
        assert_eq!(counters.output_overruns.load(Ordering::Relaxed), 0);
        assert_eq!(counters.transcript_dropped.load(Ordering::Relaxed), 0);
    }
}
