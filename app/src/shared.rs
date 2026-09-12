//! Types shared between the audio engine (real on Windows, inert stub
//! elsewhere - see `engine.rs`) and the GUI (`gui.rs`). Nothing in this file
//! touches an OS audio API, so it compiles and its tests run natively on
//! Linux as well as when cross-checked for Windows.

use dsp_core::{FrameCounters, LatencyLog, SharedPreset, Stage, StageToggle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Maps `studio_dsp::StudioPreset` to/from the plain `u8` index
/// `dsp_core::SharedPreset` carries (that crate can't depend on
/// `studio-dsp`'s real enum - wrong dependency direction). Kept in one place
/// so the app crate never encodes/decodes this mapping ad hoc.
pub const STUDIO_PRESET_NAMES: [&str; 4] = ["Off", "Natural", "Balanced", "Strong"];

pub fn studio_preset_to_index(preset: studio_dsp::StudioPreset) -> u8 {
    match preset {
        studio_dsp::StudioPreset::Off => 0,
        studio_dsp::StudioPreset::Natural => 1,
        studio_dsp::StudioPreset::Balanced => 2,
        studio_dsp::StudioPreset::Strong => 3,
    }
}

pub fn studio_preset_from_index(index: u8) -> studio_dsp::StudioPreset {
    match index {
        0 => studio_dsp::StudioPreset::Off,
        2 => studio_dsp::StudioPreset::Balanced,
        3 => studio_dsp::StudioPreset::Strong,
        // Anything else (including the expected `1`) - `Natural` is the
        // documented default, so an unrecognized index degrades to it
        // rather than panicking.
        _ => studio_dsp::StudioPreset::Natural,
    }
}

pub fn studio_preset_name(preset: studio_dsp::StudioPreset) -> &'static str {
    STUDIO_PRESET_NAMES[studio_preset_to_index(preset) as usize]
}

pub fn studio_preset_from_name(name: &str) -> Option<studio_dsp::StudioPreset> {
    match name {
        "Off" => Some(studio_dsp::StudioPreset::Off),
        "Natural" => Some(studio_dsp::StudioPreset::Natural),
        "Balanced" => Some(studio_dsp::StudioPreset::Balanced),
        "Strong" => Some(studio_dsp::StudioPreset::Strong),
        _ => None,
    }
}

/// Lock-free single-value f32 "mailbox", used to publish the most recent
/// processed frame's peak level from the realtime mic pipeline thread to the
/// GUI's periodic poll. Stored as raw bits in an `AtomicU32` so the store/
/// load are plain atomic ops with no lock, matching this project's realtime
/// rules (the writer side runs on the mic pipeline thread).
pub struct LevelMeter(AtomicU32);

impl LevelMeter {
    pub fn new() -> Self {
        Self(AtomicU32::new(0f32.to_bits()))
    }

