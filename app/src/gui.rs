//! The single native window: mic-path and speaker-path stage togglers (each
//! path independently gating RNNoise/BVC/Studio, plus a live Studio preset
//! picker per path), AEC and Speaker Tap togglers, a live realtime-status/
//! level indicator, and two device pickers for the virtual-cable roles
//! documented in `docs/VIRTUAL_DEVICES.md`.
//!
//! Built against `iced` 0.14's `iced::application(boot, update, view)`
//! builder API (confirmed current on docs.rs/crates.io on 2026-09-11 - see
//! the top-level report for the exact pages read; the older `Application`/
//! `Sandbox` trait API from pre-0.13 iced does not exist in this version).
//!
//! This module is portable: it only depends on `dsp_core` (for
//! `RealtimeStatus`/percentiles) and `crate::shared`/`crate::settings`
//! (also portable), never on `audio_io` or `engine` directly. `main.rs`
//! constructs the real (Windows) or inert (elsewhere) engine handles and
//! hands them in, so this file - and therefore the whole GUI layer - type
//! checks and its logic can in principle run on any platform `iced`
//! supports, even though the audio behind it is Windows-only.

use crate::setup::CompatibilityReport;
use crate::settings::Settings;
use crate::shared::{
    studio_preset_from_index, studio_preset_name, studio_preset_to_index, DeviceList, DeviceOption,
    EngineHandles, STUDIO_PRESET_NAMES,
};
use crate::status_ui::{status_color, status_label};
use dsp_core::{classify_status, compute_percentiles, RealtimeStatus};
use iced::widget::{button, column, container, pick_list, progress_bar, row, scrollable, text, toggler};
use iced::{Color, Element, Length, Subscription};
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

/// Frame deadline in nanoseconds (10ms @ 48kHz), matching `dsp_core`'s frame
/// config - duplicated here (rather than importing a private constant) since
/// `dsp_core` only exposes `FRAME_SAMPLES`/`SAMPLE_RATE_HZ`, not a
/// precomputed deadline.
const DEADLINE_NS: u64 = (dsp_core::FRAME_SAMPLES as u64 * 1_000_000_000) / dsp_core::SAMPLE_RATE_HZ as u64;

/// Everything `gui::run` needs to boot the window, gathered in `main.rs`.
/// The audio engine (Windows) / inert stub (elsewhere) is always started up
/// front exactly as before `setup.rs` existed - a first-run setup screen
/// bolted in front of it never changes `engine.rs`'s bootstrap timing, it
/// only changes which screen is shown first.
pub struct Initial {
    pub settings: Settings,
    pub handles: EngineHandles,
    pub devices: DeviceList,
    /// Extra non-fatal warnings gathered by `main.rs` itself (e.g. "not
    /// running on Windows"), shown alongside any the engine collected.
    pub extra_warnings: Vec<String>,
    /// Kept alive only so the real engine's OS threads (mic capture,
    /// render, loopback tap) live exactly as long as this window does - the
    /// GUI never inspects it. `None` on a platform with no real engine.
    pub keepalive: Option<Box<dyn std::any::Any + Send>>,
    /// Fresh (fast, synchronous) system compatibility check, run in
    /// `main.rs` before the window is created.
    pub compat: CompatibilityReport,
    /// `Some(message)` if `main.rs::bootstrap` failed to self-extract the
    /// embedded BVC DLL/model into `%LOCALAPPDATA%` (disk full, permission
    /// denied, etc.) - `None` means extraction succeeded (or was already
    /// up to date), i.e. the ordinary, expected case that needs no mention
    /// on the setup screen at all.
    pub bvc_extraction_error: Option<String>,
}

struct MainState {
    settings: Settings,
    handles: EngineHandles,
    devices: DeviceList,
    warnings: Vec<String>,
    status: RealtimeStatus,
    level: f32,
    _keepalive: Option<Box<dyn std::any::Any + Send>>,
}

