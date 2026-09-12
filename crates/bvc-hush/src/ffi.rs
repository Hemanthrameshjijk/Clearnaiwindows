//! Raw C ABI bindings for `weya_nc.dll`, matching the real, upstream
//! `deployment/include/weya_nc.h` from github.com/pulp-vision/Hush (fetched
//! and read directly — see crate-level docs in `lib.rs` for the exact URL
//! and the full header text). These types/signatures are transcribed
//! directly from that header; nothing here is guessed.
//!
//! ```c
//! typedef struct WeyaModel WeyaModel;
//! typedef struct WeyaSession WeyaSession;
//!
//! WeyaModel* weya_nc_model_load(void);
//! WeyaModel* weya_nc_model_load_from_path(const char* path);
//! void weya_nc_model_free(WeyaModel* model);
//! WeyaSession* weya_nc_session_create(const WeyaModel* model, size_t input_sr, float atten_lim_db);
//! void weya_nc_session_free(WeyaSession* session);
//! size_t weya_nc_get_frame_length(const WeyaSession* session);
//! size_t weya_nc_get_sample_rate(const WeyaSession* session);
//! size_t weya_nc_get_input_sample_rate(const WeyaSession* session);
//! float weya_nc_process_frame(WeyaSession* session, const float* input, float* output);
//! void weya_nc_reset(WeyaSession* session);
//! ```
//!
//! The header declares no calling convention, so these use the platform C
//! default (`cdecl` on Windows x86_64, which is also the only convention
//! that matters on x86_64 — `stdcall`/`cdecl` are identical there).
//!
//! `size_t` is bound as `usize`: correct on the x86_64 targets this crate
//! and the shipped DLL both target.

use std::os::raw::{c_char, c_float};

/// Opaque handle, never constructed on the Rust side. Matches the
/// forward-declared `typedef struct WeyaModel WeyaModel;`. A zero-variant
/// enum (rather than `#[repr(C)] struct WeyaModel { _p: () }`) is the
/// standard idiom for an opaque FFI type: it cannot be instantiated,
/// matched, or read through, only pointed to.
pub enum WeyaModel {}

/// Opaque handle, never constructed on the Rust side. Matches the
/// forward-declared `typedef struct WeyaSession WeyaSession;`.
pub enum WeyaSession {}

pub type FnModelLoad = unsafe extern "C" fn() -> *mut WeyaModel;
pub type FnModelLoadFromPath = unsafe extern "C" fn(path: *const c_char) -> *mut WeyaModel;
pub type FnModelFree = unsafe extern "C" fn(model: *mut WeyaModel);
pub type FnSessionCreate =
    unsafe extern "C" fn(model: *const WeyaModel, input_sr: usize, atten_lim_db: c_float) -> *mut WeyaSession;
pub type FnSessionFree = unsafe extern "C" fn(session: *mut WeyaSession);
pub type FnGetFrameLength = unsafe extern "C" fn(session: *const WeyaSession) -> usize;
pub type FnGetSampleRate = unsafe extern "C" fn(session: *const WeyaSession) -> usize;
pub type FnGetInputSampleRate = unsafe extern "C" fn(session: *const WeyaSession) -> usize;
pub type FnProcessFrame =
    unsafe extern "C" fn(session: *mut WeyaSession, input: *const c_float, output: *mut c_float) -> c_float;
pub type FnReset = unsafe extern "C" fn(session: *mut WeyaSession);

/// The exact 10 symbol names the header declares, all of which must
/// resolve for `HushBvcStage::try_load` to succeed.
pub const SYM_MODEL_LOAD: &str = "weya_nc_model_load";
pub const SYM_MODEL_LOAD_FROM_PATH: &str = "weya_nc_model_load_from_path";
pub const SYM_MODEL_FREE: &str = "weya_nc_model_free";
pub const SYM_SESSION_CREATE: &str = "weya_nc_session_create";
pub const SYM_SESSION_FREE: &str = "weya_nc_session_free";
pub const SYM_GET_FRAME_LENGTH: &str = "weya_nc_get_frame_length";
pub const SYM_GET_SAMPLE_RATE: &str = "weya_nc_get_sample_rate";
pub const SYM_GET_INPUT_SAMPLE_RATE: &str = "weya_nc_get_input_sample_rate";
pub const SYM_PROCESS_FRAME: &str = "weya_nc_process_frame";
pub const SYM_RESET: &str = "weya_nc_reset";
