//! Persisted GUI settings: the 6 stage toggles plus the two user-chosen
//! virtual-device ids (see `docs/VIRTUAL_DEVICES.md` for why there are two
//! separate device roles, each requiring its own selection).
//!
//! Everything in this file is pure, portable Rust (serde + one environment
//! variable read) - no OS audio calls, so it builds and its tests run
//! identically on Linux and Windows. Only the *path* it resolves
//! (`%LOCALAPPDATA%\ClearNAI\settings.json`) is Windows-shaped; on a
//! platform without `LOCALAPPDATA` set, persistence is simply skipped and
//! defaults are used - never a crash.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// "RNNoise (between Mic and Virtual Mic)" - default ON.
    pub mic_noise_on: bool,
    /// "BVC (after Noise)" - default ON *if BVC is available at startup*;
    /// forced off (and the GUI toggle disabled) otherwise. This stored value
    /// is the user's preference and is honored again next launch if BVC
    /// becomes available.
    pub mic_bvc_on: bool,
    /// "Studio" - default ON.
    pub mic_studio_on: bool,
    /// Mic-path Studio preset index (see
    /// `shared::studio_preset_to_index`/`from_index`) - default `1`
    /// (`Natural`), matching the previous hardcoded default.
    #[serde(default = "default_studio_preset")]
    pub mic_studio_preset: u8,
    /// "AEC" - default OFF per spec.
    pub aec_on: bool,
    /// "RNNoise (outbound, before Virtual Speaker forward)" - default ON.
    /// Replaces the old combined `speaker_cleanup_on` field: the outbound
    /// path is now 3 independently-toggleable stages, mirroring the mic
    /// side.
    #[serde(default = "default_true")]
    pub speaker_noise_on: bool,
    /// "BVC (outbound, after Noise)" - default ON (subject to the same
    /// `bvc_available` force-off as `mic_bvc_on`).
    #[serde(default = "default_true")]
    pub speaker_bvc_on: bool,
    /// "Studio (outbound)" - default ON.
    #[serde(default = "default_true")]
    pub speaker_studio_on: bool,
    /// Speaker-path Studio preset index - independent of `mic_studio_preset`.
    #[serde(default = "default_studio_preset")]
    pub speaker_studio_preset: u8,
    /// "Speaker Tap (inbound, read-only)" - default ON.
    pub speaker_tap_on: bool,
    /// WASAPI render-device id the engine renders the processed mic signal
    /// into (the "virtual mic target", e.g. one VB-Cable instance's "CABLE
    /// Input"). `None` until auto-detected or chosen by the user.
    pub virtual_mic_target_id: Option<String>,
    /// WASAPI capture-device id the engine captures "other apps' speaker
    /// output" from (the "virtual speaker source", e.g. a *second*,
    /// independent virtual cable instance's capture endpoint). `None` until
    /// chosen by the user - see `docs/VIRTUAL_DEVICES.md`: this one in
    /// particular cannot be auto-selected safely in the common case where
    /// only one virtual cable product is installed.
    pub virtual_speaker_source_id: Option<String>,
    /// WASAPI capture-device id to use for the real physical microphone
    /// input, instead of the OS default recording device (e.g. a Bluetooth
    /// headset mic vs. a built-in mic). `None` means "use the system default
    /// capture device", matching `MicCapture::start_default`'s existing
    /// behavior. Like the two virtual-device ids above, changing this only
    /// takes effect on next launch, not live.
    #[serde(default)]
    pub physical_mic_device_id: Option<String>,
    /// Whether the "Monitor (hear processed mic locally)" diagnostic path is
    /// enabled - default OFF. When on (and `monitor_device_id` is set), the
    /// fully-processed mic signal (after AEC/Noise/BVC/Studio) is also
    /// rendered to a second, independent real output device, entirely
    /// separate from the virtual-mic-target pipeline, so the user can A/B
    /// test the effect of the DSP stages without needing VB-Cable installed
    /// at all.
    #[serde(default)]
    pub monitor_enabled: bool,
    /// WASAPI render-device id the monitor path renders into (e.g. the
    /// user's headphones). `None` means the monitor path is not started at
    /// all, regardless of `monitor_enabled`.
    #[serde(default)]
    pub monitor_device_id: Option<String>,
    /// Set once the user has explicitly clicked "Continue"/"Skip" past the
    /// first-run setup screen while a compatibility check was failing.
    /// Suppresses re-showing the setup screen for *that* reason on
    /// subsequent launches. Deliberately does **not** suppress it for a
    /// missing BVC model file - that check is always redone fresh at every
    /// launch (cheap) and the screen is shown again if the model is (still,
    /// or again) missing, so this flag can never hide a real, currently-true
    /// problem - only a compatibility warning the user has already seen and
    /// chosen to proceed past once.
    #[serde(default)]
    pub setup_acknowledged: bool,
}

