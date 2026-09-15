//! `clearnairt.exe --diagnose`: a one-shot, non-GUI report of this
//! machine's audio/BVC setup, printed to the console rather than the GUI.
//!
//! Real constraint this has to work around: the binary is built with
//! `#![windows_subsystem = "windows"]` (see `main.rs`), which means it has
//! *no* console by default - `println!`/`eprintln!` go nowhere, even when
//! launched from PowerShell or `cmd.exe`, exactly like every other GUI
//! subsystem app. `run` calls `AttachConsole(ATTACH_PARENT_PROCESS)` first
//! so `--diagnose` output actually reaches the terminal that launched it,
//! instead of silently doing nothing (which would otherwise look identical
//! to a hang).
//!
//! This intentionally does NOT start the real audio engine (`engine::start`,
//! which spawns permanent WASAPI/pipeline threads) - a diagnostic command
//! should be quick, side-effect-free, and safe to run repeatedly. That means
//! live figures (average/max per-frame processing time, dropped frames,
//! under/overruns) are not available here; those are only ever produced by
//! the real running pipeline (`dsp_core::LatencyLog`/`FrameCounters`,
//! surfaced today via the GUI's realtime-status indicator) - this mode says
//! so explicitly rather than fabricating numbers.

use std::path::Path;

/// True if `--diagnose` is one of the process's own arguments.
pub fn requested() -> bool {
    std::env::args().any(|a| a == "--diagnose")
}

/// Runs the diagnostic report and returns. Callers should exit the process
/// immediately after (see `main.rs`) - this never starts the GUI or the
/// audio engine.
pub fn run() {
    #[cfg(windows)]
    attach_parent_console();

    println!("ClearNAI diagnostic report");
    println!("==========================");
    println!("Application version: {}", env!("CARGO_PKG_VERSION"));
    println!();

    print_system_section();
    println!();
    print_audio_contract_section();
    println!();
    print_bvc_section();
    println!();
    print_devices_section();
    println!();
    print_settings_section();
    println!();
    println!(
        "Live processing stats (avg/max frame time, dropped frames, under/overruns) are only \
         produced by the real running pipeline, not this one-shot check - see the app's \
         realtime-status indicator while it's running."
    );
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: no arguments beyond a plain constant; failure (e.g. launched
    // by double-click, with no parent console to attach to) is harmless and
    // deliberately ignored - output then simply goes nowhere, same as
    // before this function existed, rather than crashing.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

fn print_system_section() {
    println!("-- System --");
    #[cfg(windows)]
    {
        let compat = crate::setup::check_system_compatibility();
        println!("OS: Windows (compatible: {})", compat.os_ok);
        println!("Architecture compatible: {}", compat.arch_ok);
        println!("COM initialization: {}", if compat.com_ok { "ok" } else { "FAILED" });
        for d in &compat.details {
            println!("  note: {d}");
        }
    }
    #[cfg(not(windows))]
    {
        println!("OS: not Windows - the real audio engine does not exist on this platform build.");
    }
}

fn print_audio_contract_section() {
    println!("-- Audio contract (fixed, not runtime-negotiated) --");
    println!("Sample rate: {} Hz", dsp_core::SAMPLE_RATE_HZ);
    println!("Frame size: {} samples", dsp_core::FRAME_SAMPLES);
    println!(
        "Frame duration: {:.2} ms",
        1000.0 * dsp_core::FRAME_SAMPLES as f64 / dsp_core::SAMPLE_RATE_HZ as f64
    );
    println!("Channels: 1 (mono)");
    println!("Sample format: f32, -1.0..=1.0");
    println!("Audio backend: WASAPI (shared mode, event-driven)");
}

fn print_bvc_section() {
    println!("-- BVC --");
    let bvc_dir = crate::setup::app_data_dir();
    println!("BVC assets directory: {}", bvc_dir.display());
    report_bvc_extraction(&bvc_dir);
    report_bvc_load("Mic path", &bvc_dir, bvc_hush::DLL_FILE_NAME);
    report_bvc_load("Speaker path", &bvc_dir, bvc_hush::SPEAKER_DLL_FILE_NAME);
}

fn report_bvc_extraction(bvc_dir: &Path) {
    match crate::setup::ensure_bvc_assets_extracted(bvc_dir) {
        Ok(outcome) => {
            println!("Mic DLL ({}): {:?}", bvc_hush::DLL_FILE_NAME, outcome.dll);
            println!("Speaker DLL ({}): {:?}", bvc_hush::SPEAKER_DLL_FILE_NAME, outcome.speaker_dll);
            println!("Model bundle ({}): {:?}", bvc_hush::MODEL_BUNDLE_FILE_NAME, outcome.model);
        }
        Err(e) => println!("Asset extraction FAILED: {e:#}"),
    }
}

fn report_bvc_load(label: &str, bvc_dir: &Path, dll_file_name: &str) {
    match bvc_hush::HushBvcStage::try_load_named(bvc_dir, dll_file_name) {
        Ok(stage) => {
            println!(
                "{label}: loaded OK (native frame length: {}, model sample rate: {} Hz)",
                stage.native_frame_length(),
                stage.model_sample_rate_hz()
            );
        }
        Err(e) => println!("{label}: UNAVAILABLE - {e}"),
    }
}

fn print_devices_section() {
    println!("-- Devices --");
    #[cfg(windows)]
    {
        let devices = crate::engine::enumerate_devices();
        println!("Render devices ({}):", devices.render.len());
        for d in &devices.render {
            println!("  {} [{}]", d.label, d.id);
        }
        println!("Capture devices ({}):", devices.capture.len());
        for d in &devices.capture {
            println!("  {} [{}]", d.label, d.id);
        }
    }
    #[cfg(not(windows))]
    {
        println!("Not running on Windows - no real devices to enumerate.");
    }
}

fn print_settings_section() {
    println!("-- Persisted settings --");
    let settings = crate::settings::load();
    println!("Virtual mic target device: {:?}", settings.virtual_mic_target_id);
    println!("Virtual speaker source device: {:?}", settings.virtual_speaker_source_id);
    println!("Physical mic device: {:?}", settings.physical_mic_device_id);
    println!("Monitor device: {:?}", settings.monitor_device_id);
    println!("Mic noise/BVC/studio on: {}/{}/{}", settings.mic_noise_on, settings.mic_bvc_on, settings.mic_studio_on);
    println!(
        "Speaker noise/BVC/studio on: {}/{}/{}",
        settings.speaker_noise_on, settings.speaker_bvc_on, settings.speaker_studio_on
    );
    println!("AEC on: {}", settings.aec_on);
}
