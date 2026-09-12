//! Portable, OS-independent core: stage bypass flags, the pipeline trait,
//! and the SPSC ring buffer type used to cross data between the realtime
//! audio thread(s) and everything else (GUI, AEC reference feed, etc).
//!
//! Nothing in this crate touches an OS audio API. It must build and run
//! identically on Linux and Windows.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

pub use rtrb::{Consumer, Producer, RingBuffer};

mod clock;
mod metrics;

pub use clock::{FrameConfig, FrameDeadline, DEFAULT_FRAME_CONFIG};
pub use metrics::{
    classify_status, compute_percentiles, FrameCounters, LatencyLog, Percentiles, RealtimeStatus,
};

/// A lock-free on/off switch for one pipeline stage. The GUI thread flips
/// it; the realtime audio callback reads it every frame with `Relaxed`
/// ordering (audible-frame-granularity staleness is fine, a lock or a
/// stronger ordering is not).
#[derive(Clone)]
pub struct StageToggle(Arc<AtomicBool>);

impl StageToggle {
    pub fn new(initially_on: bool) -> Self {
        Self(Arc::new(AtomicBool::new(initially_on)))
    }

    #[inline(always)]
    pub fn is_on(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::Relaxed);
    }

    pub fn toggle(&self) -> bool {
        let new = !self.is_on();
        self.set(new);
        new
    }
}

/// A lock-free "shared small integer" handle used to publish a rarely-
/// changed preset selection (e.g. `studio_dsp::StudioPreset`, encoded here
/// as a plain `u8` index) from the GUI thread to a realtime audio thread.
///
/// This crate cannot depend on `studio-dsp`'s actual enum (that would be a
/// wrong-direction dependency: `studio-dsp` depends on `dsp-core`, not the
/// other way around), so `SharedPreset` is deliberately generic - callers in
/// the app crate map their real enum to/from the `u8` index. Mirrors
/// `StageToggle`'s API/style: an `Arc`-backed atomic, `Relaxed` ordering
/// (audible-frame-granularity staleness is fine here too), no lock.
#[derive(Clone)]
pub struct SharedPreset(Arc<AtomicU8>);

impl SharedPreset {
    pub fn new(default: u8) -> Self {
        Self(Arc::new(AtomicU8::new(default)))
    }

    #[inline(always)]
    pub fn load(&self) -> u8 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn store(&self, value: u8) {
        self.0.store(value, Ordering::Relaxed);
    }
}

/// One stage in the DSP chain. Implementors process a frame of interleaved
/// or planar f32 samples in place. `process` is called unconditionally by
/// `run_if_enabled` below only when the stage's toggle is on; when off, the
/// buffer passes through completely untouched (true bypass, zero added
/// latency, no allocation).
pub trait Stage: Send {
    /// Human-readable name, used only for logging/GUI labels.
    fn name(&self) -> &'static str;

    /// Process one frame in place. Must not allocate, lock, or block.
    fn process(&mut self, frame: &mut [f32]);
}

/// Runs `stage` on `frame` only if `toggle` is on. This is the single
/// bypass mechanism used throughout the pipeline: every stage is always
/// constructed and warm (models loaded, buffers allocated) at startup,
/// and a toggle flip only changes whether this function calls `process`.
#[inline(always)]
pub fn run_if_enabled(stage: &mut dyn Stage, toggle: &StageToggle, frame: &mut [f32]) {
    if toggle.is_on() {
        stage.process(frame);
    }
}

/// Fixed frame size (samples per channel per callback) the whole pipeline
/// is built around. 480 samples @ 48kHz = 10ms, matching RNNoise/DeepFilterNet's
/// native frame size so no internal resampling/re-framing is needed at the
/// noise-suppression stage boundary.
pub const FRAME_SAMPLES: usize = 480;
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Creates a ring buffer sized for `seconds` of audio at `SAMPLE_RATE_HZ`,
/// rounded up to a whole number of `FRAME_SAMPLES` chunks. Used for the
/// mic->engine, engine->virtual-mic, loopback-tap->AEC, and AEC reference
/// crossings.
pub fn make_ring_buffer(seconds: f32) -> (Producer<f32>, Consumer<f32>) {
    let frames = ((SAMPLE_RATE_HZ as f32 * seconds) / FRAME_SAMPLES as f32).ceil() as usize;
    let capacity = (frames.max(1)) * FRAME_SAMPLES;
    RingBuffer::<f32>::new(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Gain(f32);
    impl Stage for Gain {
        fn name(&self) -> &'static str {
            "gain"
        }
        fn process(&mut self, frame: &mut [f32]) {
            for s in frame.iter_mut() {
                *s *= self.0;
            }
        }
    }

    #[test]
    fn bypass_leaves_frame_untouched() {
        let mut gain = Gain(0.0);
        let toggle = StageToggle::new(false);
        let mut frame = [1.0f32, 2.0, 3.0];
        run_if_enabled(&mut gain, &toggle, &mut frame);
        assert_eq!(frame, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn enabled_runs_stage() {
        let mut gain = Gain(0.0);
        let toggle = StageToggle::new(true);
        let mut frame = [1.0f32, 2.0, 3.0];
        run_if_enabled(&mut gain, &toggle, &mut frame);
        assert_eq!(frame, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn shared_preset_round_trips_and_defaults() {
        let preset = SharedPreset::new(1);
        assert_eq!(preset.load(), 1, "constructed with the given default");
        preset.store(3);
        assert_eq!(preset.load(), 3);
        preset.store(0);
        assert_eq!(preset.load(), 0);
    }

    #[test]
    fn shared_preset_clone_shares_the_same_underlying_value() {
        let a = SharedPreset::new(0);
        let b = a.clone();
        b.store(2);
        assert_eq!(a.load(), 2, "clones must observe writes via the shared Arc");
    }

    #[test]
    fn ring_buffer_round_trip() {
        let (mut p, mut c) = make_ring_buffer(0.1);
        p.push(1.23).unwrap();
        assert_eq!(c.pop().unwrap(), 1.23);
    }
}
