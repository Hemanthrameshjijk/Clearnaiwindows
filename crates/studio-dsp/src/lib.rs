//! "Studio" stage: a hand-rolled EQ / compressor / de-esser / limiter chain
//! for voice, matching the reference Linux (ClearNAI) signal chain:
//!
//! 1. High-pass filter (rumble removal)
//! 2. 3-band EQ (low shelf, mid peak, high shelf)
//! 3. Envelope-follower compressor
//! 4. De-esser (band-limited dynamic EQ over the sibilance band)
//! 5. Peak limiter (brick-wall safety net)
//!
//! Everything here is pure, allocation-free, portable Rust: no OS calls, no
//! locks, no heap activity in `process()`. All coefficients are hardcoded
//! for `dsp_core::SAMPLE_RATE_HZ` (48kHz) since the pipeline never resamples.

use dsp_core::Stage;

const FS: f32 = dsp_core::SAMPLE_RATE_HZ as f32;

#[inline(always)]
fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

// ---------------------------------------------------------------------
// Biquad (RBJ "Audio EQ Cookbook" forms), Direct Form I, f32 state.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn from_coeffs(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        let inv_a0 = 1.0 / a0;
        Self {
            b0: b0 * inv_a0,
            b1: b1 * inv_a0,
            b2: b2 * inv_a0,
            a1: a1 * inv_a0,
            a2: a2 * inv_a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn highpass(freq_hz: f32, q: f32, fs: f32) -> Self {
        let w0 = 2.0 * core::f32::consts::PI * freq_hz / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        Self::from_coeffs(b0, b1, b2, a0, a1, a2)
    }

    fn lowpass(freq_hz: f32, q: f32, fs: f32) -> Self {
        let w0 = 2.0 * core::f32::consts::PI * freq_hz / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        Self::from_coeffs(b0, b1, b2, a0, a1, a2)
    }

    fn low_shelf(freq_hz: f32, q: f32, gain_db: f32, fs: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * core::f32::consts::PI * freq_hz / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let sqrt_a = a.sqrt();

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha;
        Self::from_coeffs(b0, b1, b2, a0, a1, a2)
    }

    fn high_shelf(freq_hz: f32, q: f32, gain_db: f32, fs: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * core::f32::consts::PI * freq_hz / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let sqrt_a = a.sqrt();

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha;
        Self::from_coeffs(b0, b1, b2, a0, a1, a2)
    }

    fn peaking(freq_hz: f32, q: f32, gain_db: f32, fs: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * core::f32::consts::PI * freq_hz / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;
        Self::from_coeffs(b0, b1, b2, a0, a1, a2)
    }

    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

// ---------------------------------------------------------------------
// Envelope follower: one-pole peak detector with separate attack/release.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct EnvFollower {
    env: f32,
    attack_coef: f32,
    release_coef: f32,
}

impl EnvFollower {
    fn new(attack_ms: f32, release_ms: f32, fs: f32) -> Self {
        // One-pole time constant: coef = exp(-1 / (time_s * fs))
        let attack_coef = (-1.0 / (attack_ms.max(0.001) * 0.001 * fs)).exp();
        let release_coef = (-1.0 / (release_ms.max(0.001) * 0.001 * fs)).exp();
        Self {
            env: 0.0,
            attack_coef,
            release_coef,
        }
    }

    #[inline(always)]
    fn process(&mut self, rectified_in: f32) -> f32 {
        let coef = if rectified_in > self.env {
            self.attack_coef
        } else {
            self.release_coef
        };
        self.env = coef * self.env + (1.0 - coef) * rectified_in;
        self.env
    }
}

// ---------------------------------------------------------------------
// Compressor: envelope-follower-driven gain reduction above a threshold.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Compressor {
    env: EnvFollower,
    threshold_db: f32,
    ratio: f32,
    // Linear makeup gain, applied as a separate multiply *after* the
    // ratio-based gain reduction (matching the reference: `x * gain *
    // makeup_gain`, not folded into a combined dB sum before converting
    // to linear).
    makeup_gain: f32,
}

