# Windows vs Linux audio/DSP contract parity

This compares this repo (Windows, Rust + WASAPI) against the real Linux
reference implementation found on this development machine at
`/home/lemon-peak/krsipCl` (Rust + PipeWire — crates `clearnai-core`,
`clearnai-pipewire`, `clearnai-bvc`, `clearnai-studio`, `clearnai-rnnoise-sys`,
`clearnai-hush-sys`, `clearnai-cli`). It is **not** a second copy of that
project inside this repo — it lives entirely outside `clearnWindows/` and is
only used here as a read-only source of truth to verify against, per the
project's actual constraint (no Linux build/audio hardware in this dev
environment, only source-level comparison).

Everything below was verified by reading both codebases side by side, not
assumed from either side's own comments. File:line references are into each
repo as it stood on 2026-09-15.

## 1. Sample rate

| | Linux | Windows |
|---|---|---|
| Value | `48_000` — `clearnai-pipewire/src/format.rs:9`, `clearnai-cli/src/main.rs:22` | `48_000` — `crates/dsp-core/src/lib.rs:108` |

**MATCH.** Both also run a separate 16kHz-native model internally for BVC/Hush
(the DLL's own input-rate handling — see §6), which is consistent on both
sides, not a discrepancy.

## 2. Frame size

| | Linux | Windows |
|---|---|---|
| Value | `480` samples (10ms @ 48kHz), asserted throughout: `clearnai-bvc/src/hush.rs:25` (`EXPECTED_FRAME_SIZE`), `clearnai-rnnoise-sys/src/lib.rs:45` (asserts `rnnoise_get_frame_size() == 480`), PipeWire node latency fixed to `480/48000` in `capture.rs:63`, `speaker_output.rs:46`, `virtual_mic.rs:65` | `480` — `crates/dsp-core/src/lib.rs:107`, with a runtime assertion that `nnnoiseless::DenoiseState::FRAME_SIZE` also equals 480 (`crates/noise-engine/src/lib.rs:65-71`) |

**MATCH.**

## 3. Channels and sample format

| | Linux | Windows |
|---|---|---|
| Channels | mono, explicit `SPA_AUDIO_CHANNEL_MONO` (`clearnai-pipewire/src/format.rs:13-20`) | mono (`crates/audio-io/src/lib.rs:131`) |
| Format | interleaved f32, F32LE, `-1.0..=1.0` | f32, `-1.0..=1.0` (`dsp_core::Stage` contract, `crates/dsp-core/src/lib.rs:79-90`) |

**MATCH.** The mechanism differs (PipeWire negotiates the node format
explicitly; WASAPI shared-mode does the conversion at the engine boundary via
`engine_wave_format()`), which is an inherent, expected platform difference —
the two audio APIs don't work the same way — not a drift in the contract
itself.

## 4. Noise-cancellation engine (RNNoise / DeepFilterNet)

| | Linux | Windows |
|---|---|---|
| Default | `rnnoise` (`clearnai-cli/src/main.rs:65`) | `RnNoise` (`app/src/engine.rs:416,419`) |
| RNNoise impl | vendored native C library via FFI (`clearnai-rnnoise-sys`, Xiph's fork) | `nnnoiseless`, a **pure-Rust reimplementation** (`crates/noise-engine/src/lib.rs:36-56`) |
| DeepFilterNet | `deep_filter`/`tract`, same crate family | `deep_filter`/`tract`, pinned to the same upstream tag |
| Int16-range scaling constant | `32768.0` (`clearnai-noise/src/rnnoise.rs:113,125`) | `32768.0` (`crates/noise-engine/src/lib.rs:99,107`) |

**MISMATCH, intentional and documented**: the RNNoise backend is a pure-Rust
port instead of the vendored C library, specifically to avoid cross-compiling
a C toolchain to Windows/MSVC (explained in `noise-engine/src/lib.rs:38-41`).
Same algorithm family, same frame size, same default engine choice, same
scaling convention — the only difference is native-C vs pure-Rust
*implementation* of the identical RNNoise algorithm, not a different
algorithm or different defaults.

## 5. BVC / Hush

| | Linux | Windows |
|---|---|---|
| C API | `weya_nc_model_load_from_path`, `weya_nc_session_create(model, input_sr, atten_lim_db)`, `weya_nc_get_frame_length`, `weya_nc_get_sample_rate`, `weya_nc_get_input_sample_rate`, `weya_nc_process_frame`, `weya_nc_reset` (`clearnai-hush-sys/src/lib.rs`) | Identical symbol set (`crates/bvc-hush/src/ffi.rs`) |
| Default `atten_lim_db` | `30.0` (`clearnai-bvc/src/registry.rs:19`) | `30.0` (`crates/bvc-hush/src/lib.rs:127`, `DEFAULT_ATTEN_LIM_DB`) |
| Frame-length mismatch handling | hard-fails at init if `weya_nc_get_frame_length() != 480` (`hush.rs:81-92`) | tolerates a mismatch via `FrameAdapter` (re-chunks/buffers between the host's 480-sample frame and whatever the DLL actually reports) |

**MATCH** on the API and the 30.0dB default — this repo's own comment
tracing that value back to the Linux CLI's `--bvc-atten-lim-db` default (and
noting an earlier wrong guess of 100.0 was corrected) is confirmed accurate
against the real Linux source, not just self-reported.

**Documented divergence, not drift**, on frame-length handling: Windows is
*more* defensive (adapts instead of refusing to run), which its own comments
already flag as unverified without the real DLL's frame length being queried
on real hardware (`bvc-hush/src/lib.rs:36-49`, `frame_adapter.rs:2-20`). This
is the one place worth confirming empirically: if the real `weya_nc.dll`'s
native frame length at 48kHz genuinely is 480 (matching Linux's hard
assumption), the adapter is a no-op passthrough and there's no difference in
practice; if it's ever something else, Windows would introduce a few frames
of startup latency/silence that Linux would instead refuse to run at all.

## 6. Studio DSP (EQ / compressor / de-esser / limiter)

Every parameter in `clearnai-studio/src/chain.rs::preset_params()`
(lines 58-129) was compared field by field against
`crates/studio-dsp/src/lib.rs::StudioPreset::params()` (lines 356-427) for
all four presets (Off/Natural/Balanced/Strong): HPF cutoff, shelf gains/
frequencies/Q, compressor threshold/ratio/attack/release/makeup gain,
de-esser enabled flag/threshold/ratio, limiter ceiling.

**MATCH — every value is identical**, including fixed corner frequencies
(150Hz low shelf, 8000Hz high shelf, 6000Hz de-esser crossover). Windows has
a dedicated regression test pinning these against the reference
(`studio-dsp/src/lib.rs::preset_values_match_reference_table`, lines
794-842). Re-verified now against the real Linux source: still true, no
drift since this repo's own comment claimed the correction was made.

## 7. AEC (echo cancellation)

Linux has **no echo canceller at all** — an exhaustive search of every
`krsipCl` crate for "echo"/"aec"/"AEC" and for a `webrtc` dependency in any
`Cargo.toml` found nothing.

Windows adds a hand-written NLMS (Normalized Least-Mean-Squares) adaptive
filter (`crates/aec`, 1024 taps, ~21.3ms modeled echo path), **defaulting
OFF** (`DisabledAec`, `aec/src/lib.rs:69-71`). Its module doc extensively
documents that the "real" option (`webrtc-audio-processing`'s bundled AEC3)
was attempted and failed to build in this environment (bindgen/clang
resource-header issue) and that the NLMS filter is the accepted fallback.

**This is a Windows-only addition, not a broken port of a Linux feature** —
there is nothing on the Linux side to diverge from, and since it ships off by
default, it changes no default behavior relative to Linux. Documented, not
silent.

## 8. Buffering granularity

Linux's realtime path is strictly single-frame (480 samples) end to end:
PipeWire buffers are fixed at 480 (`capture.rs:120`, `speaker_output.rs:88`,
`virtual_mic.rs:104`), and RNNoise/Hush/Studio each process exactly one
480-sample frame per call. The one multi-frame buffer anywhere in that
codebase is a 4-second window for the **transcript** feature
(`clearnai-cli/src/main.rs:414-421`), explicitly decoupled from the realtime
mic path on a separate thread — not part of the noise-cancellation contract.

Windows matches this: `dsp_core::make_ring_buffer` sizes ring buffers in
whole multiples of `FRAME_SAMPLES`, but those exist purely for **cross-thread
handoff** (mic thread → pipeline thread → render thread), never as
discretionary multi-frame lookahead in the DSP stages themselves. Every
`Stage::process()` call (RNNoise, BVC, Studio, AEC) operates on exactly one
480-sample frame. The only exception is BVC's `FrameAdapter` (§5), which is
bounded (`4 * host_len.max(target_len) + 8` capacity) and exists solely to
reconcile a real frame-length mismatch, not to add buffering headroom.

**MATCH** — both are single-frame-granularity in the actual DSP path.

## 9. Summary

No accidental, undocumented numeric drift was found. Every real difference
uncovered — RNNoise's C-library vs pure-Rust implementation, the presence of
an AEC filter Linux doesn't have, and BVC's adapt-vs-hard-fail strategy on a
frame-length mismatch — is explicitly explained in this repo's own source
comments with a concrete reason (a build-tooling constraint, a Windows-only
addition, or defensive engineering pending real-hardware confirmation), not
silently different. The one open item worth confirming on real hardware is
noted in §5: what `weya_nc_get_frame_length()` actually returns at 48kHz.

## Architecture: where the OS boundary actually is

```
Windows Physical Microphone
        |
   WASAPI capture (crates/audio-io/src/capture.rs)
        |  <- OS-specific: device enumeration, shared-mode negotiation,
        |     byte-queue -> f32 conversion, MMCSS elevation
        v
  Bounded ring buffer (dsp_core::make_ring_buffer, cross-thread handoff only)
        |
   Common frame format: mono f32, 480 samples, 48kHz  <-- the parity boundary
        |
  dsp_core::Stage chain: AEC -> RNNoise -> BVC -> Studio
        |     (same algorithms/parameters as Linux - see table above;
        |      never touches WASAPI/PipeWire types directly)
        v
  Bounded ring buffer
        |
   WASAPI render (crates/audio-io/src/render.rs) -> VB-Cable/VoiceMeeter
        |
Zoom / Google Meet / Teams (select the virtual cable as their microphone)
```

The `dsp_core::Stage` trait (`process(&mut self, frame: &mut [f32])`,
`crates/dsp-core/src/lib.rs`) is already the platform-independent interface:
none of `noise-engine`, `bvc-hush`, `studio-dsp`, or `aec` import anything
from `audio-io` or the `wasapi`/`windows` crates. `audio-io` is the only
crate that knows WASAPI exists. This already matches the "OS audio layer vs
common processing engine" separation this document set out to verify — it
did not need to be introduced, only confirmed.
