//! First-run "setup" checks shown before the main pipeline GUI: basic
//! system compatibility (OS version, architecture, COM) and BVC asset
//! (DLL + model) extraction from what is now embedded directly in this
//! binary - see `ensure_bvc_assets_extracted()`.
//!
//! Only the OS-version/COM checks are genuinely Windows-only
//! (`#[cfg(windows)]`); everything else here (the pure version-check math,
//! and the extraction logic) is ordinary portable Rust and builds/tests on
//! any platform, matching the rest of this crate's "portable unless it
//! truly must touch an OS API" pattern (see `gui.rs`/`shared.rs`/
//! `settings.rs`).
//!
//! # Why embedding instead of a next-to-the-exe download
//!
//! Earlier revisions of this module shipped `weya_nc.dll` and the ONNX
//! model bundle as separate files the user (or a packaging script) had to
//! place next to `clearnairt.exe`, with a first-run background HTTP
//! download as a fallback for the model. Real hands-on testing on Windows
//! found this confusing (a `.dll` isn't runnable and double-clicking it
//! does nothing useful) and not "production ready" for a single-binary
//! distribution goal. Both files are now `include_bytes!`-embedded directly
//! into `clearnairt.exe` at build time (see `app/assets/`) and
//! self-extracted into `%LOCALAPPDATA%\ClearNAI` on every launch - fast
//! (already in memory, just a size check + maybe a file write) enough to do
//! synchronously with no progress UI needed, and it means the packaged
//! folder now only needs the one `.exe` for BVC to work at all.
//!
//! # What has and hasn't been verified
//!
//! `check_system_compatibility()`'s real Windows branch (the `RtlGetVersion`
//! call and the COM init check) has never run on real Windows - only
//! type-checked via `cargo check --target x86_64-pc-windows-gnu`. Likewise
//! `ensure_bvc_assets_extracted()`'s real write into `%LOCALAPPDATA%` has
//! never been run on real Windows - only type/link-checked, plus unit
//! tests against an arbitrary temp directory (the extraction logic itself
//! is parameterized on the target directory for exactly this reason).

use std::io::Write;
use std::path::{Path, PathBuf};

/// The real `weya_nc.dll` (see `crates/bvc-hush`), embedded directly into
/// this binary at build time from `app/assets/weya_nc.dll` - a real,
/// unmodified copy built from github.com/pulp-vision/Hush's
/// `native/weya_nc_build/weya_nc` source, confirmed via `objdump` to export
/// exactly the symbols `bvc-hush/src/ffi.rs` binds to.
const EMBEDDED_DLL_BYTES: &[u8] = include_bytes!("../assets/weya_nc.dll");

/// The real ONNX model bundle, embedded directly into this binary at build
/// time from `app/assets/advanced_dfnet16k_model_best_onnx.tar.gz` - a real,
/// unmodified download from
/// <https://huggingface.co/weya-ai/hush/resolve/main/onnx/advanced_dfnet16k_model_best_onnx.tar.gz>
/// (Apache-2.0 licensed), confirmed to contain `enc.onnx`/`erb_dec.onnx`/
/// `df_dec.onnx`/`config.ini`/`version.txt`.
const EMBEDDED_MODEL_BYTES: &[u8] = include_bytes!("../assets/advanced_dfnet16k_model_best_onnx.tar.gz");

/// Result of the one-time, fast system compatibility check run at every
/// launch (cheap enough to always redo, unlike the model download).
#[derive(Debug, Clone, PartialEq)]
pub struct CompatibilityReport {
    pub os_ok: bool,
    pub arch_ok: bool,
    pub com_ok: bool,
    pub details: Vec<String>,
}

impl CompatibilityReport {
    /// True if every individual check passed - callers use this to decide
    /// whether the setup screen needs to be shown at all for compatibility
    /// reasons (a missing model file is a separate, independent trigger).
    pub fn all_ok(&self) -> bool {
        self.os_ok && self.arch_ok && self.com_ok
    }
}

/// Pure logic factored out of the real `RtlGetVersion` call so it can be
/// unit-tested on any platform: Windows 10 and 11 both report major version
/// 10 (there is no separate "11" major version number at the Win32 API
/// level - the two are told apart by build number, which this app does not
/// need to distinguish for a basic compatibility gate).
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_major_version_is_supported(major: u32) -> bool {
    major >= 10
}