impl Compressor {
    fn new(threshold_db: f32, ratio: f32, makeup_db: f32, attack_ms: f32, release_ms: f32, fs: f32) -> Self {
        Self {
            env: EnvFollower::new(attack_ms, release_ms, fs),
            threshold_db,
            ratio: ratio.max(1.0),
            makeup_gain: db_to_lin(makeup_db),
        }
    }

    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 {
        let level = self.env.process(x.abs()).max(1.0e-9);
        let level_db = 20.0 * level.log10();
        let gain_db = if level_db > self.threshold_db {
            let over = level_db - self.threshold_db;
            (self.threshold_db + over / self.ratio - level_db).min(0.0)
        } else {
            0.0
        };
        let gain_reduction = db_to_lin(gain_db);
        x * gain_reduction * self.makeup_gain
    }
}

// ---------------------------------------------------------------------
// De-esser: lowpass-complement split, matching the reference exactly.
// A single lowpass biquad at the crossover frequency produces the "safe"
// low band; whatever remains (`sibilant = x - low`) is treated as the
// sibilance band. An envelope follower on the sibilant band's own level
// drives a dynamic gain that is applied *only* to that remainder before
// recombination: `output = low + sibilant * gain`. Fixed 2ms attack /
// 40ms release regardless of preset. A single biquad lowpass isn't a
// perfectly complementary filter (some phase interaction at the
// crossover) — an accepted approximation, matching the reference's own
// note on this point.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct DeEsser {
    lowpass: Biquad,
    env: EnvFollower,
    threshold_db: f32,
    ratio: f32,
}

impl DeEsser {
    fn new(crossover_hz: f32, threshold_db: f32, ratio: f32, fs: f32) -> Self {
        Self {
            lowpass: Biquad::lowpass(crossover_hz, core::f32::consts::FRAC_1_SQRT_2, fs),
            env: EnvFollower::new(2.0, 40.0, fs),
            threshold_db,
            ratio: ratio.max(1.0),
        }
    }

    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 {
        let low = self.lowpass.process(x);
        let sibilant = x - low;
        let level = self.env.process(sibilant.abs()).max(1.0e-9);
        let level_db = 20.0 * level.log10();
        let gain_db = if level_db > self.threshold_db {
            (self.threshold_db + (level_db - self.threshold_db) / self.ratio - level_db).min(0.0)
        } else {
            0.0
        };
        let gain = db_to_lin(gain_db);
        low + sibilant * gain
    }
}

// ---------------------------------------------------------------------
// Limiter: ceiling is specified/stored as dBFS and converted to a linear
// value internally (`10^(db/20)`), matching the reference. Gain reduction
// is computed sample-by-sample from a target gain (`ceiling / |x|` when
// over, else unity): an over is applied with instant attack (the gain
// never exceeds the shrinking target, so a sample can never exceed the
// ceiling), while recovery back toward unity gain is smoothed by a fixed
// 50ms release, regardless of preset. A final clamp is a belt-and-braces
// safety net.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Limiter {
    ceiling: f32, // linear
    release_coeff: f32,
    gain: f32,
}

impl Limiter {
    fn new(ceiling_db: f32, fs: f32) -> Self {
        let release_ms = 50.0;
        Self {
            ceiling: db_to_lin(ceiling_db),
            release_coeff: (-1.0 / (release_ms * 0.001 * fs)).exp(),
            gain: 1.0,
        }
    }

    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 {
        let abs_x = x.abs();
        let target_gain = if abs_x > self.ceiling { self.ceiling / abs_x } else { 1.0 };
        self.gain = if target_gain < self.gain {
            target_gain // instant attack: never let a sample exceed the ceiling
        } else {
            self.release_coeff * self.gain + (1.0 - self.release_coeff) * target_gain
        };
        (x * self.gain).clamp(-self.ceiling, self.ceiling)
    }
}

// ---------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------

/// Progressively stronger processing presets for [`StudioStage`].
///
/// `Off` is *not* the bypass mechanism (that's `dsp_core::StageToggle`,
/// applied externally by the pipeline); it is a real, still-running
/// preset with the mildest possible internal parameters (0dB EQ, 1:1
/// compression, high de-esser threshold), so constructing a `StudioStage`
/// with `Off` and running it is close to a no-op but not a special case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StudioPreset {
    Off,
    Natural,
    Balanced,
    Strong,
}

