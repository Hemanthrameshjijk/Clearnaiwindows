//! Windows port glue for "Hush" / BVC (background-voice-cancellation),
//! wrapping the prebuilt `weya_nc.dll` from
//! <https://github.com/pulp-vision/Hush> (built on DeepFilterNet-SE
//! internals) as a `dsp_core::Stage`.
//!
//! # Provenance of the C API this binds to
//!
//! The real header was fetched and read directly from the upstream repo
//! before any FFI code was written:
//!
//! - Repo browsed: <https://github.com/pulp-vision/Hush> (default branch
//!   `main`), confirming `deployment/include/weya_nc.h` exists.
//! - Header fetched from:
//!   <https://raw.githubusercontent.com/pulp-vision/Hush/main/deployment/include/weya_nc.h>
//! - Cross-checked against the integration guide at
//!   <https://raw.githubusercontent.com/pulp-vision/Hush/main/deployment/README.md>
//!   and the worked C example at
//!   <https://raw.githubusercontent.com/pulp-vision/Hush/main/deployment/examples/c/denoise_frame_demo.c>.
//!
//! All three fetches returned HTTP 200 with real content (verified with
//! plain `curl`, not just an LLM summarization tool, specifically to rule
//! out a hallucinated 404/placeholder page). The full header text is
//! reproduced in `ffi.rs`.
//!
//! Key facts from the real header/docs that shaped this wrapper (do not
//! assume these without re-reading the header if it ever changes):
//!
//! - There are exactly 10 exported functions (matching the "~10 functions"
//!   description) — all bound in `ffi.rs`.
//! - `weya_nc_process_frame` takes **separate** input/output buffers (not
//!   in-place), both `float` in range `[-1.0, 1.0]` — this project's
//!   `Stage::process` contract is in-place, so `HushBvcStage::process`
//!   bridges that.
//! - The frame length the DLL wants per call is **not a fixed constant**:
//!   it depends on the `input_sr` passed to `weya_nc_session_create` and
//!   can only be read back at runtime via `weya_nc_get_frame_length`. This
//!   crate passes `dsp_core::SAMPLE_RATE_HZ` (48kHz) as `input_sr`, then
//!   queries the real frame length rather than assuming it equals
//!   `dsp_core::FRAME_SAMPLES` (480). Since the two are genuinely unknown
//!   to be equal without running the real DLL (not possible on this Linux
//!   dev machine), `FrameAdapter` (in `frame_adapter.rs`) handles arbitrary
//!   mismatches in either direction.
//! - No calling convention is declared in the header, so plain C default
//!   (`cdecl`) applies.
//!
//! # What is unverified
//!
//! Nobody has run this against the real DLL on Windows. In particular:
//! - The actual numeric value of `weya_nc_get_frame_length()` at 48kHz
//!   input is unknown (this crate handles whatever it turns out to be, but
//!   the exact resulting latency introduced by `FrameAdapter` has not been
//!   measured).
//! - Model file discovery (`advanced_dfnet16k_model_best_onnx.tar.gz` next
//!   to the DLL, falling back to `weya_nc_model_load()`'s built-in/env-var
//!   default) is this crate's own convention, not something the header
//!   specifies — `try_load` only takes a `dll_dir`, so there was no room in
//!   the requested signature for an explicit model path. A future engineer
//!   with access to a real deployment should confirm this convention
//!   matches how the model bundle is actually shipped alongside the app,
//!   and adjust `HushBvcStage::locate_model_bundle` (or add an explicit
//!   model path parameter) if not.
//! - `weya_nc_session_create`'s `atten_lim_db` is hardcoded to
//!   `DEFAULT_ATTEN_LIM_DB` (30.0 dB). This value is confirmed, not guessed:
//!   it matches the reference Linux port's `BvcConfig::default()` in
//!   `clearnai-bvc/src/registry.rs` (`hush_atten_lim_db: 30.0`) and its CLI
//!   default in `clearnai-cli/src/main.rs` (`--bvc-atten-lim-db`,
//!   `default_value_t = 30.0`). An earlier version of this file used 100.0
//!   ("unlimited" per upstream's own docs) as an unverified guess; 30.0 is
//!   the value the reference project actually ships and tunes against.
//!   Wiring this to a user-facing setting is left for later.
//!
//! # Documented divergence from the reference project
//!
//! The reference Linux port (`clearnai-bvc/src/hush.rs`) fails fast: it
//! calls `weya_nc_get_frame_length()` once at initialization and returns a
//! hard error if it doesn't equal the expected 480, refusing to run at a
//! mismatched frame size. This crate deliberately does *not* replicate that
//! strict assert. Instead `FrameAdapter` (see `frame_adapter.rs`) buffers
//! and re-chunks between the host's fixed 480-sample frames and whatever
//! frame length the DLL actually reports, tolerating a mismatch instead of
//! refusing to start. This is a deliberate design choice, not an oversight:
//! nobody has been able to run the real `weya_nc.dll` on this Windows port
//! yet, so there is no confirmed value for `weya_nc_get_frame_length()` at
//! 48kHz to assert against. Failing fast on an unverified assumption would
//! risk bricking BVC entirely on a mismatch that might in fact be harmless;
//! the lenient adapter degrades gracefully (extra buffering latency) instead.
//! Once the real DLL's frame length at 48kHz is confirmed on real hardware,
//! reconsider whether to tighten this back to the reference's fail-fast
//! behavior.