#[cfg(windows)]
fn check_os_version(details: &mut Vec<String>) -> bool {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOEXW;

    // SAFETY: `RtlGetVersion` only reads/writes the `OSVERSIONINFOEXW` we
    // pass it; `dwOSVersionInfoSize` must be set to the struct size first,
    // per the documented contract, or the call fails with STATUS_INVALID_PARAMETER.
    let mut info = OSVERSIONINFOEXW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOEXW>() as u32,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut info as *mut _ as *mut _) };
    if status.is_err() {
        details.push(format!("Failed to query Windows version (RtlGetVersion status {status:?})"));
        return false;
    }
    let major = info.dwMajorVersion;
    let build = info.dwBuildNumber;
    let ok = windows_major_version_is_supported(major);
    if ok {
        details.push(format!("Windows 10/11 detected (build {build})"));
    } else {
        details.push(format!(
            "Unsupported Windows version detected (found major version {major}, need 10+)"
        ));
    }
    ok
}

#[cfg(windows)]
fn check_com(details: &mut Vec<String>) -> bool {
    // Must match `audio_io::initialize_com_for_this_thread`'s apartment
    // choice exactly (STA, not MTA) — this runs on the same main/GUI
    // thread that `winit` (via `iced`) later calls `OleInitialize` on for
    // window drag-and-drop support, and `OleInitialize` hard-requires STA.
    // Real hardware testing hit this directly: initializing MTA here (and
    // in `audio-io`) worked for WASAPI enumeration alone, but crashed the
    // whole app with `OleInitialize failed! RPC_E_CHANGED_MODE` as soon as
    // the GUI window tried to come up, because the thread had already
    // committed to a different (incompatible) apartment model. Calling the
    // same STA init twice on one thread (once here, once in
    // `audio_io::initialize_com_for_this_thread`) is safe — COM reference-
    // counts nested *compatible* `CoInitializeEx`/`OleInitialize` calls.
    let hr = wasapi::initialize_sta();
    if hr.is_ok() {
        details.push("COM (STA) initialized successfully".to_string());
        true
    } else {
        details.push(format!("COM (STA) initialization failed: {hr:?}"));
        false
    }
}

/// Runs the real system compatibility checks. On Windows this queries the
/// real OS version via `RtlGetVersion` (not the manifest-shimmed
/// `GetVersionEx`) and attempts a real COM MTA initialization. On any other
/// platform, returns an honest "not running on Windows" report rather than
/// pretending to have checked anything.
pub fn check_system_compatibility() -> CompatibilityReport {
    #[cfg(windows)]
    {
        let mut details = Vec::new();
        let os_ok = check_os_version(&mut details);
        let arch_ok = cfg!(target_arch = "x86_64");
        details.push(if arch_ok {
            "x86_64 architecture detected (supported)".to_string()
        } else {
            "Unsupported CPU architecture (this build only targets x86_64)".to_string()
        });
        let com_ok = check_com(&mut details);
        CompatibilityReport { os_ok, arch_ok, com_ok, details }
    }
    #[cfg(not(windows))]
    {
        CompatibilityReport {
            os_ok: false,
            arch_ok: true,
            com_ok: false,
            details: vec![
                "Not running on Windows: OS-version and COM checks are unavailable on this \
                 platform."
                    .to_string(),
            ],
        }
    }
}

/// The directory BVC assets (`weya_nc.dll` and the ONNX model bundle) are
/// extracted into on every launch: `%LOCALAPPDATA%\ClearNAI` on Windows -
/// a per-user, writable-without-admin-rights location that is the
/// conventional home for this kind of app-managed cache/support data (as
/// opposed to `%PROGRAMFILES%`, which a non-admin install typically can't
/// write to, or right next to the exe, which is what this whole change
/// moves away from).
///
/// If `LOCALAPPDATA` isn't set (unusual, but not impossible - e.g. a
/// stripped-down service account), falls back to `%TEMP%\ClearNAI` so the
/// app still has *somewhere* writable rather than failing outright; if
/// even that can't be resolved, falls back to `.\ClearNAI` (relative to
/// the current directory).
///
/// On non-Windows platforms (only ever exercised by `cargo check`/
/// `cargo test` from this Linux dev machine, never for real) this resolves
/// to a temp directory instead, purely so the portable parts of this
/// module have something sane to test against.
///
/// Creates the directory (`create_dir_all`) if it doesn't already exist;
/// callers don't need to do this themselves.
pub fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("TEMP"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(windows))]
    let base = std::env::temp_dir();

    let dir = base.join("ClearNAI");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::log_error!("[clearnairt] WARNING: failed to create app data dir {}: {e:#}", dir.display());
    }
    dir
}

/// Where the BVC model bundle lives once extracted: under
/// `app_data_dir()`, using the real filename `bvc-hush` already exports
/// (never re-hardcoded here, so the two crates can't silently drift apart).
pub fn model_bundle_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(bvc_hush::MODEL_BUNDLE_FILE_NAME)
}

/// Where the DLL lives once extracted: under `app_data_dir()`.
pub fn dll_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(bvc_hush::DLL_FILE_NAME)
}