struct SetupState {
    settings: Settings,
    compat: CompatibilityReport,
    /// `Some(message)` if embedded-BVC-asset extraction failed; see
    /// `Initial::bvc_extraction_error`.
    bvc_extraction_error: Option<String>,
    // Kept only to hand off to `MainState` once the user continues past
    // setup - never read directly by the setup screen itself.
    handles: EngineHandles,
    devices: DeviceList,
    warnings: Vec<String>,
    keepalive: Option<Box<dyn std::any::Any + Send>>,
}

enum Screen {
    Setup(SetupState),
    Main(MainState),
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    ToggleMicNoise(bool),
    ToggleMicBvc(bool),
    ToggleMicStudio(bool),
    SelectMicStudioPreset(&'static str),
    ToggleAec(bool),
    ToggleSpeakerNoise(bool),
    ToggleSpeakerBvc(bool),
    ToggleSpeakerStudio(bool),
    SelectSpeakerStudioPreset(&'static str),
    ToggleSpeakerTap(bool),
    SelectMicTarget(DeviceOption),
    SelectSpeakerSource(DeviceOption),
    SelectPhysicalMic(DeviceOption),
    ToggleMonitor(bool),
    SelectMonitorDevice(DeviceOption),
    /// Re-enumerates devices (e.g. after starting VoiceMeter/VB-Cable
    /// *after* ClearNAI) without restarting the app. See
    /// `LiveDeviceSwitcher::enumerate_devices`.
    RefreshDevices,
    /// Setup screen: "Continue" / "Continue anyway" clicked.
    ContinueFromSetup,
}

fn persist(settings: &Settings) {
    if let Err(e) = crate::settings::save(settings) {
        crate::log_error!("[clearnairt] failed to persist settings: {e:#}");
    }
}

/// Runs a `LiveDeviceSwitcher` call on a background thread rather than
/// inline in `update_main` (iced's UI thread). `LiveAudioSwitcher::set_*`
/// opens a fresh WASAPI device and then blocks joining the *old* worker
/// thread - real hardware log showed that join can take anywhere from the
/// event-wait timeout up to (with `audio-io`'s reconnect-on-failure loop)
/// however long the old thread's in-flight device-open attempt takes to
/// return, which can be much longer than a UI frame. Calling it inline froze
/// the whole window ("Not Responding") on every device switch. `switcher` is
/// `Arc`-shared and `Send + Sync`, so this is safe to run off-thread; any
/// failure is already logged by `LiveAudioSwitcher` itself.
fn spawn_device_switch(
    switcher: Arc<dyn crate::shared::LiveDeviceSwitcher>,
    call: impl FnOnce(&dyn crate::shared::LiveDeviceSwitcher) + Send + 'static,
) {
    std::thread::spawn(move || call(switcher.as_ref()));
}

/// Top-level dispatch: routes to whichever screen is currently active, and
/// handles the one-way `Setup -> Main` transition when `update_setup`
/// reports the user is done with the setup screen.
fn update(screen: &mut Screen, message: Message) {
    let transition = match screen {
        Screen::Main(state) => {
            update_main(state, message);
            None
        }
        Screen::Setup(setup) => update_setup(setup, message),
    };
    if let Some(main_state) = transition {
        *screen = Screen::Main(main_state);
    }
}

/// Handles the setup screen's messages. There is no background work left to
/// poll here (BVC asset extraction is synchronous and already finished
/// before this screen is ever shown - see `main.rs::bootstrap`), so `Tick`
/// is a no-op on this screen; the only real transition is the user clicking
/// through. Returns `Some(MainState)` exactly then, signalling the
/// top-level `update` to switch screens.
fn update_setup(setup: &mut SetupState, message: Message) -> Option<MainState> {
    match message {
        Message::Tick => None,
        Message::ContinueFromSetup => {
            // Only the compatibility-warning half of the setup screen is
            // ever suppressed by this flag on future launches - a missing
            // model is independently re-checked fresh every launch (see
            // `Settings::setup_acknowledged`'s docs), so "Skip" here never
            // hides a real, currently-true problem, only an
            // already-acknowledged one.
            setup.settings.setup_acknowledged = true;
            persist(&setup.settings);
            Some(MainState {
                settings: setup.settings.clone(),
                handles: setup.handles.clone(),
                devices: setup.devices.clone(),
                warnings: setup.warnings.clone(),
                status: RealtimeStatus::Bypass,
                level: 0.0,
                _keepalive: setup.keepalive.take(),
            })
        }
        // Every other message is a Main-screen-only toggle/picker action;
        // the setup screen's `view` never emits these, so this is
        // unreachable in practice, not a silently-swallowed real message.
        _ => None,
    }
}

fn update_main(state: &mut MainState, message: Message) {
    match message {
        Message::Tick => {
            let snapshot = state.handles.latency_log.snapshot();
            let percentiles = compute_percentiles(snapshot);
            // "Bypass" here means every mic-path DSP stage is currently
            // off, i.e. the mic pipeline is doing pure passthrough - a
            // reasonable, documented interpretation of `classify_status`'s
            // `bypass` flag, which `dsp_core` leaves for the caller to
            // define. AEC is intentionally excluded from this check since
            // it defaults off and is not one of the three "cleanup" stages
            // the status indicator is about.
            let all_off = !state.handles.mic_noise.is_on()
                && !state.handles.mic_bvc.is_on()
                && !state.handles.mic_studio.is_on();
            state.status = classify_status(&percentiles, DEADLINE_NS, all_off);
            state.level = state.handles.level.load();
        }
        Message::ToggleMicNoise(on) => {
            state.handles.mic_noise.set(on);
            state.settings.mic_noise_on = on;
            persist(&state.settings);
        }
        Message::ToggleMicBvc(on) => {
            if state.handles.mic_bvc_available {
                state.handles.mic_bvc.set(on);
                state.settings.mic_bvc_on = on;
                persist(&state.settings);
            }
        }
        Message::ToggleMicStudio(on) => {
            state.handles.mic_studio.set(on);
            state.settings.mic_studio_on = on;
            persist(&state.settings);
        }
        Message::SelectMicStudioPreset(name) => {
            if let Some(preset) = crate::shared::studio_preset_from_name(name) {
                let index = studio_preset_to_index(preset);
                state.handles.mic_studio_preset.store(index);
                state.settings.mic_studio_preset = index;
                persist(&state.settings);
            }
        }
        Message::ToggleAec(on) => {
            state.handles.aec.set(on);
            state.settings.aec_on = on;
            persist(&state.settings);
        }
        Message::ToggleSpeakerNoise(on) => {
            state.handles.speaker_noise.set(on);
            state.settings.speaker_noise_on = on;
            persist(&state.settings);
        }
        Message::ToggleSpeakerBvc(on) => {
            if state.handles.speaker_bvc_available {
                state.handles.speaker_bvc.set(on);
                state.settings.speaker_bvc_on = on;
                persist(&state.settings);
            }
        }
        Message::ToggleSpeakerStudio(on) => {
            state.handles.speaker_studio.set(on);
            state.settings.speaker_studio_on = on;
            persist(&state.settings);
        }
        Message::SelectSpeakerStudioPreset(name) => {
            if let Some(preset) = crate::shared::studio_preset_from_name(name) {
                let index = studio_preset_to_index(preset);
                state.handles.speaker_studio_preset.store(index);
                state.settings.speaker_studio_preset = index;
                persist(&state.settings);
            }
        }
        Message::ToggleSpeakerTap(on) => {
            state.handles.speaker_tap.set(on);
            state.settings.speaker_tap_on = on;
            persist(&state.settings);
        }
        Message::SelectMicTarget(device) => {
            state.settings.virtual_mic_target_id = Some(device.id.clone());
            persist(&state.settings);
            // Live switch, not just a next-launch preference: tears down the
            // render thread on the previous virtual-mic-target device (if
            // any) and starts a fresh one on `device.id`, reconnecting the
            // mic pipeline thread's output to a new ring buffer. See
            // `engine::LiveAudioSwitcher::set_virtual_mic_target`. Run off
            // the UI thread - see `spawn_device_switch`.
            spawn_device_switch(state.handles.device_switcher.clone(), move |s| {
                s.set_virtual_mic_target(Some(device.id))
            });
        }
        Message::SelectSpeakerSource(device) => {
            state.settings.virtual_speaker_source_id = Some(device.id.clone());
            persist(&state.settings);
            spawn_device_switch(state.handles.device_switcher.clone(), move |s| {
                s.set_virtual_speaker_source(Some(device.id))
            });
        }
        Message::SelectPhysicalMic(device) => {
            state.settings.physical_mic_device_id = Some(device.id.clone());
            persist(&state.settings);
            spawn_device_switch(state.handles.device_switcher.clone(), move |s| {
                s.set_physical_mic(Some(device.id))
            });
        }
        Message::ToggleMonitor(on) => {
            state.handles.monitor.set(on);
            state.settings.monitor_enabled = on;
            persist(&state.settings);
        }
        Message::SelectMonitorDevice(device) => {
            state.settings.monitor_device_id = Some(device.id.clone());
            persist(&state.settings);
            spawn_device_switch(state.handles.device_switcher.clone(), move |s| {
                s.set_monitor_device(Some(device.id))
            });
        }
        Message::RefreshDevices => {
            // Enumeration itself (unlike a device switch) never opens a
            // device or joins a worker thread, so it's cheap and safe to run
            // inline on the UI thread rather than needing
            // `spawn_device_switch`'s background-thread treatment.
            state.devices = state.handles.device_switcher.enumerate_devices();
        }
        // Setup-screen-only messages; unreachable once `Screen::Main` is
        // active (its `view` never emits them), same reasoning as the
        // catch-all in `update_setup`.
        Message::ContinueFromSetup => {}
    }
}

fn status_row(state: &MainState) -> Element<'_, Message> {
    let color = status_color(state.status);
    let dot = container(text("●").size(20).color(Color::from_rgb(color.r, color.g, color.b)));
    let label = text(format!("Engine: {}", status_label(state.status))).size(16);
    let level_bar = progress_bar(0.0..=1.0, state.level.clamp(0.0, 1.0)).girth(Length::Fixed(10.0));

    column![
        row![dot, label].spacing(8).align_y(iced::Alignment::Center),
        row![text("Input level").size(12), level_bar].spacing(8).align_y(iced::Alignment::Center),
    ]
    .spacing(4)
    .into()
}

fn device_picker<'a>(
    label: &'a str,
    options: &'a [DeviceOption],
    selected_id: &Option<String>,
    on_select: impl Fn(DeviceOption) -> Message + 'a,
) -> Element<'a, Message> {
    let selected = selected_id
        .as_ref()
        .and_then(|id| options.iter().find(|d| &d.id == id).cloned());
    column![
        text(label).size(14),
        pick_list(options, selected, on_select).placeholder("(not selected - applies immediately)"),
    ]
    .spacing(4)
    .into()
}