/// Preset DSP parameters per profile, matching the reference Linux
/// (ClearNAI) `preset_params()` in `chain.rs` exactly (values, not
/// ear-adjacent approximations). Low shelf is fixed at 150Hz and high
/// shelf fixed at 8000Hz across every preset — only their gains change.
struct PresetParams {
    hpf_hz: f32,
    low_shelf_db: f32,
    mid_peak_hz: f32,
    mid_peak_q: f32,
    mid_peak_db: f32,
    high_shelf_db: f32,
    comp_threshold_db: f32,
    comp_ratio: f32,
    comp_attack_ms: f32,
    comp_release_ms: f32,
    comp_makeup_db: f32,
    de_esser_enabled: bool,
    de_esser_threshold_db: f32,
    de_esser_ratio: f32,
    limiter_ceiling_db: f32,
}

/// Low/high shelf corner frequencies are fixed across every preset.
const LOW_SHELF_HZ: f32 = 150.0;
const HIGH_SHELF_HZ: f32 = 8000.0;
/// De-esser crossover frequency, fixed whenever the de-esser is enabled.
const DEESSER_CROSSOVER_HZ: f32 = 6000.0;

impl StudioPreset {
    fn params(self) -> PresetParams {
        match self {
            StudioPreset::Off => PresetParams {
                hpf_hz: 20.0,
                low_shelf_db: 0.0,
                mid_peak_hz: 2500.0,
                mid_peak_q: 1.0,
                mid_peak_db: 0.0,
                high_shelf_db: 0.0,
                comp_threshold_db: 0.0,
                comp_ratio: 1.0,
                comp_attack_ms: 10.0,
                comp_release_ms: 100.0,
                comp_makeup_db: 0.0,
                de_esser_enabled: false,
                de_esser_threshold_db: 0.0,
                de_esser_ratio: 1.0,
                limiter_ceiling_db: -0.1,
            },
            StudioPreset::Natural => PresetParams {
                hpf_hz: 80.0,
                low_shelf_db: 0.0,
                mid_peak_hz: 2500.0,
                mid_peak_q: 1.0,
                mid_peak_db: 0.5,
                high_shelf_db: 1.0,
                comp_threshold_db: -18.0,
                comp_ratio: 1.5,
                comp_attack_ms: 10.0,
                comp_release_ms: 100.0,
                comp_makeup_db: 1.0,
                de_esser_enabled: false,
                de_esser_threshold_db: -22.0,
                de_esser_ratio: 3.0,
                limiter_ceiling_db: -1.0,
            },
            StudioPreset::Balanced => PresetParams {
                hpf_hz: 90.0,
                low_shelf_db: -1.0,
                mid_peak_hz: 3000.0,
                mid_peak_q: 1.0,
                mid_peak_db: 1.0,
                high_shelf_db: 2.0,
                comp_threshold_db: -20.0,
                comp_ratio: 2.5,
                comp_attack_ms: 8.0,
                comp_release_ms: 80.0,
                comp_makeup_db: 2.0,
                de_esser_enabled: true,
                de_esser_threshold_db: -22.0,
                de_esser_ratio: 3.0,
                limiter_ceiling_db: -1.0,
            },
            StudioPreset::Strong => PresetParams {
                hpf_hz: 100.0,
                low_shelf_db: -2.0,
                mid_peak_hz: 3000.0,
                mid_peak_q: 1.2,
                mid_peak_db: 2.0,
                high_shelf_db: 3.0,
                comp_threshold_db: -24.0,
                comp_ratio: 4.0,
                comp_attack_ms: 5.0,
                comp_release_ms: 60.0,
                comp_makeup_db: 3.0,
                de_esser_enabled: true,
                de_esser_threshold_db: -20.0,
                de_esser_ratio: 4.0,
                limiter_ceiling_db: -0.5,
            },
        }
    }
}

// ---------------------------------------------------------------------
// StudioStage
// ---------------------------------------------------------------------

/// Hand-rolled EQ / compressor / de-esser / limiter chain for a single
/// mono voice signal, matching the reference Linux (ClearNAI) "Studio"
/// stage. Allocation-free per `process()` call: all filter/envelope state
/// lives inline in the struct.
pub struct StudioStage {
    hpf: Biquad,
    low_shelf: Biquad,
    mid_peak: Biquad,
    high_shelf: Biquad,
    compressor: Compressor,
    // `None` whenever the preset's de-esser is disabled (Off, Natural),
    // matching the reference's `Option<DeEsser>` — not merely a
    // high-threshold no-op de-esser.
    deesser: Option<DeEsser>,
    limiter: Limiter,
}

