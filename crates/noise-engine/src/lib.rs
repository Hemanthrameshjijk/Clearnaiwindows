//! Registry of interchangeable noise-suppression `Stage` implementations.
//!
//! Each engine is a `dsp_core::Stage`: it operates in place on exactly
//! `dsp_core::FRAME_SAMPLES` (480) mono f32 samples at
//! `dsp_core::SAMPLE_RATE_HZ` (48kHz), normalized to `[-1.0, 1.0]`, and
//! must not allocate/lock/block inside `process()`.
//!
//! The project's actual bypass mechanism is the external `StageToggle` /
//! `run_if_enabled` wrapper in `dsp-core` — a toggled-off stage is simply
//! never called. `DisabledNoiseEngine` exists in addition to that so
//! callers can still hold "no engine selected" as a concrete, uniform
//! `Box<dyn Stage>` value (e.g. as the initial/fallback registry entry)
//! without needing an `Option<Box<dyn Stage>>` everywhere.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use dsp_core::{Stage, FRAME_SAMPLES};

/// `process()` never even reads or writes the buffer — a true no-op, not
/// just an identity copy. This is intentionally *not* the primary bypass
/// path (that's `dsp_core::StageToggle`); it just needs to behave
/// correctly if it's ever called directly.
pub struct DisabledNoiseEngine;

impl Stage for DisabledNoiseEngine {
    fn name(&self) -> &'static str {
        "disabled"
    }

    fn process(&mut self, _frame: &mut [f32]) {
        // Intentionally empty: do not touch `_frame` at all.
    }
}

/// RNNoise-family noise suppression via the `nnnoiseless` crate — a pure
/// Rust reimplementation of Xiph's RNNoise (no C/libclang dependency),
/// which is exactly why it was chosen for this Windows port over an FFI
/// binding to the native C library (cross-compiling C deps to
/// x86_64-pc-windows-gnu from Linux is the kind of thing this port is
/// trying to avoid).
///
/// Verified against `nnnoiseless` 0.5.2 source
/// (https://github.com/jneem/nnnoiseless, `src/denoise.rs`,
/// `DenoiseState::process_frame`):
/// - `DenoiseState::FRAME_SIZE` is `120 << 2 = 480`, matching
///   `dsp_core::FRAME_SAMPLES` exactly (checked at construction time
///   below rather than assumed).
/// - `process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32`
///   expects **int16-amplitude-range** floats, i.e. `[-32768.0, 32767.0]`,
///   *not* `[-1.0, 1.0]` — this crate's own doc comment says so explicitly.
///   `dsp_core::Stage`'s contract is normalized `[-1.0, 1.0]` f32, so this
///   wrapper scales by 32768.0 on the way in and back down on the way out.
/// - Default (non-dev, non-optional) dependencies of `nnnoiseless` are
///   just `easyfft` and `once_cell` — pure Rust, confirmed via the
///   crates.io dependency listing for 0.5.2.
pub struct RnnoiseEngine {
    state: Box<nnnoiseless::DenoiseState<'static>>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
}

impl RnnoiseEngine {
    pub fn new() -> Result<Self> {
        if nnnoiseless::DenoiseState::FRAME_SIZE != FRAME_SAMPLES {
            return Err(anyhow!(
                "nnnoiseless::DenoiseState::FRAME_SIZE ({}) does not match dsp_core::FRAME_SAMPLES ({}); \
                 internal buffering would be required and is not implemented",
                nnnoiseless::DenoiseState::FRAME_SIZE,
                FRAME_SAMPLES
            ));
        }
        Ok(Self {
            state: nnnoiseless::DenoiseState::new(),
            scratch_in: vec![0.0; FRAME_SAMPLES],
            scratch_out: vec![0.0; FRAME_SAMPLES],
        })
    }
}

impl Default for RnnoiseEngine {
    /// Panics if `nnnoiseless`'s frame size ever stops matching
    /// `dsp_core::FRAME_SAMPLES` — prefer `RnnoiseEngine::new()` to handle
    /// that as a recoverable error instead.
    fn default() -> Self {
        Self::new().expect("nnnoiseless frame size mismatch")
    }
}