fn view_main(state: &MainState) -> Element<'_, Message> {
    let mic_bvc_label = if state.handles.mic_bvc_available {
        "BVC (after Noise)".to_string()
    } else {
        format!(
            "BVC (after Noise) - unavailable: {}",
            state
                .handles
                .mic_bvc_unavailable_reason
                .as_deref()
                .unwrap_or("weya_nc.dll not found")
        )
    };

    let speaker_bvc_label = if state.handles.speaker_bvc_available {
        "BVC (outbound, after Noise)".to_string()
    } else {
        format!(
            "BVC (outbound, after Noise) - unavailable: {}",
            state
                .handles
                .speaker_bvc_unavailable_reason
                .as_deref()
                .unwrap_or("weya_nc.dll not found")
        )
    };

    let mic_studio_preset_row = row![
        text("Studio preset").size(12),
        pick_list(
            &STUDIO_PRESET_NAMES[..],
            Some(studio_preset_name(studio_preset_from_index(state.handles.mic_studio_preset.load()))),
            Message::SelectMicStudioPreset,
        ),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let speaker_studio_preset_row = row![
        text("Studio preset").size(12),
        pick_list(
            &STUDIO_PRESET_NAMES[..],
            Some(studio_preset_name(studio_preset_from_index(state.handles.speaker_studio_preset.load()))),
            Message::SelectSpeakerStudioPreset,
        ),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let monitor_section = column![
        text("Monitor - hear the processed mic locally, for testing (not part of the normal virtual-mic/speaker signal path)").size(13),
        toggler(state.handles.monitor.is_on())
            .label("Monitor (hear processed mic locally)")
            .on_toggle(Message::ToggleMonitor),
        device_picker(
            "Monitor output device (e.g. your headphones)",
            &state.devices.render,
            &state.settings.monitor_device_id,
            Message::SelectMonitorDevice,
        ),
    ]
    .spacing(8);

    let toggles = column![
        toggler(state.handles.mic_noise.is_on())
            .label("RNNoise (between Mic and Virtual Mic)")
            .on_toggle(Message::ToggleMicNoise),
        toggler(state.handles.mic_bvc.is_on())
            .label(mic_bvc_label)
            .on_toggle_maybe(state.handles.mic_bvc_available.then_some(Message::ToggleMicBvc as fn(bool) -> Message)),
        toggler(state.handles.mic_studio.is_on())
            .label("Studio")
            .on_toggle(Message::ToggleMicStudio),
        mic_studio_preset_row,
        toggler(state.handles.aec.is_on())
            .label("AEC")
            .on_toggle(Message::ToggleAec),
        toggler(state.handles.speaker_noise.is_on())
            .label("RNNoise (outbound, before Virtual Speaker forward)")
            .on_toggle(Message::ToggleSpeakerNoise),
        toggler(state.handles.speaker_bvc.is_on())
            .label(speaker_bvc_label)
            .on_toggle_maybe(state.handles.speaker_bvc_available.then_some(Message::ToggleSpeakerBvc as fn(bool) -> Message)),
        toggler(state.handles.speaker_studio.is_on())
            .label("Studio (outbound)")
            .on_toggle(Message::ToggleSpeakerStudio),
        speaker_studio_preset_row,
        toggler(state.handles.speaker_tap.is_on())
            .label("Speaker Tap (inbound, read-only)")
            .on_toggle(Message::ToggleSpeakerTap),
    ]
    .spacing(10);

    let devices_heading = row![
        text("Virtual devices (see docs/VIRTUAL_DEVICES.md - two separate cable instances required)").size(13),
        button(text("Refresh devices").size(12)).on_press(Message::RefreshDevices),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center);

    let devices = column![
        devices_heading,
        text("Started VoiceMeter/VB-Cable after ClearNAI? Click Refresh, then (re)select it below - devices are only scanned once at launch.").size(12),
        device_picker(
            "Virtual mic target (render into this device)",
            &state.devices.render,
            &state.settings.virtual_mic_target_id,
            Message::SelectMicTarget,
        ),
        device_picker(
            "Virtual speaker source (capture from this device)",
            &state.devices.capture,
            &state.settings.virtual_speaker_source_id,
            Message::SelectSpeakerSource,
        ),
        device_picker(
            "Physical microphone (real input device)",
            &state.devices.capture,
            &state.settings.physical_mic_device_id,
            Message::SelectPhysicalMic,
        ),
    ]
    .spacing(8);

    let mut warnings_col = column![].spacing(4);
    for w in &state.warnings {
        warnings_col = warnings_col.push(text(format!("\u{26A0} {w}")).size(12).color(Color::from_rgb(0.85, 0.6, 0.1)));
    }

    let content = column![
        text("ClearNAI").size(24),
        status_row(state),
        iced::widget::rule::horizontal(1),
        monitor_section,
        iced::widget::rule::horizontal(1),
        toggles,
        iced::widget::rule::horizontal(1),
        devices,
        warnings_col,
    ]
    .spacing(16)
    .padding(20);

    scrollable(content).into()
}

/// One pass/fail line in the setup screen's compatibility list, reusing the
/// same green/red palette `status_ui::status_color` uses for
/// realtime/overloaded so the whole app reads as one consistent color
/// language rather than inventing a second one here.
fn compat_line<'a>(label: &'a str, ok: bool) -> Element<'a, Message> {
    let color = if ok { Color::from_rgb(0.20, 0.75, 0.30) } else { Color::from_rgb(0.85, 0.20, 0.20) };
    let mark = if ok { "\u{2713}" } else { "\u{2717}" };
    row![
        text(mark).size(16).color(color),
        text(label).size(14),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .into()
}