impl StudioStage {
    pub fn new(preset: StudioPreset) -> Self {
        let p = preset.params();
        let hp_q = core::f32::consts::FRAC_1_SQRT_2; // Butterworth Q, ~0.7071
        Self {
            hpf: Biquad::highpass(p.hpf_hz, hp_q, FS),
            low_shelf: Biquad::low_shelf(LOW_SHELF_HZ, hp_q, p.low_shelf_db, FS),
            mid_peak: Biquad::peaking(p.mid_peak_hz, p.mid_peak_q, p.mid_peak_db, FS),
            high_shelf: Biquad::high_shelf(HIGH_SHELF_HZ, hp_q, p.high_shelf_db, FS),
            compressor: Compressor::new(
                p.comp_threshold_db,
                p.comp_ratio,
                p.comp_makeup_db,
                p.comp_attack_ms,
                p.comp_release_ms,
                FS,
            ),
            deesser: p.de_esser_enabled.then(|| {
                DeEsser::new(DEESSER_CROSSOVER_HZ, p.de_esser_threshold_db, p.de_esser_ratio, FS)
            }),
            limiter: Limiter::new(p.limiter_ceiling_db, FS),
        }
    }

    #[inline(always)]
    fn process_sample(&mut self, x: f32) -> f32 {
        let y = self.hpf.process(x);
        let y = self.low_shelf.process(y);
        let y = self.mid_peak.process(y);
        let y = self.high_shelf.process(y);
        let y = self.compressor.process(y);
        let y = match self.deesser.as_mut() {
            Some(deesser) => deesser.process(y),
            None => y,
        };
        self.limiter.process(y)
    }
}

impl Stage for StudioStage {
    fn name(&self) -> &'static str {
        "studio"
    }

    fn process(&mut self, frame: &mut [f32]) {
        for s in frame.iter_mut() {
            *s = self.process_sample(*s);
        }
    }
}