mod ffi;
mod frame_adapter;

pub use frame_adapter::FrameAdapter;

use dsp_core::{Stage, FRAME_SAMPLES, SAMPLE_RATE_HZ};
use libloading::Library;
use std::ffi::CString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Filename of the prebuilt library, as shipped by upstream in
/// `deployment/lib/weya_nc.dll`. Looked up relative to `dll_dir` (in
/// practice, the running executable's own directory).
pub const DLL_FILE_NAME: &str = "weya_nc.dll";

/// Filename of the speaker-path's own copy of the exact same DLL bytes,
/// extracted separately (see `try_load_named`'s docs and
/// `setup::ensure_bvc_assets_extracted`) so the OS loader maps it as a
/// distinct module instance from the mic path's `weya_nc.dll`, rather than
/// the same already-loaded image via a bumped refcount.
pub const SPEAKER_DLL_FILE_NAME: &str = "weya_nc_speaker.dll";

/// Filename of the ONNX model bundle, as shipped by upstream in
/// `deployment/models/advanced_dfnet16k_model_best_onnx.tar.gz`. If found
/// next to the DLL, it is loaded explicitly via
/// `weya_nc_model_load_from_path`; otherwise `weya_nc_model_load()` is used
/// (upstream's env-var-or-built-in-default path).
pub const MODEL_BUNDLE_FILE_NAME: &str = "advanced_dfnet16k_model_best_onnx.tar.gz";

/// `atten_lim_db` passed to `weya_nc_session_create`. 30.0 dB, matching the
/// reference Linux port's confirmed default (`BvcConfig::default()` in
/// `clearnai-bvc/src/registry.rs`, and the `--bvc-atten-lim-db` CLI flag
/// default in `clearnai-cli/src/main.rs`). Not to be confused with
/// upstream Hush's own "100.0 = unlimited" Python-quickstart default, which
/// this project's reference deliberately does not use.
pub const DEFAULT_ATTEN_LIM_DB: f32 = 30.0;