fn view_setup(setup: &SetupState) -> Element<'_, Message> {
    let compat_col = column![
        text("System compatibility").size(16),
        compat_line("Operating system (Windows 10/11)", setup.compat.os_ok),
        compat_line("CPU architecture (x86_64)", setup.compat.arch_ok),
        compat_line("Audio subsystem (COM)", setup.compat.com_ok),
    ]
    .spacing(6);

    let mut details_col = column![].spacing(2);
    for d in &setup.compat.details {
        details_col = details_col.push(text(d).size(11).color(Color::from_rgb(0.6, 0.6, 0.6)));
    }

    // BVC's DLL + model bundle are embedded in the binary and self-extract
    // synchronously before this screen is ever shown (see
    // `main.rs::bootstrap`) - there is no download/progress step left, only
    // a one-line confirmation, or a clear error if extraction itself
    // failed (disk full, unwritable %LOCALAPPDATA%, etc.).
    let model_col: Element<'_, Message> = match &setup.bvc_extraction_error {
        None => column![
            text("BVC files").size(16),
            compat_line("weya_nc.dll and the BVC model bundle ready", true),
        ]
        .spacing(6)
        .into(),
        Some(err) => column![
            text("BVC files").size(16),
            compat_line("failed to prepare weya_nc.dll / BVC model bundle", false),
            text(err.clone()).size(12).color(Color::from_rgb(0.85, 0.2, 0.2)),
        ]
        .spacing(8)
        .into(),
    };

    let content = column![
        text("ClearNAI - first-run setup").size(24),
        compat_col,
        details_col,
        iced::widget::rule::horizontal(1),
        model_col,
        iced::widget::rule::horizontal(1),
        text(
            "You can continue without BVC (background voice cleanup) - the rest of the \
             pipeline works normally and BVC will simply show as unavailable, exactly like \
             today's behavior when weya_nc.dll or the model file fail to load."
        )
        .size(12)
        .color(Color::from_rgb(0.6, 0.6, 0.6)),
        button(text("Continue")).on_press(Message::ContinueFromSetup),
    ]
    .spacing(16)
    .padding(20);

    scrollable(content).into()
}