    #[inline(always)]
    pub fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// A true no-op `Stage`, used in place of a real BVC (or, in principle, any
/// other stage) instance when it genuinely could not be loaded/constructed.
/// This exists so the pipeline never has to special-case "this stage might
/// be `None`" - every slot in the chain always holds a real `Box<dyn Stage>`,
/// constructed at startup, per the project's "always warm, never lazily
/// constructed" rule. Whether it does anything is controlled the normal way
/// (a `StageToggle`, forced off and disabled in the GUI when this is what
/// got constructed instead of the real thing) - never a silently-do-nothing
/// flag pretending to be a working feature.
pub struct NoOpStage(pub &'static str);

impl Stage for NoOpStage {
    fn name(&self) -> &'static str {
        self.0
    }

    #[inline(always)]
    fn process(&mut self, _frame: &mut [f32]) {}
}

/// Live device-switching interface for the four device-picker roles
/// ("Physical microphone", "Virtual mic target", "Virtual speaker source",
/// "Monitor output"). Implemented for real by the Windows engine
/// (`engine::LiveAudioSwitcher`, which actually stops/restarts the affected
/// WASAPI thread and reconnects it to a fresh ring buffer) and as a no-op
/// (`NoOpDeviceSwitcher`) everywhere else - defined here (portable) rather
/// than in `engine.rs` (Windows-only) so `gui.rs` can call it without
/// depending on `audio_io`/`engine` directly, matching this file's existing
/// "shared, portable types" role.
///
/// `device_id = None` means "system default" for the physical-mic role and
/// "not configured / disabled" for the other three - mirroring exactly how
/// `Settings`'s corresponding `Option<String>` fields are already
/// interpreted at next-launch time in `engine::start`.
pub trait LiveDeviceSwitcher: Send + Sync {
    fn set_physical_mic(&self, device_id: Option<String>);
    fn set_virtual_mic_target(&self, device_id: Option<String>);
    fn set_virtual_speaker_source(&self, device_id: Option<String>);
    fn set_monitor_device(&self, device_id: Option<String>);
}

/// The non-Windows (and startup-failure) stand-in: every call is a no-op.
/// There is no real audio engine underneath in either case, so there is
/// nothing to switch.
pub struct NoOpDeviceSwitcher;

impl LiveDeviceSwitcher for NoOpDeviceSwitcher {
    fn set_physical_mic(&self, _device_id: Option<String>) {}
    fn set_virtual_mic_target(&self, _device_id: Option<String>) {}
    fn set_virtual_speaker_source(&self, _device_id: Option<String>) {}
    fn set_monitor_device(&self, _device_id: Option<String>) {}
}

/// Live handles shared between the audio engine thread(s) and the GUI.
/// Constructed exactly once at startup and cloned (cheap: everything inside
/// is an `Arc`/`StageToggle`, itself an `Arc<AtomicBool>`) into the GUI's
/// `State`. Toggling a switch in the GUI flips the corresponding
/// `StageToggle` in place - the pipeline is never torn down or rebuilt.
#[derive(Clone)]
pub struct EngineHandles {
    /// Mic-path RNNoise, "RNNoise (between Mic and Virtual Mic)".
    pub mic_noise: StageToggle,
    /// Mic-path BVC, "BVC (after Noise)". See `bvc_available`.
    pub mic_bvc: StageToggle,
    /// Mic-path Studio stage, "Studio".
    pub mic_studio: StageToggle,
    /// Live preset selection for the mic-path Studio stage, encoded via
    /// `studio_preset_to_index`/`studio_preset_from_index`. See `engine.rs`'s
    /// mic pipeline loop for the once-per-frame load + swap-on-change
    /// pattern.
    pub mic_studio_preset: SharedPreset,
    /// AEC (mic + loopback reference), "AEC". Default OFF.
    pub aec: StageToggle,
    /// Speaker outbound path, "RNNoise (outbound, before Virtual Speaker
    /// forward)" - independent of `speaker_bvc`/`speaker_studio`, mirroring
    /// the mic side's per-stage toggles.
    pub speaker_noise: StageToggle,
    /// Speaker outbound path, "BVC (outbound, after Noise)". See
    /// `bvc_available` - forced off the same way `mic_bvc` is when BVC could
    /// not be loaded (the flag is a DLL/model property, not per-pipeline).
    pub speaker_bvc: StageToggle,
    /// Speaker outbound path, "Studio (outbound)".
    pub speaker_studio: StageToggle,
    /// Live preset selection for the speaker-path Studio stage - wholly
    /// independent of `mic_studio_preset` (not synced).
    pub speaker_studio_preset: SharedPreset,
    /// Whether the hardware-output loopback tap's samples are forwarded to
    /// anything downstream (currently: the AEC reference input), "Speaker
    /// Tap (inbound, read-only)". This is *not* a DSP bypass - the tap
    /// itself is a read-only WASAPI loopback capture that never touches
    /// playback, so there is nothing to "bypass" on the playback side. When
    /// off, the tap's real OS capture thread keeps running (starting/
    /// stopping a WASAPI stream on every toggle flip would itself be a kind
    /// of live pipeline rebuild), but its samples are discarded rather than
    /// handed to AEC - see `engine.rs`'s mic pipeline loop for exactly where
    /// this is applied.
    pub speaker_tap: StageToggle,

    /// "Monitor (hear processed mic locally)" - a diagnostic/testing-only
    /// path, wholly independent of the virtual-mic-target pipeline. When on
    /// (and a monitor output device has been configured - see
    /// `Settings::monitor_device_id`), the mic pipeline thread copies its
    /// fully-processed frame (after AEC/Noise/BVC/Studio) into a second
    /// render path so the user can hear the effect of those stages directly,
    /// without needing VB-Cable installed. When off, real silence is
    /// rendered to the monitor device instead of the processed signal (the
    /// render thread itself is never torn down/rebuilt on a toggle flip,
    /// matching every other `StageToggle` in this struct).
    pub monitor: StageToggle,