/// Everything that can go wrong loading and initializing the DLL, surfaced
/// as a typed error rather than a panic so the rest of the app can report
/// "BVC unavailable on this machine" honestly instead of silently no-op'ing.
#[derive(Debug)]
pub enum HushLoadError {
    /// `weya_nc.dll` does not exist at the expected path.
    DllNotFound(PathBuf),
    /// The DLL exists but the OS loader rejected it (wrong architecture,
    /// missing transitive dependency, corrupt file, etc).
    DllLoadFailed(PathBuf, String),
    /// The DLL loaded, but a required exported symbol is missing. Carries
    /// the symbol name and the underlying `libloading` error text.
    SymbolNotFound(&'static str, String),
    /// A path needed for an FFI call (DLL path or model bundle path)
    /// contains bytes that cannot be represented as a C string / UTF-8.
    InvalidPath(PathBuf),
    /// `weya_nc_model_load` / `weya_nc_model_load_from_path` returned NULL.
    ModelLoadFailed { attempted_path: Option<PathBuf> },
    /// `weya_nc_session_create` returned NULL.
    SessionCreateFailed,
    /// `weya_nc_get_frame_length` returned 0, which cannot be a valid frame
    /// size; something is wrong with the session even though creation
    /// nominally succeeded.
    InvalidFrameLength,
}

impl fmt::Display for HushLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HushLoadError::DllNotFound(path) => {
                write!(f, "{} not found (looked at {})", DLL_FILE_NAME, path.display())
            }
            HushLoadError::DllLoadFailed(path, err) => {
                write!(f, "failed to load {}: {err}", path.display())
            }
            HushLoadError::SymbolNotFound(name, err) => {
                write!(f, "required symbol `{name}` not found in {DLL_FILE_NAME}: {err}")
            }
            HushLoadError::InvalidPath(path) => {
                write!(f, "path is not valid UTF-8 / contains a NUL byte: {}", path.display())
            }
            HushLoadError::ModelLoadFailed { attempted_path: Some(path) } => {
                write!(f, "weya_nc_model_load_from_path returned NULL for {}", path.display())
            }
            HushLoadError::ModelLoadFailed { attempted_path: None } => {
                write!(f, "weya_nc_model_load returned NULL (no model bundle found next to the DLL, and no default/env-configured model was found either)")
            }
            HushLoadError::SessionCreateFailed => {
                write!(f, "weya_nc_session_create returned NULL")
            }
            HushLoadError::InvalidFrameLength => {
                write!(f, "weya_nc_get_frame_length returned 0")
            }
        }
    }
}

impl std::error::Error for HushLoadError {}

/// `dsp_core::Stage` implementation backed by the real `weya_nc.dll` C API,
/// loaded dynamically at runtime (never linked at build time — see
/// crate docs).
pub struct HushBvcStage {
    session: *mut ffi::WeyaSession,
    model: *mut ffi::WeyaModel,
    fn_process_frame: ffi::FnProcessFrame,
    fn_session_free: ffi::FnSessionFree,
    fn_model_free: ffi::FnModelFree,
    fn_reset: ffi::FnReset,
    adapter: FrameAdapter,
    frame_len: usize,
    model_sample_rate: usize,
    last_snr_db: f32,
    // Kept alive for as long as the function pointers above are used. Must
    // outlive every call through them; dropped last (Rust drops struct
    // fields in declaration order, and our own `Drop::drop` below runs
    // before any field is dropped, so cleanup calls always happen while
    // `_lib` is still loaded regardless of field order — this ordering is
    // just for readability).
    _lib: Library,
}

// SAFETY: `HushBvcStage` owns its `WeyaModel`/`WeyaSession` handles
// exclusively; the real API's documented threading contract is "one
// session per thread, not reentrant" (safe to move to another thread
// entirely, unsafe to touch from two threads at once) which is exactly
// `Send` (movable, not `Sync`) semantics. We deliberately do not implement
// `Sync`.
unsafe impl Send for HushBvcStage {}