fn view(screen: &Screen) -> Element<'_, Message> {
    match screen {
        Screen::Setup(setup) => view_setup(setup),
        Screen::Main(state) => view_main(state),
    }
}

fn subscription(_screen: &Screen) -> Subscription<Message> {
    iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick)
}

/// Runs the GUI. Blocks until the window is closed, at which point the
/// whole process exits (no tray icon, no background daemon - per spec).
pub fn run(initial: Initial) -> iced::Result {
    let cell = RefCell::new(Some(initial));
    iced::application(
        move || {
            let initial = cell.borrow_mut().take().expect("boot is only called once");
            let mut warnings = (*initial.handles.warnings).clone();
            warnings.extend(initial.extra_warnings);

            // Show the setup screen when embedded-BVC-asset extraction
            // failed (always surfaced - never suppressed, since it's a
            // real, currently-true problem each time it happens), or when
            // a compatibility check is failing and the user hasn't already
            // acknowledged that on a previous launch (see
            // `Settings::setup_acknowledged`). Otherwise - the ordinary
            // case, extraction having succeeded instantly and compat
            // checks passing (or already acknowledged) - skip straight to
            // the main screen with no setup UI at all.
            let show_setup = initial.bvc_extraction_error.is_some()
                || (!initial.compat.all_ok() && !initial.settings.setup_acknowledged);

            if show_setup {
                Screen::Setup(SetupState {
                    settings: initial.settings,
                    compat: initial.compat,
                    bvc_extraction_error: initial.bvc_extraction_error,
                    handles: initial.handles,
                    devices: initial.devices,
                    warnings,
                    keepalive: initial.keepalive,
                })
            } else {
                Screen::Main(MainState {
                    settings: initial.settings,
                    handles: initial.handles,
                    devices: initial.devices,
                    warnings,
                    status: RealtimeStatus::Bypass,
                    level: 0.0,
                    _keepalive: initial.keepalive,
                })
            }
        },
        update,
        view,
    )
    .title("ClearNAI")
    .subscription(subscription)
    .window_size(iced::Size::new(560.0, 720.0))
    .run()
}
