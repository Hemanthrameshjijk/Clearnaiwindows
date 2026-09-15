//! `clearnairt` - the single ClearNAI Windows binary: GUI and real-time
//! audio engine in one process (no separate daemon), per the project's hard
//! architecture requirement.
//!
//! See `docs/VIRTUAL_DEVICES.md` for a real, unresolved-by-this-codebase
//! user setup requirement (two separate virtual audio cable installs), and
//! `engine.rs` / `gui.rs` for the pipeline and GUI documentation
//! respectively.
//!
//! # Portable vs Windows-only
//!
//! `settings.rs`, `status_ui.rs`, `shared.rs`, and `gui.rs` are ordinary
//! portable Rust (plus `iced`, which itself supports Linux) and build/run
//! their unit tests on any platform. `engine.rs` is `#![cfg(windows)]` in
//! its entirety - it is the only module that touches `audio_io::capture`/
//! `render`/`loopback`/`devices`'s WASAPI-backed functions, which
//! themselves only exist on Windows. This file branches on `cfg(windows)`
//! to call into the real engine or, elsewhere, construct an inert
//! `EngineHandles` with no audio threads at all - the GUI layer, and this
//! file's own logic, are still fully exercised by a native (non-Windows)
//! `cargo check`/`cargo test`, just without any real audio behind them.

#![windows_subsystem = "windows"]

mod diagnose;
mod gui;
mod logging;
mod settings;
mod setup;
mod shared;
mod status_ui;

#[cfg(windows)]
mod engine;

#[cfg(windows)]
fn bootstrap(settings: &settings::Settings) -> gui::Initial {
    let devices = engine::enumerate_devices();

    let mic_target = settings
        .virtual_mic_target_id
        .clone()
        .or_else(|| engine::default_virtual_mic_target(&devices));
    let speaker_source = settings
        .virtual_speaker_source_id
        .clone()
        .or_else(|| engine::default_virtual_speaker_source(&devices));

    // Fast, synchronous checks - both cheap enough to redo on every launch
    // rather than caching/skipping them (see `setup::CompatibilityReport`
    // and `Settings::setup_acknowledged` for why a real failure is never
    // permanently suppressed).
    let compat = setup::check_system_compatibility();

    // Self-extract the embedded BVC DLL + model into %LOCALAPPDATA% *before*
    // `engine::start` tries to load them - this used to be a manual/
    // download-based step; it is now synchronous and (bytes already being
    // resident in the binary) fast enough to never need a progress UI. A
    // real failure here (disk full, unwritable %LOCALAPPDATA%, etc.) is
    // surfaced to the setup screen rather than crashing - BVC simply stays
    // unavailable, same graceful-degradation pattern as a missing DLL
    // always used to be.
    let bvc_dir = setup::app_data_dir();
    let bvc_extraction_error = match setup::ensure_bvc_assets_extracted(&bvc_dir) {
        Ok(_) => None,
        Err(e) => {
            crate::log_error!("[clearnairt] WARNING: failed to extract embedded BVC assets: {e:#}");
            Some(format!("{e:#}"))
        }
    };

    match engine::start(
        &bvc_dir,
        settings,
        devices.clone(),
        mic_target.as_deref(),
        speaker_source.as_deref(),
    ) {
        Ok(outcome) => gui::Initial {
            settings: settings.clone(),
            handles: outcome.handles,
            devices: outcome.devices,
            extra_warnings: Vec::new(),
            keepalive: Some(Box::new(outcome.live)),
            compat,
            bvc_extraction_error,
        },
        Err(e) => {
            crate::log_error!("[clearnairt] FATAL: failed to start audio engine: {e:#}");
            // Still bring up the GUI rather than exiting silently: an inert
            // engine handle set makes the failure visible (status stays
            // Bypass, no audio flows) instead of the whole app vanishing
            // with no explanation.
            let handles = shared::EngineHandles::new_from_settings(
                settings,
                false,
                Some("engine failed to start; see stderr log".to_string()),
                false,
                Some("engine failed to start; see stderr log".to_string()),
                vec![format!("Audio engine failed to start: {e:#}")],
            );
            gui::Initial {
                settings: settings.clone(),
                handles,
                devices,
                extra_warnings: Vec::new(),
                keepalive: None,
                compat,
                bvc_extraction_error,
            }
        }
    }
}

#[cfg(not(windows))]
fn bootstrap(settings: &settings::Settings) -> gui::Initial {
    let compat = setup::check_system_compatibility();
    // Extraction is exercised here too (portable logic, see `setup.rs`) so
    // that a native `cargo check`/`cargo test` run still type-checks and
    // exercises this path, even though it's never real on this platform.
    let bvc_dir = setup::app_data_dir();
    let bvc_extraction_error = setup::ensure_bvc_assets_extracted(&bvc_dir).err().map(|e| format!("{e:#}"));
    let handles = shared::EngineHandles::new_from_settings(
        settings,
        false,
        Some("not running on Windows".to_string()),
        false,
        Some("not running on Windows".to_string()),
        Vec::new(),
    );
    gui::Initial {
        settings: settings.clone(),
        handles,
        devices: shared::DeviceList::empty(),
        extra_warnings: vec![
            "Not running on Windows: the real audio engine (WASAPI capture/render, virtual \
             cable I/O, BVC) is unavailable on this platform. This window is a portable-layer \
             type-check/preview only - see the project report for what has and hasn't been \
             verified."
                .to_string(),
        ],
        keepalive: None,
        compat,
        bvc_extraction_error,
    }
}

fn main() -> iced::Result {
    // Must happen before *anything* that touches WASAPI on this thread -
    // `bootstrap()` enumerates devices and runs each pipeline's "does a
    // matching device exist" pre-check on this very thread before spawning
    // any worker thread (each worker thread separately calls this itself
    // internally). Missing this produced a real, observed failure on actual
    // Windows hardware: "CoInitialize has not been called (0x800401F0)" on
    // both device enumeration and default mic capture startup.
    #[cfg(windows)]
    audio_io::initialize_com_for_this_thread();

    // `--diagnose`: print a one-shot report and exit, never starting the
    // GUI or the real audio engine. Checked before `logging::init` on
    // purpose - a diagnostic run's own output goes to the console it was
    // launched from (see `diagnose::run`), not the log file.
    if diagnose::requested() {
        diagnose::run();
        return Ok(());
    }

    logging::init(&setup::app_data_dir());

    // `audio-io`'s capture/render/loopback worker threads used to report
    // their own fatal errors (a WASAPI device disappearing, an event wait
    // timing out, a write to a dead device) via a bare `eprintln!`, which
    // this `#![windows_subsystem = "windows"]` binary has no console to
    // ever show - the thread would silently die while its ring-buffer
    // partner kept logging generic "buffer full, dropping frame" errors
    // forever with no visible root cause. Redirect them into the same
    // file-backed logger everything else in this crate uses.
    audio_io::set_error_sink(|msg| crate::log_error!("{msg}"));

    let settings = settings::load();
    let initial = bootstrap(&settings);
    gui::run(initial)
}