impl HushBvcStage {
    /// Attempts to load `weya_nc.dll` from `dll_dir`, resolve all required
    /// symbols, load a model, and create a processing session sized for
    /// `dsp_core::SAMPLE_RATE_HZ`. Returns a descriptive error instead of
    /// panicking if anything is missing, so callers can detect "BVC
    /// unavailable on this machine" and reflect that in the UI.
    ///
    /// Equivalent to `try_load_named(dll_dir, DLL_FILE_NAME)` - see that
    /// function's docs for why a caller running two independent instances
    /// (this app's mic and speaker paths) may want a different filename.
    pub fn try_load(dll_dir: &Path) -> Result<Self, HushLoadError> {
        Self::try_load_named(dll_dir, DLL_FILE_NAME)
    }

    /// Same as `try_load`, but loads the DLL from `dll_dir.join(dll_file_name)`
    /// instead of the fixed `DLL_FILE_NAME`.
    ///
    /// Real hardware finding this exists for: this app creates two fully
    /// independent `HushBvcStage` instances (mic path, speaker path) at
    /// startup. On Windows, `LoadLibrary`-ing the *same file path* twice
    /// does not give you two independent copies of the module - it bumps a
    /// refcount and hands back the same already-mapped image, so any
    /// process-global (not per-session) state inside this closed-source DLL
    /// - a shared thread pool, a global model/license cache, anything not
    /// scoped to the `WeyaSession`/`WeyaModel` handles the documented API
    /// exposes - would silently be shared between the two "independent"
    /// paths despite each one holding its own session. Loading from two
    /// distinct on-disk file paths (see `setup::ensure_bvc_assets_extracted`,
    /// which extracts a second copy under a different filename) forces the
    /// OS loader to map two separate module instances, so any such global
    /// state - if it exists at all, which cannot be confirmed without the
    /// DLL's source - is at least not accidentally shared just because both
    /// paths happen to load "the same" DLL by name.
    pub fn try_load_named(dll_dir: &Path, dll_file_name: &str) -> Result<Self, HushLoadError> {
        let dll_path = dll_dir.join(dll_file_name);
        if !dll_path.is_file() {
            return Err(HushLoadError::DllNotFound(dll_path));
        }

        // SAFETY: loading an on-disk library whose path we just verified
        // exists. Running arbitrary DLL init code is inherent to dynamic
        // loading; this is what the task requires (runtime, not
        // build-time, linkage).
        let lib = unsafe { Library::new(&dll_path) }
            .map_err(|e| HushLoadError::DllLoadFailed(dll_path.clone(), e.to_string()))?;

        // SAFETY: each symbol type below is transcribed directly from the
        // real weya_nc.h (see ffi.rs); as long as the DLL actually
        // implements that header (which we can't verify without the DLL
        // present) these types are correct.
        let fn_model_load: ffi::FnModelLoad = unsafe { get_symbol(&lib, ffi::SYM_MODEL_LOAD)? };
        let fn_model_load_from_path: ffi::FnModelLoadFromPath =
            unsafe { get_symbol(&lib, ffi::SYM_MODEL_LOAD_FROM_PATH)? };
        let fn_model_free: ffi::FnModelFree = unsafe { get_symbol(&lib, ffi::SYM_MODEL_FREE)? };
        let fn_session_create: ffi::FnSessionCreate =
            unsafe { get_symbol(&lib, ffi::SYM_SESSION_CREATE)? };
        let fn_session_free: ffi::FnSessionFree =
            unsafe { get_symbol(&lib, ffi::SYM_SESSION_FREE)? };
        let fn_get_frame_length: ffi::FnGetFrameLength =
            unsafe { get_symbol(&lib, ffi::SYM_GET_FRAME_LENGTH)? };
        let fn_get_sample_rate: ffi::FnGetSampleRate =
            unsafe { get_symbol(&lib, ffi::SYM_GET_SAMPLE_RATE)? };
        // Resolved (and required to be present) even though we don't call
        // it today, to fail loudly in `try_load` if this symbol is ever
        // missing rather than only at first use.
        let _fn_get_input_sample_rate: ffi::FnGetInputSampleRate =
            unsafe { get_symbol(&lib, ffi::SYM_GET_INPUT_SAMPLE_RATE)? };
        let fn_process_frame: ffi::FnProcessFrame =
            unsafe { get_symbol(&lib, ffi::SYM_PROCESS_FRAME)? };
        let fn_reset: ffi::FnReset = unsafe { get_symbol(&lib, ffi::SYM_RESET)? };

        // Model discovery convention: prefer an explicit bundle shipped
        // next to the DLL; otherwise fall back to the library's own
        // default (env var `WEYA_NC_MODEL_PATH` or built-in path). See
        // crate docs for why this is our own convention, not upstream's.
        let bundle_path = dll_dir.join(MODEL_BUNDLE_FILE_NAME);
        let model = if bundle_path.is_file() {
            let cpath = path_to_cstring(&bundle_path)?;
            // SAFETY: valid, non-dangling C string; function pointer type
            // matches the real header.
            let m = unsafe { fn_model_load_from_path(cpath.as_ptr()) };
            if m.is_null() {
                return Err(HushLoadError::ModelLoadFailed {
                    attempted_path: Some(bundle_path),
                });
            }
            m
        } else {
            // SAFETY: no arguments, matches the real header.
            let m = unsafe { fn_model_load() };
            if m.is_null() {
                return Err(HushLoadError::ModelLoadFailed { attempted_path: None });
            }
            m
        };

        // SAFETY: `model` just verified non-null and freshly created by
        // this call; `input_sr`/`atten_lim_db` are plain value args.
        let session =
            unsafe { fn_session_create(model, SAMPLE_RATE_HZ as usize, DEFAULT_ATTEN_LIM_DB) };
        if session.is_null() {
            // SAFETY: `model` is non-null and owned by us; freeing it here
            // (session creation failed, so nothing else references it) is
            // exactly the documented lifecycle.
            unsafe { fn_model_free(model) };
            return Err(HushLoadError::SessionCreateFailed);
        }

        // SAFETY: `session` just verified non-null.
        let frame_len = unsafe { fn_get_frame_length(session) };
        if frame_len == 0 {
            unsafe {
                fn_session_free(session);
                fn_model_free(model);
            }
            return Err(HushLoadError::InvalidFrameLength);
        }
        let model_sample_rate = unsafe { fn_get_sample_rate(session) };

        Ok(Self {
            session,
            model,
            fn_process_frame,
            fn_session_free,
            fn_model_free,
            fn_reset,
            adapter: FrameAdapter::new(FRAME_SAMPLES, frame_len),
            frame_len,
            model_sample_rate,
            last_snr_db: 0.0,
            _lib: lib,
        })
    }