impl Stage for RnnoiseEngine {
    fn name(&self) -> &'static str {
        "rnnoise"
    }

    fn process(&mut self, frame: &mut [f32]) {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);

        for (dst, &src) in self.scratch_in.iter_mut().zip(frame.iter()) {
            *dst = src * 32768.0;
        }

        let _vad_prob = self
            .state
            .process_frame(&mut self.scratch_out, &self.scratch_in);

        for (dst, &src) in frame.iter_mut().zip(self.scratch_out.iter()) {
            *dst = (src / 32768.0).clamp(-1.0, 1.0);
        }
    }
}

/// DeepFilterNet3 noise suppression via the real `deep_filter` crate
/// (Cargo package `deep_filter`, lib target name `df`), using its `tract`
/// (pure-Rust ONNX runtime) streaming API.
///
/// Verified directly against the upstream repository
/// (https://github.com/Rikorose/DeepFilterNet):
/// - The crates.io-published `deep_filter` only goes up to `0.2.5`
///   (published 2022-07-28), which **predates** `libDF/src/tract.rs`
///   entirely (that file doesn't exist at the `v0.2.5` tag). The real
///   streaming `DfTract` API only exists in the git repo from `v0.3.0`
///   onward, so this MUST be a git dependency, not a crates.io version.
///   Pinned here to git tag `v0.5.6` (commit `978576aa8400`).
/// - Confirmed via `libDF/Cargo.toml` at that tag: package name
///   `deep_filter`, lib name `df`, and a `tract` feature that pulls in
///   `tract-core`/`tract-onnx`/`tract-pulse`/`tract-hir` (all pure Rust —
///   `tract` is a from-scratch Rust ONNX runtime, no libclang/C++ toolkit
///   needed) plus `transforms`/`logging`.
/// - Confirmed via `libDF/src/tract.rs` at that tag:
///   - `DfParams::new(tar_file: PathBuf) -> Result<Self>` loads a model
///     from a **`.tar.gz` archive** containing `enc.onnx`, `erb_dec.onnx`,
///     `df_dec.onnx`, and `config.ini` — NOT a bare `.onnx` file. This
///     wrapper's constructor is documented and typed accordingly
///     (`model_path` is the path to that tar.gz).
///   - `RuntimeParams::default_with_ch(1)` (used via `RuntimeParams::default()`)
///     configures single-channel/mono processing.
///   - `DfTract::new(dfp: DfParams, rp: &RuntimeParams) -> Result<Self>`.
///   - `DfTract::process(&mut self, noisy: ArrayView2<f32>, enh: ArrayViewMut2<f32>) -> Result<f32>`
///     operates on `[n_channels, hop_size]`-shaped buffers; `hop_size` is
///     read from the model's own `config.ini` at load time (not a crate
///     constant), so it is checked against `dsp_core::FRAME_SAMPLES` at
///     construction time here rather than assumed. DeepFilterNet3's
///     standard/published config uses `hop_size = 480` at `sr = 48000`
///     (matching this workspace's frame contract), but that is a property
///     of whichever model file is actually loaded, not something this
///     crate can guarantee in general — hence the runtime check.
/// - This module compiles against the real API (verified above) but its
///   runtime behavior (`DeepFilterEngine::new` / `process`) was **not**
///   exercised in this session's tests: doing so requires an actual
///   DeepFilterNet3 model tar.gz on disk, which is a packaging-phase
///   asset this crate doesn't bundle. See the crate-level test module for
///   what was and wasn't actually run.
pub struct DeepFilterEngine {
    tract: deep_filter::tract::DfTract,
    scratch_out: Vec<f32>,
}

// Safety: `DfTract` holds `tract`-internal state (`Rc<Tensor>`,
// `Box<dyn OpState>`) that is not `Send` in the general case, because
// `tract`'s plan-execution state isn't designed to be handed between
// threads mid-use. That's not how this crate uses it: a `DeepFilterEngine`
// is constructed once and then driven exclusively by a single realtime
// audio thread for its entire lifetime (per `dsp_core::Stage`'s contract
// and this workspace's pipeline design), so it is never actually accessed
// concurrently from multiple threads — only ever *transferred* between
// threads while idle (e.g. moved into the audio thread at startup), which
// is exactly what `Send` (as opposed to `Sync`) permits. This mirrors the
// same justification used for the equivalent engine in the reference
// Linux project.
unsafe impl Send for DeepFilterEngine {}