    /// Whether `bvc_hush::HushBvcStage::try_load` succeeded at startup. When
    /// `false`, `mic_bvc` has been forced to `false` and the GUI must render
    /// its toggle disabled (never silently ignoring a user's attempt to turn
    /// it on).
    pub bvc_available: bool,
    /// Human-readable reason BVC is unavailable (e.g. "weya_nc.dll not
    /// found next to ... "), for a tooltip/label next to the disabled
    /// toggle. `None` when `bvc_available` is `true`.
    pub bvc_unavailable_reason: Option<String>,

    /// Non-fatal startup warnings (e.g. "no virtual speaker source device
    /// selected; speaker outbound pipeline not started") surfaced once in
    /// the GUI.
    pub warnings: Arc<Vec<String>>,

    pub latency_log: Arc<LatencyLog>,
    /// Read by the real Windows engine's mic pipeline thread only
    /// (`engine.rs`); not yet surfaced in the GUI beyond the derived
    /// realtime-status indicator, so a non-Windows build (which never
    /// constructs `engine.rs`) sees this field as unread. That is expected,
    /// not a bug.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub counters: Arc<FrameCounters>,
    pub level: Arc<LevelMeter>,

    /// Live device-switching handle for the four device pickers - see
    /// `LiveDeviceSwitcher`. Defaults to `NoOpDeviceSwitcher` in
    /// `new_from_settings`; the real Windows engine (`engine::start`)
    /// overwrites this with the real `Arc<engine::LiveAudioSwitcher>` after
    /// construction, once the actual WASAPI threads/ring buffers it needs to
    /// switch between exist.
    pub device_switcher: Arc<dyn LiveDeviceSwitcher>,
}

impl EngineHandles {
    /// Builds the toggle set from persisted `Settings`, honestly reflecting
    /// BVC availability (`bvc_available` forces `mic_bvc` off if BVC could
    /// not be loaded, regardless of the persisted preference).
    pub fn new_from_settings(
        settings: &crate::settings::Settings,
        bvc_available: bool,
        bvc_unavailable_reason: Option<String>,
        warnings: Vec<String>,
    ) -> Self {
        // RNNoise and BVC are two independent ML denoisers; running both at
        // once on the same path double-processes the signal (each adds its
        // own buffering latency - BVC's native frame length isn't
        // guaranteed to match the host's 480 samples, see
        // `bvc_hush::FrameAdapter` - and stacking denoisers tends to smear/
        // re-trigger noise artifacts). They are mutually exclusive by
        // construction: if a persisted settings file somehow has both on
        // (e.g. from before this rule existed), RNNoise wins and BVC is
        // forced off here, mirroring the live-toggle behavior in
        // `gui::update`.
        let mic_bvc_on = settings.mic_bvc_on && bvc_available && !settings.mic_noise_on;
        let speaker_bvc_on = settings.speaker_bvc_on && bvc_available && !settings.speaker_noise_on;

        Self {
            mic_noise: StageToggle::new(settings.mic_noise_on),
            mic_bvc: StageToggle::new(mic_bvc_on),
            mic_studio: StageToggle::new(settings.mic_studio_on),
            mic_studio_preset: SharedPreset::new(settings.mic_studio_preset),
            aec: StageToggle::new(settings.aec_on),
            speaker_noise: StageToggle::new(settings.speaker_noise_on),
            speaker_bvc: StageToggle::new(speaker_bvc_on),
            speaker_studio: StageToggle::new(settings.speaker_studio_on),
            speaker_studio_preset: SharedPreset::new(settings.speaker_studio_preset),
            speaker_tap: StageToggle::new(settings.speaker_tap_on),
            monitor: StageToggle::new(settings.monitor_enabled),
            bvc_available,
            bvc_unavailable_reason,
            warnings: Arc::new(warnings),
            latency_log: Arc::new(LatencyLog::new(1024)),
            counters: Arc::new(FrameCounters::default()),
            level: Arc::new(LevelMeter::new()),
            device_switcher: Arc::new(NoOpDeviceSwitcher),
        }
    }
}

/// One enumerated WASAPI endpoint, as shown in a GUI device picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    pub id: String,
    pub label: String,
}

impl std::fmt::Display for DeviceOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

/// Render/capture device lists as shown in the GUI's two device pickers.
/// Empty on any platform where enumeration isn't available (non-Windows, or
/// a real enumeration failure) - the GUI must degrade to "no devices
/// listed", never crash.
#[derive(Debug, Clone, Default)]
pub struct DeviceList {
    pub render: Vec<DeviceOption>,
    pub capture: Vec<DeviceOption>,
}