    /// The real per-call frame length the loaded session reported via
    /// `weya_nc_get_frame_length`, at `dsp_core::SAMPLE_RATE_HZ` input.
    /// Not necessarily equal to `dsp_core::FRAME_SAMPLES`; see crate docs.
    pub fn native_frame_length(&self) -> usize {
        self.frame_len
    }

    /// The model's internal processing sample rate, as reported by
    /// `weya_nc_get_sample_rate` (documented upstream as fixed at 16000,
    /// but read back rather than assumed).
    pub fn model_sample_rate_hz(&self) -> usize {
        self.model_sample_rate
    }

    /// Estimated SNR in dB from the most recent underlying
    /// `weya_nc_process_frame` call. Stale (holds the previous value)
    /// during host frames where `FrameAdapter` had no full chunk ready to
    /// process.
    pub fn last_snr_db(&self) -> f32 {
        self.last_snr_db
    }

    /// Resets the session's streaming (GRU/filter) state, e.g. when
    /// starting a new independent audio stream.
    pub fn reset_stream_state(&mut self) {
        // SAFETY: `self.session` is non-null for the lifetime of `self`.
        unsafe { (self.fn_reset)(self.session) };
    }
}

impl fmt::Debug for HushBvcStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HushBvcStage")
            .field("frame_len", &self.frame_len)
            .field("model_sample_rate", &self.model_sample_rate)
            .field("last_snr_db", &self.last_snr_db)
            .finish_non_exhaustive()
    }
}