impl DeepFilterEngine {
    pub fn new(model_path: &Path) -> Result<Self> {
        let params = deep_filter::tract::DfParams::new(model_path.to_path_buf())
            .with_context(|| format!("loading DeepFilterNet model from {}", model_path.display()))?;
        let runtime_params = deep_filter::tract::RuntimeParams::default(); // mono, default_with_ch(1)
        let tract = deep_filter::tract::DfTract::new(params, &runtime_params)
            .map_err(|e| anyhow!("DfTract::new failed: {e}"))?;

        if tract.hop_size != FRAME_SAMPLES {
            return Err(anyhow!(
                "DeepFilterNet model hop_size ({}) does not match dsp_core::FRAME_SAMPLES ({}); \
                 internal buffering would be required and is not implemented",
                tract.hop_size,
                FRAME_SAMPLES
            ));
        }

        let scratch_out = vec![0.0f32; tract.hop_size];
        Ok(Self { tract, scratch_out })
    }
}

impl Stage for DeepFilterEngine {
    fn name(&self) -> &'static str {
        "deepfilternet"
    }

    fn process(&mut self, frame: &mut [f32]) {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);

        let noisy = ndarray::ArrayView2::from_shape((1, frame.len()), frame)
            .expect("frame length matches hop_size, checked at construction");
        let enh = ndarray::ArrayViewMut2::from_shape((1, self.scratch_out.len()), &mut self.scratch_out)
            .expect("scratch_out length matches hop_size");

        // `DfTract::process` must not block/allocate in steady state (the
        // model graph and all scratch tensors are preallocated in `new`);
        // it can fail (e.g. an internal tract runtime error), in which
        // case we fail safe by leaving the frame untouched rather than
        // propagating a panic into the realtime audio callback.
        if self.tract.process(noisy, enh).is_ok() {
            for (dst, &src) in frame.iter_mut().zip(self.scratch_out.iter()) {
                *dst = src.clamp(-1.0, 1.0);
            }
        }
    }
}

/// Which noise-suppression engine to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseEngineKind {
    Disabled,
    RnNoise,
    DeepFilter,
}