impl DeviceList {
    #[cfg_attr(windows, allow(dead_code))]
    pub fn empty() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_meter_round_trips_arbitrary_f32_via_bit_pattern() {
        let meter = LevelMeter::new();
        for v in [0.0f32, 1.0, -1.0, 0.5, 0.999_999, -0.000_1, f32::MIN_POSITIVE] {
            meter.store(v);
            assert_eq!(meter.load(), v);
        }
    }

    #[test]
    fn noop_stage_never_mutates_the_frame() {
        let mut stage = NoOpStage("bvc unavailable");
        let mut frame = [0.1, -0.2, 0.3, 0.0];
        let original = frame;
        stage.process(&mut frame);
        assert_eq!(frame, original);
        assert_eq!(stage.name(), "bvc unavailable");
    }

    #[test]
    fn bvc_unavailable_forces_toggle_off_regardless_of_saved_preference() {
        let mut settings = crate::settings::Settings::default();
        settings.mic_bvc_on = true; // user's saved preference is "on"
        let handles = EngineHandles::new_from_settings(
            &settings,
            false,
            Some("weya_nc.dll not found".into()),
            Vec::new(),
        );
        assert!(!handles.bvc_available);
        assert!(!handles.mic_bvc.is_on(), "BVC must be forced off when unavailable, never silently left on");
    }

    #[test]
    fn bvc_available_honors_saved_preference() {
        let mut settings = crate::settings::Settings::default();
        settings.mic_bvc_on = false;
        let handles = EngineHandles::new_from_settings(&settings, true, None, Vec::new());
        assert!(handles.bvc_available);
        assert!(!handles.mic_bvc.is_on());
    }

    #[test]
    fn mic_noise_and_bvc_are_mutually_exclusive_on_construction() {
        let mut settings = crate::settings::Settings::default();
        settings.mic_noise_on = true;
        settings.mic_bvc_on = true; // both saved on from before this rule existed
        let handles = EngineHandles::new_from_settings(&settings, true, None, Vec::new());
        assert!(handles.mic_noise.is_on(), "RNNoise wins when both were saved on");
        assert!(!handles.mic_bvc.is_on(), "BVC must be forced off to avoid double-processing");
    }

    #[test]
    fn speaker_noise_and_bvc_are_mutually_exclusive_on_construction() {
        let mut settings = crate::settings::Settings::default();
        settings.speaker_noise_on = true;
        settings.speaker_bvc_on = true;
        let handles = EngineHandles::new_from_settings(&settings, true, None, Vec::new());
        assert!(handles.speaker_noise.is_on());
        assert!(!handles.speaker_bvc.is_on());
    }

    #[test]
    fn device_list_empty_has_no_devices() {
        let list = DeviceList::empty();
        assert!(list.render.is_empty());
        assert!(list.capture.is_empty());
    }

    #[test]
    fn no_op_device_switcher_accepts_every_call_without_panicking() {
        // The non-Windows (and engine-start-failure) stand-in for
        // `LiveDeviceSwitcher` - there is no real engine underneath, so
        // every call must be inert, never panic, regardless of Some/None.
        let switcher = NoOpDeviceSwitcher;
        switcher.set_physical_mic(Some("device-1".to_string()));
        switcher.set_physical_mic(None);
        switcher.set_virtual_mic_target(Some("device-2".to_string()));
        switcher.set_virtual_mic_target(None);
        switcher.set_virtual_speaker_source(Some("device-3".to_string()));
        switcher.set_virtual_speaker_source(None);
        switcher.set_monitor_device(Some("device-4".to_string()));
        switcher.set_monitor_device(None);
    }

    #[test]
    fn engine_handles_default_to_a_no_op_device_switcher() {
        // `new_from_settings` is used both for the real Windows engine
        // (immediately overwritten with the real switcher afterward - see
        // `engine::start`) and for the non-Windows/engine-failure stubs
        // (never overwritten) - it must default to a real, inert
        // implementation, never a dangling/unset handle.
        let settings = crate::settings::Settings::default();
        let handles = EngineHandles::new_from_settings(&settings, true, None, Vec::new());
        // Must not panic - proves a concrete, callable `LiveDeviceSwitcher`
        // is always present from construction.
        handles.device_switcher.set_physical_mic(None);
    }
}