impl Stage for HushBvcStage {
    fn name(&self) -> &'static str {
        "Hush BVC (Weya NC)"
    }

    fn process(&mut self, frame: &mut [f32]) {
        let session = self.session;
        let process_frame = self.fn_process_frame;
        let mut snr_db = self.last_snr_db;
        self.adapter.process_in_place(frame, &mut |input, output| {
            // SAFETY: `input`/`output` are both exactly `self.frame_len`
            // long (guaranteed by `FrameAdapter`, constructed with that
            // target length), matching what `weya_nc_process_frame`
            // expects; `session` is non-null for the lifetime of `self`.
            snr_db = unsafe { process_frame(session, input.as_ptr(), output.as_mut_ptr()) };
        });
        self.last_snr_db = snr_db;
    }
}

impl Drop for HushBvcStage {
    fn drop(&mut self) {
        // SAFETY: both handles are non-null and owned exclusively by
        // `self` for its entire lifetime; this runs at most once.
        unsafe {
            (self.fn_session_free)(self.session);
            (self.fn_model_free)(self.model);
        }
    }
}

/// Looks up `name` in `lib` and copies out the function pointer. The
/// returned pointer is only valid as long as `lib` stays loaded — callers
/// must keep the `Library` alive for at least as long as they use it.
unsafe fn get_symbol<T: Copy>(lib: &Library, name: &'static str) -> Result<T, HushLoadError> {
    let cname = CString::new(name).expect("hardcoded symbol name has no interior NUL");
    match lib.get::<T>(cname.as_bytes_with_nul()) {
        Ok(sym) => Ok(*sym),
        Err(e) => Err(HushLoadError::SymbolNotFound(name, e.to_string())),
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, HushLoadError> {
    let s = path.to_str().ok_or_else(|| HushLoadError::InvalidPath(path.to_path_buf()))?;
    CString::new(s).map_err(|_| HushLoadError::InvalidPath(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `try_load` must fail cleanly (not panic) when the DLL simply isn't
    /// there — this is the primary "BVC unavailable on this machine" path
    /// the rest of the app depends on being able to detect, and it's the
    /// one thing testable without the real DLL.
    #[test]
    fn try_load_on_missing_dll_returns_dll_not_found_not_panic() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bvc-hush-test-missing-{nonce}"));
        // Deliberately do not create `dir` — try_load must not require the
        // directory itself to exist, only check for the DLL file in it.
        let result = HushBvcStage::try_load(&dir);
        match result {
            Err(HushLoadError::DllNotFound(path)) => {
                assert_eq!(path, dir.join(DLL_FILE_NAME));
            }
            other => panic!("expected DllNotFound, got {other:?}"),
        }
    }

    /// Same, but for a directory that does exist and contains unrelated
    /// files — still must not find a DLL that isn't there, and still must
    /// not panic.
    #[test]
    fn try_load_on_existing_dir_without_dll_returns_dll_not_found() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bvc-hush-test-empty-{nonce}"));
        std::fs::create_dir_all(&dir).expect("create temp dir for test");

        let result = HushBvcStage::try_load(&dir);

        std::fs::remove_dir_all(&dir).ok();

        match result {
            Err(HushLoadError::DllNotFound(path)) => {
                assert_eq!(path, dir.join(DLL_FILE_NAME));
            }
            other => panic!("expected DllNotFound, got {other:?}"),
        }
    }

    #[test]
    fn error_display_messages_are_human_readable() {
        let e = HushLoadError::DllNotFound(PathBuf::from("/opt/app/weya_nc.dll"));
        assert!(e.to_string().contains("weya_nc.dll"));

        let e = HushLoadError::SymbolNotFound("weya_nc_process_frame", "not found".into());
        assert!(e.to_string().contains("weya_nc_process_frame"));
    }
}