/// True only if the real bundle file exists, is non-empty, and its size
/// matches the embedded bytes - i.e. `ensure_bvc_assets_extracted` has
/// already run successfully against this directory. A zero-byte, partial,
/// or stale-sized file counts as NOT present so extraction is retried.
///
/// Not called from `main.rs` any more (extraction now runs unconditionally
/// and reports its own `Result` - see `ensure_bvc_assets_extracted`), but
/// kept as a small, independently useful/testable query, matching this
/// module's existing habit of keeping honest presence-checks around.
#[allow(dead_code)]
pub fn model_bundle_present(app_data_dir: &Path) -> bool {
    match std::fs::metadata(model_bundle_path(app_data_dir)) {
        Ok(meta) => meta.is_file() && meta.len() == EMBEDDED_MODEL_BYTES.len() as u64,
        Err(_) => false,
    }
}

/// Whether a single embedded asset needed to be (re)written, returned per
/// file by `ensure_bvc_assets_extracted` so callers/tests can distinguish
/// "already there, nothing to do" from "just wrote it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetState {
    /// The file already existed at the target path with a size matching
    /// the embedded bytes - left untouched.
    AlreadyPresent,
    /// The file was missing, or present with a mismatched size (e.g. a
    /// partial/corrupt leftover, or an older/newer embedded version), so it
    /// was (re)written from the embedded bytes.
    Extracted,
}

/// Result of `ensure_bvc_assets_extracted`: what happened to each of the
/// two embedded files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionOutcome {
    pub dll: AssetState,
    pub model: AssetState,
}

impl ExtractionOutcome {
    /// True if at least one file was freshly written this run - useful for
    /// deciding whether to mention anything to the user at all versus
    /// silently proceeding straight past the setup screen.
    #[allow(dead_code)]
    pub fn wrote_anything(&self) -> bool {
        self.dll == AssetState::Extracted || self.model == AssetState::Extracted
    }
}

/// Writes `bytes` to `final_path` atomically: first to a `.part` sibling
/// file, then renamed into place only on complete success - the same
/// crash-safety property the old network download used, so a process
/// killed mid-write never leaves a corrupt file at `final_path` that a
/// later run would mistake for good (the size check in
/// `extract_one_if_needed` would in fact still catch a truncated `.part`
/// left over from an old, pre-embedding version of this app, since it only
/// ever inspects `final_path`, never `.part`).
fn write_atomically(final_path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let part_path = final_path.with_extension(match final_path.extension() {
        Some(ext) => format!("{}.part", ext.to_string_lossy()),
        None => "part".to_string(),
    });
    let result = (|| -> anyhow::Result<()> {
        if let Some(parent) = part_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("failed to create directory {}: {e}", parent.display()))?;
        }
        let mut file = std::fs::File::create(&part_path)
            .map_err(|e| anyhow::anyhow!("failed to create temp file {}: {e}", part_path.display()))?;
        file.write_all(bytes)
            .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", part_path.display()))?;
        file.sync_all().ok();
        drop(file);
        std::fs::rename(&part_path, final_path)
            .map_err(|e| anyhow::anyhow!("failed to move {} into place: {e}", final_path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part_path);
    }
    result
}

/// Writes `bytes` to `path` only if it doesn't already exist there with a
/// matching size - a cheap, good-enough staleness check that avoids
/// rewriting ~14MB/~8.5MB of already-correct data on every single launch,
/// without needing a full content hash (the bytes are, after all, embedded
/// in this very binary and never change between launches of the same
/// build).
fn extract_one_if_needed(path: &Path, bytes: &[u8]) -> anyhow::Result<AssetState> {
    let already_correct = std::fs::metadata(path).map(|m| m.is_file() && m.len() == bytes.len() as u64).unwrap_or(false);
    if already_correct {
        return Ok(AssetState::AlreadyPresent);
    }
    write_atomically(path, bytes)?;
    Ok(AssetState::Extracted)
}