/// Builds a boxed `Stage` for the requested engine kind.
///
/// `deep_filter_model_path` is only consulted (and required) for
/// `NoiseEngineKind::DeepFilter` — it must point at a DeepFilterNet
/// `.tar.gz` model archive (see `DeepFilterEngine` docs above for why it's
/// a tar.gz and not a bare `.onnx` file).
pub fn build(
    kind: NoiseEngineKind,
    deep_filter_model_path: Option<&Path>,
) -> Result<Box<dyn Stage>> {
    match kind {
        NoiseEngineKind::Disabled => Ok(Box::new(DisabledNoiseEngine)),
        NoiseEngineKind::RnNoise => Ok(Box::new(RnnoiseEngine::new()?)),
        NoiseEngineKind::DeepFilter => {
            let path = deep_filter_model_path
                .ok_or_else(|| anyhow!("NoiseEngineKind::DeepFilter requires a model path"))?;
            Ok(Box::new(DeepFilterEngine::new(path)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;

    #[test]
    fn disabled_engine_leaves_frame_bit_for_bit_unchanged() {
        let mut engine = DisabledNoiseEngine;
        let original: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| (i as f32 * 0.017).sin() * 0.5)
            .collect();
        let mut frame = original.clone();
        engine.process(&mut frame);
        assert_eq!(frame, original, "DisabledNoiseEngine must not touch the buffer");
    }

    #[test]
    fn disabled_engine_handles_nan_without_touching_it() {
        // Extra proof it's a true no-op, not "copies input to output":
        // NaN in means NaN out, byte for byte, because nothing ran.
        let mut engine = DisabledNoiseEngine;
        let mut frame = vec![f32::NAN; FRAME_SAMPLES];
        engine.process(&mut frame);
        assert!(frame.iter().all(|s| s.is_nan()));
    }

    #[test]
    fn build_disabled_via_factory() {
        let mut stage = build(NoiseEngineKind::Disabled, None).expect("disabled always builds");
        assert_eq!(stage.name(), "disabled");
        let mut frame = vec![1.0f32; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert_eq!(frame, vec![1.0f32; FRAME_SAMPLES]);
    }

    #[test]
    fn build_deep_filter_without_model_path_errors() {
        let err = match build(NoiseEngineKind::DeepFilter, None) {
            Ok(_) => panic!("DeepFilter requires a model path"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("model path"));
    }

    fn synthetic_noisy_frame(seed_offset: usize) -> Vec<f32> {
        let mut rng = rand::thread_rng();
        (0..FRAME_SAMPLES)
            .map(|i| {
                let t = (i + seed_offset * FRAME_SAMPLES) as f32 / dsp_core::SAMPLE_RATE_HZ as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.4;
                let noise: f32 = rng.gen_range(-1.0..1.0) * 0.1;
                (tone + noise).clamp(-1.0, 1.0)
            })
            .collect()
    }

    #[test]
    fn rnnoise_engine_runs_on_real_frame_without_panicking_and_is_finite() {
        let mut engine = RnnoiseEngine::new().expect("nnnoiseless frame size must match");
        assert_eq!(engine.name(), "rnnoise");

        for i in 0..20 {
            let mut frame = synthetic_noisy_frame(i);
            engine.process(&mut frame);
            assert_eq!(frame.len(), FRAME_SAMPLES);
            for &s in &frame {
                assert!(s.is_finite(), "output sample must be finite, got {s}");
                assert!(
                    (-1.0..=1.0).contains(&s),
                    "output sample must stay in [-1.0, 1.0], got {s}"
                );
            }
        }
    }

    #[test]
    fn rnnoise_engine_via_factory_runs_without_panicking() {
        let mut stage = build(NoiseEngineKind::RnNoise, None).expect("rnnoise builds with no model path");
        assert_eq!(stage.name(), "rnnoise");
        let mut frame = synthetic_noisy_frame(0);
        stage.process(&mut frame);
        assert!(frame.iter().all(|s| s.is_finite()));
    }

    // No DeepFilterEngine construction/inference test: doing so requires a
    // real DeepFilterNet3 model tar.gz on disk (enc.onnx + erb_dec.onnx +
    // df_dec.onnx + config.ini), which is a packaging-phase asset not
    // bundled in this crate or available in this sandbox. What IS verified
    // by `cargo check`/`cargo test` compiling this file successfully: the
    // `DeepFilterEngine` wrapper type-checks against the real
    // `deep_filter::tract::{DfParams, RuntimeParams, DfTract}` API at the
    // pinned `v0.5.6` git tag (see module doc comment above for exactly
    // which parts of that API were read from source vs. assumed).
}

#[cfg(test)]
mod correctness_tests {
    //! Real audio-quality regression tests, not just "doesn't panic": these
    //! catch the exact failure mode reported against this pipeline (voice
    //! coming through as suppressed/distorted noise, or background noise
    //! passing through unsuppressed) by measuring actual RMS energy
    //! reduction/preservation, the same methodology `crates/aec`'s ~120x
    //! echo-reduction test uses. A deterministic xorshift-style PRNG (not
    //! `rand::thread_rng`, unlike the other tests in this file) is used for
    //! the noise signal so these thresholds never flake between runs.
    use super::*;
    use dsp_core::FRAME_SAMPLES;

    fn rms(xs: &[f32]) -> f32 {
        (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
    }

    /// Deterministic white noise generator (LCG), seeded so results are
    /// reproducible across runs/machines.
    fn deterministic_noise(seed: u32) -> impl FnMut() -> f32 {
        let mut state = seed;
        move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 8) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// Steady-state (post-warm-up) input/output RMS after running `engine`
    /// on `n_frames` frames produced by `make_frame`, averaged in energy
    /// (mean of squares, not mean of per-frame RMS) over every post-warm-up
    /// frame rather than read from a single frame - a single frame's RMS is
    /// noisy enough (especially for a random noise signal) to flip a ratio
    /// comparison between runs; averaging over ~50 frames is stable.
    fn steady_state_rms(
        engine: &mut RnnoiseEngine,
        n_frames: usize,
        warmup_frames: usize,
        mut make_frame: impl FnMut(usize) -> Vec<f32>,
    ) -> (f32, f32) {
        let mut in_sq_sum = 0.0f64;
        let mut out_sq_sum = 0.0f64;
        let mut counted = 0usize;
        for i in 0..n_frames {
            let mut frame = make_frame(i);
            let this_in_rms = rms(&frame);
            engine.process(&mut frame);
            let this_out_rms = rms(&frame);
            if i >= warmup_frames {
                in_sq_sum += (this_in_rms as f64).powi(2);
                out_sq_sum += (this_out_rms as f64).powi(2);
                counted += 1;
            }
        }
        let in_rms = (in_sq_sum / counted as f64).sqrt() as f32;
        let out_rms = (out_sq_sum / counted as f64).sqrt() as f32;
        (in_rms, out_rms)
    }

    #[test]
    fn rnnoise_measurably_reduces_steady_broadband_noise() {
        // Background/broadband noise with no voice at all - this is the
        // "instead of background [going quiet], giving noise" case: RNNoise
        // must measurably attenuate it, not pass it through unsuppressed.
        let mut engine = RnnoiseEngine::new().unwrap();
        let mut noise = deterministic_noise(12345);
        let (in_rms, out_rms) =
            steady_state_rms(&mut engine, 200, 150, |_| {
                (0..FRAME_SAMPLES).map(|_| noise() * 0.08).collect()
            });

        let ratio = out_rms / in_rms;
        assert!(
            ratio < 0.8,
            "expected noise-only input to be measurably attenuated (empirically ~0.66), got ratio={ratio} \
             (in_rms={in_rms}, out_rms={out_rms}) - if this regresses toward 1.0, noise is passing through \
             unsuppressed"
        );
    }

    #[test]
    fn rnnoise_preserves_clean_voice_tone_without_turning_it_into_noise() {
        // A clean voice-band tone with no added noise must survive close to
        // unity gain - this is the "voice was coming through as noise"
        // failure mode: if RNNoise (mis-scaled input, wrong frame size
        // handling, etc.) were mangling the signal, this ratio would
        // collapse well below 1.0 or the output would show up as
        // near-silence/garbage rather than the same tone.
        let mut engine = RnnoiseEngine::new().unwrap();
        let fs = dsp_core::SAMPLE_RATE_HZ as f32;
        let mut t = 0usize;
        let (in_rms, out_rms) = steady_state_rms(&mut engine, 200, 150, |_| {
            let frame: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|j| {
                    let tt = (t + j) as f32 / fs;
                    0.3 * (2.0 * std::f32::consts::PI * 300.0 * tt).sin()
                })
                .collect();
            t += FRAME_SAMPLES;
            frame
        });

        let ratio = out_rms / in_rms;
        assert!(
            (0.85..=1.15).contains(&ratio),
            "expected clean voice tone to pass through near unity gain (empirically ~1.00), got ratio={ratio} \
             (in_rms={in_rms}, out_rms={out_rms}) - a collapsed ratio here means voice is being suppressed/\
             distorted like noise instead of passing through"
        );
    }

    #[test]
    fn rnnoise_suppresses_noise_more_than_it_suppresses_voice() {
        // The core correctness property a "senior audio QA" style check
        // would demand: whatever RNNoise does to a signal, it must treat
        // noise and voice asymmetrically - noise measurably reduced, voice
        // measurably preserved - never the reverse.
        let mut noise_engine = RnnoiseEngine::new().unwrap();
        let mut noise = deterministic_noise(54321);
        let (noise_in, noise_out) =
            steady_state_rms(&mut noise_engine, 200, 150, |_| {
                (0..FRAME_SAMPLES).map(|_| noise() * 0.08).collect()
            });

        let mut voice_engine = RnnoiseEngine::new().unwrap();
        let fs = dsp_core::SAMPLE_RATE_HZ as f32;
        let mut t = 0usize;
        let (voice_in, voice_out) = steady_state_rms(&mut voice_engine, 200, 150, |_| {
            let frame: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|j| {
                    let tt = (t + j) as f32 / fs;
                    0.3 * (2.0 * std::f32::consts::PI * 300.0 * tt).sin()
                })
                .collect();
            t += FRAME_SAMPLES;
            frame
        });

        let noise_ratio = noise_out / noise_in;
        let voice_ratio = voice_out / voice_in;
        // Margin kept modest (not the ~0.35 gap seen with the other test's
        // seed): different noise realizations converge to different
        // steady-state suppression levels (empirically observed between
        // ~0.66 and ~0.87 across seeds) since a synthetic LCG isn't
        // spectrally identical to the real noise types RNNoise is trained
        // on, but noise must still be attenuated measurably more than a
        // clean tone in every case.
        assert!(
            noise_ratio < voice_ratio - 0.05,
            "expected noise attenuation to be clearly stronger than any voice attenuation, got \
             noise_ratio={noise_ratio} voice_ratio={voice_ratio}"
        );
    }
}