fn default_true() -> bool {
    true
}

fn default_studio_preset() -> u8 {
    1 // Natural
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_noise_on: true,
            mic_bvc_on: true,
            mic_studio_on: true,
            mic_studio_preset: default_studio_preset(),
            aec_on: false,
            speaker_noise_on: true,
            speaker_bvc_on: true,
            speaker_studio_on: true,
            speaker_studio_preset: default_studio_preset(),
            speaker_tap_on: true,
            virtual_mic_target_id: None,
            virtual_speaker_source_id: None,
            physical_mic_device_id: None,
            monitor_enabled: false,
            monitor_device_id: None,
            setup_acknowledged: false,
        }
    }
}

/// `%LOCALAPPDATA%\ClearNAI\settings.json`, read directly from the
/// `LOCALAPPDATA` environment variable. Deliberately not using the `dirs`
/// crate: one env var read is simpler than an extra dependency for
/// something this small, and avoids pinning yet another crate version.
pub fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("ClearNAI").join("settings.json"))
}

/// Loads settings from disk, falling back to `Settings::default()` if the
/// file doesn't exist, can't be read, or contains invalid JSON. Never
/// panics or propagates an error - a missing/corrupt settings file is not
/// fatal to starting the app.
pub fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

/// Persists settings to disk, creating `%LOCALAPPDATA%\ClearNAI\` if needed.
/// Returns an error (not a panic) if `LOCALAPPDATA` isn't set or the write
/// fails; callers treat persistence failure as non-fatal (log and continue).
pub fn save(settings: &Settings) -> anyhow::Result<()> {
    let Some(path) = settings_path() else {
        anyhow::bail!("LOCALAPPDATA is not set; cannot persist settings");
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let s = Settings::default();
        assert!(s.mic_noise_on, "RNNoise defaults ON");
        assert!(s.mic_bvc_on, "BVC preference defaults ON (actual availability is gated separately)");
        assert!(s.mic_studio_on, "Studio defaults ON");
        assert!(!s.aec_on, "AEC defaults OFF per spec");
        assert!(s.speaker_noise_on, "Speaker RNNoise defaults ON");
        assert!(s.speaker_bvc_on, "Speaker BVC defaults ON");
        assert!(s.speaker_studio_on, "Speaker Studio defaults ON");
        assert_eq!(s.mic_studio_preset, 1, "mic Studio preset defaults to Natural");
        assert_eq!(s.speaker_studio_preset, 1, "speaker Studio preset defaults to Natural");
        assert!(s.speaker_tap_on, "Speaker Tap defaults ON");
        assert!(s.virtual_mic_target_id.is_none());
        assert!(s.virtual_speaker_source_id.is_none());
        assert!(s.physical_mic_device_id.is_none(), "physical mic device defaults to auto/default");
        assert!(!s.monitor_enabled, "monitor path defaults OFF");
        assert!(s.monitor_device_id.is_none());
        assert!(!s.setup_acknowledged, "setup screen must not be pre-acknowledged by default");
    }

    #[test]
    fn settings_json_from_before_physical_mic_and_monitor_fields_existed_still_loads() {
        // Simulates a real settings.json written before this task added
        // `physical_mic_device_id`/`monitor_enabled`/`monitor_device_id` -
        // must still deserialize via `#[serde(default)]`.
        let json = r#"{
            "mic_noise_on": true,
            "mic_bvc_on": true,
            "mic_studio_on": true,
            "aec_on": false,
            "speaker_tap_on": true,
            "virtual_mic_target_id": null,
            "virtual_speaker_source_id": null
        }"#;
        let s: Settings = serde_json::from_str(json).expect("old-format settings.json must still parse");
        assert!(s.physical_mic_device_id.is_none());
        assert!(!s.monitor_enabled);
        assert!(s.monitor_device_id.is_none());
    }

    #[test]
    fn round_trips_physical_mic_and_monitor_fields_through_json() {
        let mut s = Settings::default();
        s.physical_mic_device_id = Some("{aaaa-bbbb}".into());
        s.monitor_enabled = true;
        s.monitor_device_id = Some("{cccc-dddd}".into());
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn settings_json_from_before_setup_acknowledged_existed_still_loads() {
        // Simulates a real settings.json written by an older build that
        // predates this field: it must still deserialize (via `#[serde(default)]`)
        // rather than losing every other saved preference just because one
        // new field is missing.
        let json = r#"{
            "mic_noise_on": true,
            "mic_bvc_on": false,
            "mic_studio_on": true,
            "aec_on": true,
            "speaker_cleanup_on": true,
            "speaker_tap_on": false,
            "virtual_mic_target_id": null,
            "virtual_speaker_source_id": null
        }"#;
        let s: Settings = serde_json::from_str(json).expect("old-format settings.json must still parse");
        assert!(!s.setup_acknowledged);
        assert!(!s.mic_bvc_on);
        assert!(s.aec_on);
        // Fields added after this format, including the replacement for the
        // old single `speaker_cleanup_on`, must fall back to their defaults.
        assert!(s.speaker_noise_on);
        assert!(s.speaker_bvc_on);
        assert!(s.speaker_studio_on);
        assert_eq!(s.mic_studio_preset, 1);
        assert_eq!(s.speaker_studio_preset, 1);
    }

    #[test]
    fn settings_json_with_old_combined_speaker_cleanup_field_still_loads() {
        // A real settings.json written before Task 3 split the combined
        // toggle into 3 - the now-unknown `speaker_cleanup_on` key must be
        // ignored (serde ignores unknown fields by default) rather than
        // failing deserialization, and the 3 new fields fall back to their
        // defaults.
        let json = r#"{
            "mic_noise_on": true,
            "mic_bvc_on": true,
            "mic_studio_on": true,
            "aec_on": false,
            "speaker_cleanup_on": false,
            "speaker_tap_on": true,
            "virtual_mic_target_id": null,
            "virtual_speaker_source_id": null
        }"#;
        let s: Settings = serde_json::from_str(json).expect("must still parse despite the removed field");
        assert!(s.speaker_noise_on);
        assert!(s.speaker_bvc_on);
        assert!(s.speaker_studio_on);
    }

    #[test]
    fn round_trips_through_json() {
        let mut s = Settings::default();
        s.aec_on = true;
        s.mic_bvc_on = false;
        s.virtual_mic_target_id = Some("{11111111-2222-3333-4444-555555555555}".into());
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Settings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn corrupt_json_falls_back_to_default_not_panic() {
        let back: Settings = serde_json::from_str("not valid json { [").unwrap_or_default();
        assert_eq!(back, Settings::default());
    }

    #[test]
    fn empty_json_object_fails_to_deserialize_and_falls_back() {
        // No fields are `Option`-defaulted by serde here (only `Option<String>`
        // fields tolerate missing keys as `None`; the bools are required), so
        // a bare `{}` must fail deserialization and callers must fall back
        // to `Settings::default()` rather than get a half-initialized value.
        let result: Result<Settings, _> = serde_json::from_str("{}");
        assert!(result.is_err());
    }
}