impl Default for StudioStage {
    fn default() -> Self {
        Self::new(StudioPreset::Balanced)
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq_hz: f32, amp: f32, n: usize, fs: f32, phase0: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (phase0 + 2.0 * core::f32::consts::PI * freq_hz * (i as f32) / fs).sin())
            .collect()
    }

    fn rms(xs: &[f32]) -> f32 {
        (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
    }

    fn peak(xs: &[f32]) -> f32 {
        xs.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    // ---- Biquad HPF ----

    #[test]
    fn hpf_attenuates_far_below_cutoff_much_more_than_far_above() {
        let n = 48_000usize * 2; // 2s, plenty of settling time
        let amp = 0.5;
        let low = sine(30.0, amp, n, FS, 0.0); // well below ~90Hz cutoff
        let high = sine(2000.0, amp, n, FS, 0.0); // well above cutoff

        let mut hpf_low = Biquad::highpass(90.0, core::f32::consts::FRAC_1_SQRT_2, FS);
        let mut hpf_high = Biquad::highpass(90.0, core::f32::consts::FRAC_1_SQRT_2, FS);

        let out_low: Vec<f32> = low.iter().map(|&x| hpf_low.process(x)).collect();
        let out_high: Vec<f32> = high.iter().map(|&x| hpf_high.process(x)).collect();

        // Measure steady-state RMS (skip the first 10% for filter settling).
        let settle = n / 10;
        let rms_low = rms(&out_low[settle..]);
        let rms_high = rms(&out_high[settle..]);
        let rms_in = amp / std::f32::consts::SQRT_2;

        // 30Hz should be attenuated to well under half its input RMS.
        assert!(
            rms_low < rms_in * 0.5,
            "expected strong attenuation at 30Hz, got rms_low={rms_low} rms_in={rms_in}"
        );
        // 2kHz should pass through nearly unattenuated.
        assert!(
            rms_high > rms_in * 0.9,
            "expected near-unity passband at 2kHz, got rms_high={rms_high} rms_in={rms_in}"
        );
        // And the low frequency must be measurably more attenuated than the high one.
        assert!(rms_low < rms_high * 0.5);
    }

    // ---- Compressor ----

    #[test]
    fn compressor_reduces_loud_tone_peak_more_than_quiet_tone() {
        let n = 48_000usize; // 1s
        let loud_in = sine(300.0, 0.9, n, FS, 0.0);
        let quiet_in = sine(300.0, 0.05, n, FS, 0.0);

        let mut comp_loud = Compressor::new(-20.0, 4.0, 0.0, 5.0, 100.0, FS);
        let mut comp_quiet = Compressor::new(-20.0, 4.0, 0.0, 5.0, 100.0, FS);

        let loud_out: Vec<f32> = loud_in.iter().map(|&x| comp_loud.process(x)).collect();
        let quiet_out: Vec<f32> = quiet_in.iter().map(|&x| comp_quiet.process(x)).collect();

        let settle = n / 4; // let the envelope settle
        let loud_ratio = peak(&loud_out[settle..]) / peak(&loud_in[settle..]);
        let quiet_ratio = peak(&quiet_out[settle..]) / peak(&quiet_in[settle..]);

        // The loud, above-threshold tone must be reduced measurably.
        assert!(
            loud_ratio < 0.85,
            "expected compression of loud tone, got output/input peak ratio={loud_ratio}"
        );
        // The quiet, below-threshold tone must pass through essentially unity.
        assert!(
            quiet_ratio > 0.95 && quiet_ratio < 1.05,
            "expected near-unity gain for quiet tone, got ratio={quiet_ratio}"
        );
        // And the loud tone's gain reduction must clearly exceed the quiet tone's.
        assert!(loud_ratio < quiet_ratio - 0.1);
    }

    // ---- De-esser ----
    //
    // Architecture: `low = lowpass(x)`, `sibilant = x - low`, a dynamic
    // gain computed from the sibilant band's own envelope is applied only
    // to `sibilant`, then added back: `output = low + sibilant * gain`.
    // This is a lowpass-complement split, not bandpass isolation.

    #[test]
    fn deesser_attenuates_sibilant_band_more_than_low_band() {
        let n = 48_000usize; // 1s
        let amp = 0.6;
        let sibilant_in = sine(7000.0, amp, n, FS, 0.0); // above the 6kHz crossover
        let low_in = sine(300.0, amp, n, FS, 0.0); // well below the crossover

        let mut de_sib = DeEsser::new(6000.0, -30.0, 4.0, FS);
        let mut de_low = DeEsser::new(6000.0, -30.0, 4.0, FS);

        let sib_out: Vec<f32> = sibilant_in.iter().map(|&x| de_sib.process(x)).collect();
        let low_out: Vec<f32> = low_in.iter().map(|&x| de_low.process(x)).collect();

        let settle = n / 4;
        let sib_ratio = rms(&sib_out[settle..]) / rms(&sibilant_in[settle..]);
        let low_ratio = rms(&low_out[settle..]) / rms(&low_in[settle..]);

        assert!(
            sib_ratio < 0.7,
            "expected strong attenuation in sibilant band, got ratio={sib_ratio}"
        );
        assert!(
            low_ratio > 0.9,
            "expected low band to pass through mostly unaffected, got ratio={low_ratio}"
        );
    }

    #[test]
    fn deesser_lowpass_complement_leaves_out_of_band_audio_essentially_unaffected() {
        // A tone well below the 6kHz crossover should travel almost
        // entirely through the `low` path (`low = lowpass(x)`) with the
        // sibilant remainder (`x - low`) close to zero, so even a very
        // low de-esser threshold/high ratio (aggressive gain reduction on
        // the sibilant band) barely touches it — this is the defining
        // behavior of a lowpass-complement split vs. bandpass isolation
        // (which would instead attenuate a whole resonant band around the
        // center frequency, including any leakage from low content).
        let n = 48_000usize;
        let amp = 0.7;
        let low_in = sine(200.0, amp, n, FS, 0.0);

        // Aggressive de-esser settings: low threshold, high ratio.
        let mut de = DeEsser::new(6000.0, -60.0, 10.0, FS);
        let out: Vec<f32> = low_in.iter().map(|&x| de.process(x)).collect();

        let settle = n / 4;
        let ratio = rms(&out[settle..]) / rms(&low_in[settle..]);
        assert!(
            ratio > 0.95 && ratio < 1.05,
            "expected out-of-band audio to pass through the low path essentially unaffected, got ratio={ratio}"
        );
    }

    // ---- Limiter ----

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let ceiling_db = -1.0;
        let ceiling = db_to_lin(ceiling_db);
        let mut lim = Limiter::new(ceiling_db, FS);

        // Deliberately over-driven signal (well above 1.0 pre-limiter),
        // including abrupt transients the envelope can't fully predict.
        let n = 48_000usize;
        let mut xs = sine(500.0, 3.0, n, FS, 0.0);
        // Inject some hard transient spikes.
        for i in (0..n).step_by(1000) {
            xs[i] = 5.0;
            if i + 1 < n {
                xs[i + 1] = -5.0;
            }
        }

        let eps = 1.0e-4;
        for &x in &xs {
            let y = lim.process(x);
            assert!(
                y.abs() <= ceiling + eps,
                "limiter let a sample through above ceiling: {y} > {ceiling}"
            );
        }
    }

    // ---- Full chain / presets ----

    #[test]
    fn studio_stage_processes_frame_without_nan_or_inf() {
        let mut stage = StudioStage::new(StudioPreset::Strong);
        let mut frame = sine(440.0, 0.8, dsp_core::FRAME_SAMPLES, FS, 0.0);
        for _ in 0..50 {
            stage.process(&mut frame);
            for &s in frame.iter() {
                assert!(s.is_finite(), "non-finite sample produced: {s}");
            }
        }
    }

    #[test]
    fn studio_stage_full_chain_respects_limiter_ceiling() {
        let preset = StudioPreset::Strong;
        // Limiter ceiling is specified/stored as dBFS (Strong = -0.5dB);
        // convert to linear the same way `Limiter::new` does internally.
        let ceiling = db_to_lin(preset.params().limiter_ceiling_db);
        let mut stage = StudioStage::new(preset);

        let n_frames = 200;
        let mut xs = sine(500.0, 3.0, n_frames * dsp_core::FRAME_SAMPLES, FS, 0.0);
        for i in (0..xs.len()).step_by(777) {
            xs[i] = 4.0;
        }

        let eps = 1.0e-3;
        for chunk in xs.chunks_mut(dsp_core::FRAME_SAMPLES) {
            stage.process(chunk);
            for &s in chunk.iter() {
                assert!(s.abs() <= ceiling + eps, "sample {s} exceeded ceiling {ceiling}");
            }
        }
    }

    #[test]
    fn preset_off_is_near_unity_for_midband_signal() {
        // Use a mid-band frequency well clear of the HPF's cutoff so the
        // only expected deviation from "off" is the (0dB) EQ/comp/deesser
        // stages, which should be effectively transparent.
        let mut stage = StudioStage::new(StudioPreset::Off);
        let n = dsp_core::FRAME_SAMPLES * 20;
        let input = sine(1000.0, 0.3, n, FS, 0.0);
        let mut output = input.clone();
        for chunk in output.chunks_mut(dsp_core::FRAME_SAMPLES) {
            stage.process(chunk);
        }

        let settle = n / 4;
        let ratio = rms(&output[settle..]) / rms(&input[settle..]);
        assert!(
            ratio > 0.97 && ratio < 1.03,
            "expected near-unity gain for Off preset, got ratio={ratio}"
        );
    }

    #[test]
    fn presets_apply_progressively_stronger_compression() {
        // A loud, sustained tone should be gain-reduced progressively more
        // as the preset gets stronger (Off <= Natural <= Balanced <= Strong).
        let n = 48_000usize;
        let amp = 0.9;

        let mut ratios = Vec::new();
        for preset in [
            StudioPreset::Off,
            StudioPreset::Natural,
            StudioPreset::Balanced,
            StudioPreset::Strong,
        ] {
            let mut stage = StudioStage::new(preset);
            let input = sine(500.0, amp, n, FS, 0.0);
            let mut output = input.clone();
            for chunk in output.chunks_mut(dsp_core::FRAME_SAMPLES) {
                stage.process(chunk);
            }
            let settle = n / 4;
            ratios.push(peak(&output[settle..]) / peak(&input[settle..]));
        }

        // Off should be closest to unity gain among the four.
        assert!(ratios[0] > ratios[1] - 0.05, "Off should not compress more than Natural: {ratios:?}");
        // Strong should show clearly more gain reduction than Off.
        assert!(
            ratios[3] < ratios[0] - 0.1,
            "expected Strong to reduce peak markedly more than Off: {ratios:?}"
        );
    }

    #[test]
    fn deesser_enabled_flag_matches_reference_per_preset() {
        // Off and Natural ship with the de-esser disabled; Balanced and
        // Strong ship with it enabled. This mirrors `preset_params()` in
        // the reference `chain.rs`, not a "very high threshold" stand-in.
        assert!(!StudioPreset::Off.params().de_esser_enabled);
        assert!(!StudioPreset::Natural.params().de_esser_enabled);
        assert!(StudioPreset::Balanced.params().de_esser_enabled);
        assert!(StudioPreset::Strong.params().de_esser_enabled);

        assert!(StudioStage::new(StudioPreset::Off).deesser.is_none());
        assert!(StudioStage::new(StudioPreset::Natural).deesser.is_none());
        assert!(StudioStage::new(StudioPreset::Balanced).deesser.is_some());
        assert!(StudioStage::new(StudioPreset::Strong).deesser.is_some());
    }

    #[test]
    fn preset_values_match_reference_table() {
        // Spot-check the corrected numbers against the reference's
        // `preset_params()` (chain.rs:58-129) so a future regression here
        // is caught directly, not just inferred from downstream behavior.
        let off = StudioPreset::Off.params();
        assert_eq!(off.hpf_hz, 20.0);
        assert_eq!(off.limiter_ceiling_db, -0.1);

        let natural = StudioPreset::Natural.params();
        assert_eq!(natural.hpf_hz, 80.0);
        assert_eq!(natural.mid_peak_db, 0.5);
        assert_eq!(natural.high_shelf_db, 1.0);
        assert_eq!(natural.comp_threshold_db, -18.0);
        assert_eq!(natural.comp_ratio, 1.5);
        assert_eq!(natural.comp_makeup_db, 1.0);
        assert_eq!(natural.limiter_ceiling_db, -1.0);

        let balanced = StudioPreset::Balanced.params();
        assert_eq!(balanced.hpf_hz, 90.0);
        assert_eq!(balanced.low_shelf_db, -1.0);
        assert_eq!(balanced.mid_peak_hz, 3000.0);
        assert_eq!(balanced.mid_peak_db, 1.0);
        assert_eq!(balanced.high_shelf_db, 2.0);
        assert_eq!(balanced.comp_threshold_db, -20.0);
        assert_eq!(balanced.comp_ratio, 2.5);
        assert_eq!(balanced.comp_attack_ms, 8.0);
        assert_eq!(balanced.comp_release_ms, 80.0);
        assert_eq!(balanced.comp_makeup_db, 2.0);
        assert_eq!(balanced.de_esser_threshold_db, -22.0);
        assert_eq!(balanced.de_esser_ratio, 3.0);
        assert_eq!(balanced.limiter_ceiling_db, -1.0);

        let strong = StudioPreset::Strong.params();
        assert_eq!(strong.hpf_hz, 100.0);
        assert_eq!(strong.low_shelf_db, -2.0);
        assert_eq!(strong.mid_peak_hz, 3000.0);
        assert_eq!(strong.mid_peak_q, 1.2);
        assert_eq!(strong.mid_peak_db, 2.0);
        assert_eq!(strong.high_shelf_db, 3.0);
        assert_eq!(strong.comp_threshold_db, -24.0);
        assert_eq!(strong.comp_ratio, 4.0);
        assert_eq!(strong.comp_attack_ms, 5.0);
        assert_eq!(strong.comp_release_ms, 60.0);
        assert_eq!(strong.comp_makeup_db, 3.0);
        assert_eq!(strong.de_esser_threshold_db, -20.0);
        assert_eq!(strong.de_esser_ratio, 4.0);
        assert_eq!(strong.limiter_ceiling_db, -0.5);
    }

    #[test]
    fn compressor_makeup_gain_is_a_separate_post_multiply() {
        // With gain reduction forced to unity (input held under threshold),
        // the only effect should be the makeup gain applied as a separate
        // linear multiply: output = x * 1.0 * makeup_linear.
        let amp = 0.01; // well under any reasonable threshold
        let makeup_db = 6.0;
        let mut comp = Compressor::new(-6.0, 4.0, makeup_db, 5.0, 100.0, FS);
        let input = sine(300.0, amp, 48_000, FS, 0.0);
        let output: Vec<f32> = input.iter().map(|&x| comp.process(x)).collect();

        let settle = 48_000 / 4;
        let ratio = rms(&output[settle..]) / rms(&input[settle..]);
        let expected = db_to_lin(makeup_db);
        assert!(
            (ratio - expected).abs() < 0.02,
            "expected below-threshold signal scaled by makeup gain alone ({expected}), got ratio={ratio}"
        );
    }
}