/// Self-extracts the embedded `weya_nc.dll` and ONNX model bundle into
/// `target_dir` (in practice, always `app_data_dir()` - factored out as a
/// parameter here purely so this logic is unit-testable against a plain
/// temp directory without touching `%LOCALAPPDATA%`).
///
/// Fast enough (bytes are already resident in the binary's own image; this
/// is at most two size-checks plus two small file writes) to run
/// synchronously on the GUI thread at every launch, unlike the old
/// network-download path this replaces - no background thread or progress
/// bar is needed.
///
/// Returns a real `Result`: disk-full, permission-denied (e.g. an
/// unwritable `%LOCALAPPDATA%`), or any other I/O failure is surfaced as an
/// `Err` rather than panicking, so the caller can show it on the setup
/// screen and offer "Continue anyway" - BVC simply stays unavailable in
/// that case, exactly like the DLL-not-found case already handled by
/// `bvc_hush::HushBvcStage::try_load`.
pub fn ensure_bvc_assets_extracted(target_dir: &Path) -> anyhow::Result<ExtractionOutcome> {
    let dll = extract_one_if_needed(&dll_path(target_dir), EMBEDDED_DLL_BYTES)
        .map_err(|e| anyhow::anyhow!("failed to extract {}: {e:#}", bvc_hush::DLL_FILE_NAME))?;
    let model = extract_one_if_needed(&model_bundle_path(target_dir), EMBEDDED_MODEL_BYTES)
        .map_err(|e| anyhow::anyhow!("failed to extract {}: {e:#}", bvc_hush::MODEL_BUNDLE_FILE_NAME))?;
    Ok(ExtractionOutcome { dll, model })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_10_and_11_major_version_is_supported() {
        assert!(windows_major_version_is_supported(10));
        assert!(windows_major_version_is_supported(11));
        assert!(windows_major_version_is_supported(12), "future major versions >= 10 are not rejected");
    }

    #[test]
    fn pre_windows_10_major_version_is_unsupported() {
        assert!(!windows_major_version_is_supported(6)); // Vista/7/8/8.1 era
        assert!(!windows_major_version_is_supported(0));
    }

    #[test]
    fn model_bundle_path_uses_the_real_shared_constant() {
        let dir = PathBuf::from("/some/exe/dir");
        let path = model_bundle_path(&dir);
        assert_eq!(path, dir.join(bvc_hush::MODEL_BUNDLE_FILE_NAME));
    }

    #[test]
    fn model_bundle_absent_when_no_file_exists() {
        let dir = std::env::temp_dir().join(format!("clearnai_setup_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!model_bundle_present(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_bundle_absent_when_file_is_zero_bytes() {
        let dir = std::env::temp_dir().join(format!("clearnai_setup_test_zero_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(model_bundle_path(&dir), []).unwrap();
        assert!(!model_bundle_present(&dir), "a zero-byte file must not count as present");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_bundle_absent_when_size_does_not_match_embedded_bytes() {
        let dir = std::env::temp_dir().join(format!("clearnai_setup_test_wrongsize_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(model_bundle_path(&dir), b"not a real tarball, and the wrong size too").unwrap();
        assert!(!model_bundle_present(&dir), "a size mismatch vs the embedded bytes must not count as present");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_bundle_present_after_real_extraction() {
        let dir = std::env::temp_dir().join(format!("clearnai_setup_test_present_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ensure_bvc_assets_extracted(&dir).unwrap();
        assert!(model_bundle_present(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_bvc_assets_extracted_writes_files_matching_embedded_byte_lengths() {
        let dir = std::env::temp_dir().join(format!("clearnai_setup_test_extract_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let outcome = ensure_bvc_assets_extracted(&dir).expect("extraction into a fresh temp dir must succeed");
        assert_eq!(outcome.dll, AssetState::Extracted);
        assert_eq!(outcome.model, AssetState::Extracted);
        assert!(outcome.wrote_anything());

        let dll_meta = std::fs::metadata(dll_path(&dir)).unwrap();
        assert_eq!(dll_meta.len(), EMBEDDED_DLL_BYTES.len() as u64);
        let model_meta = std::fs::metadata(model_bundle_path(&dir)).unwrap();
        assert_eq!(model_meta.len(), EMBEDDED_MODEL_BYTES.len() as u64);

        // No leftover `.part` files after a successful extraction.
        assert!(!dll_path(&dir).with_extension("dll.part").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_bvc_assets_extracted_is_idempotent_and_does_not_rewrite_correct_files() {
        let dir = std::env::temp_dir().join(format!("clearnai_setup_test_idempotent_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let first = ensure_bvc_assets_extracted(&dir).expect("first extraction must succeed");
        assert_eq!(first.dll, AssetState::Extracted);
        assert_eq!(first.model, AssetState::Extracted);

        let second = ensure_bvc_assets_extracted(&dir).expect("second extraction must succeed");
        assert_eq!(second.dll, AssetState::AlreadyPresent, "a correctly-sized file must not be rewritten");
        assert_eq!(second.model, AssetState::AlreadyPresent, "a correctly-sized file must not be rewritten");
        assert!(!second.wrote_anything());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compatibility_report_all_ok_requires_every_field() {
        let mut r = CompatibilityReport { os_ok: true, arch_ok: true, com_ok: true, details: Vec::new() };
        assert!(r.all_ok());
        r.com_ok = false;
        assert!(!r.all_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_report_is_honestly_not_ok() {
        let report = check_system_compatibility();
        assert!(!report.os_ok);
        assert!(!report.com_ok);
        assert!(report.arch_ok);
        assert!(!report.details.is_empty());
    }

    #[test]
    fn app_data_dir_is_creatable_and_a_directory() {
        let dir = app_data_dir();
        assert!(dir.is_dir(), "app_data_dir() must create the directory it returns: {}", dir.display());
    }
}
