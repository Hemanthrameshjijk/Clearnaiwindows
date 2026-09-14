//! Real Windows audio engine: constructs every DSP stage up front, wires
//! the three required pipelines (mic, speaker outbound, speaker inbound
//! loopback tap) over `dsp_core` ring buffers, and starts the real WASAPI
//! threads via `audio-io`.
//!
//! This entire module only exists on Windows (`#![cfg(windows)]`) because
//! it is the only place in this crate that touches `audio_io::capture`,
//! `audio_io::render`, and `audio_io::loopback`, all of which are
//! themselves Windows-only (`#![cfg(windows)]` in `audio-io` itself). On any
//! other platform, `main.rs` uses an inert `EngineHandles` with no threads
//! at all instead of calling into this module - see `main.rs`.
//!
//! # What has and hasn't been verified
//!
//! This module type-checks against the real public APIs of every crate it
//! calls into (`cargo check --target x86_64-pc-windows-gnu`). It has never
//! been run: no real WASAPI device, no real `weya_nc.dll`, no real VB-Cable
//! install. Every claim about behavior below is a claim about what the code
//! is *written* to do, not what has been *observed* to happen.

#![cfg(windows)]

use crate::settings::Settings;
use crate::shared::{DeviceList, DeviceOption, EngineHandles, LiveDeviceSwitcher, NoOpStage};
use aec::{AecEngine, NlmsAec};
use anyhow::{Context, Result};
use audio_io::capture::MicCapture;
use audio_io::devices;
use audio_io::loopback::LoopbackTap;
use audio_io::render::HardwareRender;
use dsp_core::{make_ring_buffer, run_if_enabled, Consumer, Producer, Stage, FRAME_SAMPLES};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// Frame deadline in nanoseconds (10ms @ 48kHz), used to classify realtime
/// status against measured processing-time percentiles.
const DEADLINE_NS: u64 = (FRAME_SAMPLES as u64 * 1_000_000_000) / dsp_core::SAMPLE_RATE_HZ as u64;

/// Owns every real OS-thread-backed handle the engine started, purely to
/// keep them alive for the life of the process (each one's `Drop` stops its
/// thread). Never inspected after construction - the process exiting when
/// the GUI window closes is what actually tears these down, matching the
/// "no tray/background daemon" requirement.
#[allow(dead_code)]
pub enum LiveHandle {
    Mic(MicCapture),
    Render(HardwareRender),
    Loopback(LoopbackTap),
}

/// One end of a ring buffer, shared between the pipeline thread that reads/
/// writes it every frame and `LiveAudioSwitcher`, which replaces the whole
/// `Option` when the user picks a different device for that role. `rtrb`'s
/// `Consumer`/`Producer` are consumed by move and cannot be "reused" after
/// their paired half is dropped (see module docs below) - a device switch
/// always builds a brand new ring buffer pair and swaps this pointer to the
/// new half, never mutates the old one in place.
///
/// Locking here is a deliberate, small departure from this project's
/// otherwise strict "no locks on the realtime audio path" rule: device
/// switching is a rare, user-initiated action (not per-frame data), and the
/// critical section is a single `Option::as_mut()` + `pop`/`push` per
/// sample - not spinning, not blocking on I/O, and never contended except
/// for the brief instant a switch is in flight. This mirrors how
/// `dsp_core::SharedPreset`/`StageToggle` already accept a small amount of
/// non-realtime-pure plumbing for rare live-control-surface actions.
type SwapConsumer = Arc<Mutex<Option<Consumer<f32>>>>;
type SwapProducer = Arc<Mutex<Option<Producer<f32>>>>;

fn swap_consumer(initial: Option<Consumer<f32>>) -> SwapConsumer {
    Arc::new(Mutex::new(initial))
}

fn swap_producer(initial: Option<Producer<f32>>) -> SwapProducer {
    Arc::new(Mutex::new(initial))
}

/// Owns every device-role live handle that a GUI device-picker change can
/// individually stop/restart, plus the swappable ring-buffer ends the
/// permanently-running mic/speaker pipeline threads read/write through.
///
/// # Why this replaces the old "just keep it alive in a Vec" approach
///
/// `LiveHandle`/`BootstrapOutcome::live` (still used for the *non*-
/// switchable roles: the loopback tap and the speaker path's fixed hardware
/// render) only ever needed to keep OS threads alive for the process
/// lifetime - nothing ever replaced one of its entries. A device picker
/// change needs the opposite: a named, individually-replaceable slot per
/// role. `Mutex<Option<T>>` per role (rather than, say, an enum-keyed map)
/// was chosen because there are exactly four fixed, statically-known roles
/// with different concrete types (`MicCapture` for two of them,
/// `HardwareRender` for the other two) - a map would need a trait object or
/// an enum anyway, with no benefit over four plain named fields.
///
/// # The ring-buffer wrinkle
///
/// The mic and speaker pipeline threads (`spawn_mic_pipeline`/
/// `spawn_speaker_pipeline`) now run for the entire process lifetime
/// regardless of whether any device is configured for their role yet -
/// this is what makes "select a virtual mic target for the first time
/// without ever having configured one" and "switch it live later" the same
/// code path. They read/write through the `SwapConsumer`/`SwapProducer`
/// handles below instead of owning a `Consumer`/`Producer` directly, so a
/// device switch can point them at a freshly-created ring buffer half
/// without the pipeline thread itself needing to know a switch happened.
pub struct LiveAudioSwitcher {
    physical_mic: Mutex<Option<MicCapture>>,
    virtual_mic_render: Mutex<Option<HardwareRender>>,
    speaker_source_capture: Mutex<Option<MicCapture>>,
    monitor_render: Mutex<Option<HardwareRender>>,

    mic_in_swap: SwapConsumer,
    mic_out_swap: SwapProducer,
    monitor_out_swap: SwapProducer,
    spk_in_swap: SwapConsumer,
}

impl LiveAudioSwitcher {
    fn stop_and_replace_render(slot: &Mutex<Option<HardwareRender>>, new: Option<HardwareRender>) {
        let old = std::mem::replace(&mut *slot.lock().unwrap(), new);
        if let Some(old) = old {
            old.stop();
        }
    }

    fn stop_and_replace_mic(slot: &Mutex<Option<MicCapture>>, new: Option<MicCapture>) {
        let old = std::mem::replace(&mut *slot.lock().unwrap(), new);
        if let Some(old) = old {
            old.stop();
        }
    }
}

impl LiveDeviceSwitcher for LiveAudioSwitcher {
    /// Restarts physical mic capture on `device_id` (or the system default
    /// if `None`), reconnecting the mic pipeline thread's input to a fresh
    /// ring buffer. The new capture is started (and its producer wired into
    /// `mic_in_swap`) *before* the old one is stopped, so the pipeline
    /// thread never sees a spurious "gone" gap wider than the time it takes
    /// WASAPI to open the new device.
    fn set_physical_mic(&self, device_id: Option<String>) {
        let (producer, consumer) = make_ring_buffer(2.0);
        let started = match device_id.as_deref() {
            Some(id) => MicCapture::start_on_device_id(id, producer),
            None => MicCapture::start_default(producer),
        };
        match started {
            Ok(capture) => {
                *self.mic_in_swap.lock().unwrap() = Some(consumer);
                Self::stop_and_replace_mic(&self.physical_mic, Some(capture));
            }
            Err(e) => {
                crate::log_error!(
                    "[clearnairt] failed to switch physical microphone live to {device_id:?} \
                     ({e:#}); the previous physical microphone (if any) keeps running."
                );
            }
        }
    }

    /// Restarts the virtual-mic-target render device, reconnecting the mic
    /// pipeline thread's `mic_out` output to a fresh ring buffer. `None`
    /// tears the render device down entirely (matching "no virtual mic
    /// target selected" at boot) - the mic pipeline thread keeps running
    /// (e.g. for the monitor path) but has nowhere configured to forward to.
    fn set_virtual_mic_target(&self, device_id: Option<String>) {
        match device_id {
            Some(id) => {
                let (producer, consumer) = make_ring_buffer(2.0);
                match HardwareRender::start_on_device_id(&id, consumer) {
                    Ok(render) => {
                        *self.mic_out_swap.lock().unwrap() = Some(producer);
                        Self::stop_and_replace_render(&self.virtual_mic_render, Some(render));
                    }
                    Err(e) => {
                        crate::log_error!(
                            "[clearnairt] failed to switch virtual mic target live to '{id}' \
                             ({e:#}); the previous virtual mic target (if any) keeps running."
                        );
                    }
                }
            }
            None => {
                *self.mic_out_swap.lock().unwrap() = None;
                Self::stop_and_replace_render(&self.virtual_mic_render, None);
            }
        }
    }

    /// Restarts the virtual-speaker-source capture device, reconnecting the
    /// speaker pipeline thread's input to a fresh ring buffer. `None` tears
    /// the capture device down; the speaker pipeline thread keeps running
    /// but has no input, matching "no virtual speaker source selected".
    fn set_virtual_speaker_source(&self, device_id: Option<String>) {
        match device_id {
            Some(id) => {
                let (producer, consumer) = make_ring_buffer(2.0);
                match MicCapture::start_on_device_id(&id, producer) {
                    Ok(capture) => {
                        *self.spk_in_swap.lock().unwrap() = Some(consumer);
                        Self::stop_and_replace_mic(&self.speaker_source_capture, Some(capture));
                    }
                    Err(e) => {
                        crate::log_error!(
                            "[clearnairt] failed to switch virtual speaker source live to '{id}' \
                             ({e:#}); the previous virtual speaker source (if any) keeps running."
                        );
                    }
                }
            }
            None => {
                *self.spk_in_swap.lock().unwrap() = None;
                Self::stop_and_replace_mic(&self.speaker_source_capture, None);
            }
        }
    }

    /// Restarts the monitor render device, reconnecting the mic pipeline
    /// thread's monitor output to a fresh ring buffer. `None` tears the
    /// monitor render device down entirely.
    fn set_monitor_device(&self, device_id: Option<String>) {
        match device_id {
            Some(id) => {
                let (producer, consumer) = make_ring_buffer(2.0);
                match HardwareRender::start_on_device_id(&id, consumer) {
                    Ok(render) => {
                        *self.monitor_out_swap.lock().unwrap() = Some(producer);
                        Self::stop_and_replace_render(&self.monitor_render, Some(render));
                    }
                    Err(e) => {
                        crate::log_error!(
                            "[clearnairt] failed to switch monitor output live to '{id}' ({e:#}); \
                             the previous monitor output (if any) keeps running."
                        );
                    }
                }
            }
            None => {
                *self.monitor_out_swap.lock().unwrap() = None;
                Self::stop_and_replace_render(&self.monitor_render, None);
            }
        }
    }
}

/// Result of starting the engine: the shared toggle/metrics handles for the
/// GUI, the enumerated device lists for the two device pickers, any
/// non-fatal startup warnings, and every live OS thread handle (kept only to
/// extend their lifetime to the whole process).
pub struct BootstrapOutcome {
    pub handles: EngineHandles,
    pub devices: DeviceList,
    pub live: Vec<LiveHandle>,
}

/// Enumerates render/capture devices for the GUI's device pickers. Returns
/// empty lists (never an error to the caller) if enumeration itself fails -
/// the GUI must still come up, just with no devices to pick from.
pub fn enumerate_devices() -> DeviceList {
    let render = devices::list_render_devices()
        .unwrap_or_else(|e| {
            crate::log_error!("[clearnairt] WARNING: failed to enumerate render devices: {e:#}");
            Vec::new()
        })
        .into_iter()
        .map(|(id, label)| DeviceOption { id, label })
        .collect();
    let capture = devices::list_capture_devices()
        .unwrap_or_else(|e| {
            crate::log_error!("[clearnairt] WARNING: failed to enumerate capture devices: {e:#}");
            Vec::new()
        })
        .into_iter()
        .map(|(id, label)| DeviceOption { id, label })
        .collect();
    DeviceList { render, capture }
}

/// Auto-selects the "virtual mic target" render device only if exactly one
/// render endpoint looks like a VB-Cable-style "CABLE Input" - see
/// `docs/VIRTUAL_DEVICES.md`. Ambiguous (zero or more than one match) means
/// no safe default exists and the user must choose explicitly.
pub fn default_virtual_mic_target(list: &DeviceList) -> Option<String> {
    let mut matches = list
        .render
        .iter()
        .filter(|d| devices::name_contains_cable_input(&d.label));
    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first.id.clone())
    }
}

/// Auto-selects the "virtual speaker source" capture device only if exactly
/// one capture endpoint looks like a VB-Cable-style "CABLE Output". In the
/// common case of a single VB-Cable install, this is also the device the
/// mic-target render already paired with, which is exactly the collision
/// `docs/VIRTUAL_DEVICES.md` documents - callers should treat an
/// auto-selected speaker-source id that equals the auto-selected mic-target
/// id's paired endpoint as a signal to warn the user to install a *second*
/// virtual cable instance.
pub fn default_virtual_speaker_source(list: &DeviceList) -> Option<String> {
    let mut matches = list
        .capture
        .iter()
        .filter(|d| devices::name_contains_cable_output(&d.label));
    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first.id.clone())
    }
}

/// Starts the full engine: loads BVC (or falls back to a real no-op stage,
/// honestly reflected in `EngineHandles::mic_bvc_available`/
/// `speaker_bvc_available`), constructs every
/// DSP stage up front (regardless of initial toggle state), wires the three
/// required pipelines, and starts their WASAPI threads.
///
/// `virtual_mic_target_id` / `virtual_speaker_source_id` are the
/// user-selected (or auto-detected) device ids for the two virtual-cable
/// roles described in `docs/VIRTUAL_DEVICES.md`. Either may be `None`, in
/// which case the corresponding half of the pipeline is not started and a
/// warning is added to `BootstrapOutcome`'s handles - this is not an error,
/// since a first-time user has not necessarily installed/configured virtual
/// cables yet.
pub fn start(
    bvc_assets_dir: &Path,
    settings: &Settings,
    devices_list: DeviceList,
    virtual_mic_target_id: Option<&str>,
    virtual_speaker_source_id: Option<&str>,
) -> Result<BootstrapOutcome> {
    let mut warnings = Vec::new();

    // --- BVC: one real load attempt per independent pipeline instance. ---
    // `bvc_assets_dir` is `setup::app_data_dir()` (`%LOCALAPPDATA%\ClearNAI`),
    // where `main.rs::bootstrap` has already self-extracted the embedded
    // `weya_nc.dll` + model bundle via `setup::ensure_bvc_assets_extracted`
    // before calling here - no more "next to the exe" file placement.
    // Mic-path BVC gates the GUI's single "BVC" toggle and its availability
    // flag; the speaker-path instance is wholly independent (its own DLL
    // session), matching the project's "always construct every stage, even
    // ones behind a toggle that starts off" rule, and its own honest
    // fallback if loading fails a second time (e.g. DLL present but a
    // transient session-creation failure) is a plain no-op, not a crash.
    let mic_bvc_load = bvc_hush::HushBvcStage::try_load(bvc_assets_dir);
    let (mic_bvc_available, mic_bvc_unavailable_reason) = match &mic_bvc_load {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    // BVC calls into a closed-source DLL (`weya_nc.dll`) this crate does not
    // control. Real hardware finding: the mic pipeline thread can stop
    // draining its ring buffer entirely for a minute-plus with no error
    // from the WASAPI capture thread at all - i.e. something in the
    // pipeline blocked forever rather than erroring, and BVC's FFI call is
    // the only link in the chain that isn't our own pure-Rust code. Wrap it
    // in a `WatchdogStage` so a wedged call gets a bounded timeout (well
    // above the 10ms realtime deadline, since the DLL is allowed to be slow,
    // just not infinite) instead of freezing the whole pipeline thread
    // silently and permanently.
    const BVC_WATCHDOG_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
    let mic_bvc_stage: Box<dyn Stage> = match mic_bvc_load {
        Ok(stage) => Box::new(dsp_core::WatchdogStage::new(
            "mic BVC",
            Box::new(stage),
            BVC_WATCHDOG_TIMEOUT,
            |msg| crate::log_error!("[clearnairt] {msg}"),
        )),
        Err(_) => Box::new(NoOpStage("BVC (unavailable)")),
    };
    let speaker_bvc_load = bvc_hush::HushBvcStage::try_load(bvc_assets_dir);
    let (speaker_bvc_available, speaker_bvc_unavailable_reason) = match &speaker_bvc_load {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    let speaker_bvc_stage: Box<dyn Stage> = match speaker_bvc_load {
        Ok(stage) => Box::new(dsp_core::WatchdogStage::new(
            "speaker BVC",
            Box::new(stage),
            BVC_WATCHDOG_TIMEOUT,
            |msg| crate::log_error!("[clearnairt] {msg}"),
        )),
        Err(e) => {
            warnings.push(format!(
                "Speaker-path BVC unavailable ({e}); the outbound BVC toggle will have no effect."
            ));
            Box::new(NoOpStage("BVC (unavailable)"))
        }
    };

    // --- Noise: RNNoise for both paths, per spec default. DeepFilter is a
    // real, supported `NoiseEngineKind` in `noise-engine`, but is not wired
    // to anything reachable from this GUI - see crate-level docs below for
    // why (no bundled model file). Not calling `build(DeepFilter, ...)`
    // anywhere is intentional, not an oversight. ---
    let mic_noise_stage: Box<dyn Stage> =
        noise_engine::build(noise_engine::NoiseEngineKind::RnNoise, None)
            .context("constructing mic-path RNNoise stage")?;
    let speaker_noise_stage: Box<dyn Stage> =
        noise_engine::build(noise_engine::NoiseEngineKind::RnNoise, None)
            .context("constructing speaker-path RNNoise stage")?;

    // --- Studio: constructed from the persisted preset preference (default
    // `Natural`, the gentlest of the three active presets - see
    // `settings::default_studio_preset`). Both mic and speaker paths now get
    // their own independent Studio stage; live preset changes are handled by
    // each pipeline thread swapping in a freshly-constructed `StudioStage`
    // when its `SharedPreset` changes (see the mic/speaker pipeline loops
    // below) - `StudioStage` itself has no live "change preset" method. ---
    let mic_studio_stage: Box<dyn Stage> = Box::new(studio_dsp::StudioStage::new(
        crate::shared::studio_preset_from_index(settings.mic_studio_preset),
    ));

    // --- AEC: always constructed as a real, warmed `NlmsAec` regardless of
    // the toggle's initial state (per the "every stage is always
    // constructed" rule) - `aec::build`'s `enabled` flag instead picks
    // between `NlmsAec`/`DisabledAec` permanently, which would defeat that
    // rule for the OFF-by-default case, so it is deliberately not used
    // here. The `StageToggle` alone decides whether `.process()` is called
    // each frame (see the mic pipeline loop below), mirroring
    // `dsp_core::run_if_enabled`'s pattern for the bespoke, non-`Stage`
    // `AecEngine` trait. ---
    let aec_engine: Box<dyn AecEngine> = Box::new(NlmsAec::new_default());

    let mut handles = EngineHandles::new_from_settings(
        settings,
        mic_bvc_available,
        mic_bvc_unavailable_reason,
        speaker_bvc_available,
        speaker_bvc_unavailable_reason,
        Vec::new(),
    );

    // --- Ring buffers. 2 seconds of headroom at 48kHz is generous relative
    // to the 10ms frame size; sized once here, never resized (a live device
    // switch later builds a brand new pair - see `LiveAudioSwitcher`). ---
    let (mic_in_p, mic_in_c) = make_ring_buffer(2.0);
    let (loopback_p, loopback_c) = make_ring_buffer(2.0);
    let (spk_out_p, spk_out_c) = make_ring_buffer(2.0);

    let mut live: Vec<LiveHandle> = Vec::new();

    // --- Pipeline 1 (mic): real physical mic -> ring buffer. Uses the
    // user-selected physical capture device (`settings.physical_mic_device_id`,
    // e.g. a Bluetooth headset mic instead of a built-in mic) if one was
    // chosen, mirroring exactly how `virtual_mic_target_id`/
    // `default_virtual_mic_target` pick between "auto/default" (`None`) and
    // "a specific device" (`Some(id)`) elsewhere in this function. Picking a
    // *different* device later in the GUI now takes effect live, via
    // `LiveAudioSwitcher::set_physical_mic` - this initial start is only
    // "next launch" in the sense that the persisted *starting* selection is
    // read once, here. ---
    let physical_mic_id = settings.physical_mic_device_id.clone();
    let mic_capture = match physical_mic_id.as_deref() {
        Some(id) => MicCapture::start_on_device_id(id, mic_in_p)
            .context("starting mic capture on the selected physical microphone device")?,
        None => MicCapture::start_default(mic_in_p).context("starting default mic capture")?,
    };

    // --- Monitor (diagnostic/testing-only): an optional second, independent
    // render path that receives a *copy* of the mic pipeline's final
    // processed frame, wholly separate from the virtual-mic-target pipeline.
    // Only started at all if the user has selected a monitor output device;
    // see `spawn_mic_pipeline` for where the frame is actually copied to
    // both downstream producers (the SPSC ring buffers used throughout this
    // crate cannot fan a single one out to two consumers, hence a wholly
    // separate ring buffer pair here). ---
    let (monitor_render, monitor_out_p) = match settings.monitor_device_id.as_deref() {
        Some(id) => {
            let (p, c) = make_ring_buffer(2.0);
            match HardwareRender::start_on_device_id(id, c) {
                Ok(render) => (Some(render), Some(p)),
                Err(e) => {
                    // Exactly as graceful as the "no virtual mic target
                    // selected" case below: a monitor device that fails to
                    // open must never take down the rest of the pipeline.
                    warnings.push(format!(
                        "Monitor output device could not be started ({e:#}); local monitoring \
                         is unavailable, but the rest of the pipeline is unaffected."
                    ));
                    (None, None)
                }
            }
        }
        None => (None, None),
    };

    // --- Pipeline 3 (speaker inbound / loopback tap): real hardware OUTPUT
    // device (default render device - what's actually reaching the user's
    // ears), read-only, always running once started; see
    // `EngineHandles::speaker_tap` docs for why the *toggle* doesn't stop
    // this thread. Not one of the four live-switchable roles, so it stays a
    // plain `LiveHandle` kept alive for the process lifetime. ---
    let loopback_tap = LoopbackTap::start_default(loopback_p).context("starting hardware-output loopback tap")?;
    live.push(LiveHandle::Loopback(loopback_tap));

    // --- Pipeline 1 continued: engine -> user-selected virtual mic target,
    // and/or -> the monitor path started above. These two downstream
    // producers are independent of each other by construction (separate
    // ring buffer pairs). Unlike before, the mic pipeline thread is now
    // *always* spawned (see `spawn_mic_pipeline` below) regardless of
    // whether either downstream role is configured yet, so that selecting
    // one for the first time via the GUI (a live switch, not just a
    // next-launch preference) has a running thread to reconnect to. ---
    let (virtual_mic_render, mic_out_p) = match virtual_mic_target_id {
        Some(id) => {
            let (p, c) = make_ring_buffer(2.0);
            match HardwareRender::start_on_device_id(id, c) {
                Ok(render) => (Some(render), Some(p)),
                Err(e) => {
                    warnings.push(format!(
                        "Failed to start render into the selected virtual-mic-target device ({e:#}); \
                         the processed mic pipeline will keep running for the monitor path (if \
                         configured), but nothing is being forwarded to a virtual mic."
                    ));
                    (None, None)
                }
            }
        },
        None => {
            warnings.push(
                "No virtual mic target device selected - the processed mic signal is not being \
                 forwarded to a virtual microphone. Install a virtual audio cable and select its \
                 render endpoint in the GUI (see docs/VIRTUAL_DEVICES.md), or use the Monitor \
                 toggle to hear the processed signal directly without one."
                    .to_string(),
            );
            (None, None)
        }
    };

    let mic_in_swap = swap_consumer(Some(mic_in_c));
    let mic_out_swap = swap_producer(mic_out_p);
    let monitor_out_swap = swap_producer(monitor_out_p);

    spawn_mic_pipeline(
        mic_in_swap.clone(),
        mic_out_swap.clone(),
        monitor_out_swap.clone(),
        loopback_c,
        handles.clone(),
        aec_engine,
        mic_noise_stage,
        mic_bvc_stage,
        mic_studio_stage,
    );

    // --- Pipeline 2 (speaker outbound): capture from user-selected virtual
    // speaker source -> Noise -> BVC -> Studio (3 independent toggles,
    // mirroring the mic pipeline) -> real hardware speakers. The hardware
    // render half is fixed (always the default output device, not one of
    // the four picker roles), so it starts unconditionally; only the
    // capture half depends on `virtual_speaker_source_id`. Like the mic
    // pipeline, this thread now always runs so a first-time device
    // selection is a live switch rather than requiring a restart. ---
    let render = HardwareRender::start_default(spk_out_c).context("starting hardware speaker render")?;
    live.push(LiveHandle::Render(render));

    let (speaker_source_capture, spk_in_p) = match virtual_speaker_source_id {
        Some(id) => {
            let (p, c) = make_ring_buffer(2.0);
            match MicCapture::start_on_device_id(id, p) {
                Ok(capture) => (Some(capture), Some(c)),
                Err(e) => {
                    warnings.push(format!(
                        "Failed to start capture from the selected virtual-speaker-source device \
                         ({e:#}); the speaker outbound pipeline will keep running once a working \
                         device is selected."
                    ));
                    (None, None)
                }
            }
        }
        None => {
            warnings.push(
                "No virtual speaker source device selected - the speaker outbound cleanup \
                 pipeline is not running. See docs/VIRTUAL_DEVICES.md: this must be a *second*, \
                 independent virtual cable instance, distinct from the virtual mic target."
                    .to_string(),
            );
            (None, None)
        }
    };

    let spk_in_swap = swap_consumer(spk_in_p);

    let speaker_studio_stage: Box<dyn Stage> = Box::new(studio_dsp::StudioStage::new(
        crate::shared::studio_preset_from_index(handles.speaker_studio_preset.load()),
    ));
    spawn_speaker_pipeline(
        spk_in_swap.clone(),
        spk_out_p,
        handles.clone(),
        speaker_noise_stage,
        speaker_bvc_stage,
        speaker_studio_stage,
    );

    handles.device_switcher = Arc::new(LiveAudioSwitcher {
        physical_mic: Mutex::new(Some(mic_capture)),
        virtual_mic_render: Mutex::new(virtual_mic_render),
        speaker_source_capture: Mutex::new(speaker_source_capture),
        monitor_render: Mutex::new(monitor_render),
        mic_in_swap,
        mic_out_swap,
        monitor_out_swap,
        spk_in_swap,
    });

    handles.warnings = std::sync::Arc::new(warnings);

    Ok(BootstrapOutcome {
        handles,
        devices: devices_list,
        live,
    })
}

/// Blocks (short-sleeping between polls - this is a plain consumer thread,
/// not the realtime WASAPI callback itself, so sleeping here is fine) until
/// `out` is fully filled from `consumer`.
///
/// Unlike the pre-live-switching version of this function, this never gives
/// up and returns `false`: `consumer` is a `SwapConsumer`, and its current
/// `None` state (no device configured for this role yet, or momentarily
/// mid-switch) is an ordinary, expected, *temporary* condition now - not a
/// sign the producer is gone forever - so the pipeline thread must keep
/// waiting rather than exit. A device permanently going away without a
/// replacement (e.g. unplugged) looks the same from here: silent underrun
/// forever, not a crash, matching this project's "never emit garbage,
/// silence on underrun" rule elsewhere.
fn pull_frame_blocking(consumer: &SwapConsumer, out: &mut [f32]) {
    let mut n = 0;
    while n < out.len() {
        let popped = {
            let mut guard = consumer.lock().unwrap();
            guard.as_mut().and_then(|c| c.pop().ok())
        };
        match popped {
            Some(sample) => {
                out[n] = sample;
                n += 1;
            }
            None => thread::sleep(std::time::Duration::from_micros(200)),
        }
    }
}

/// Pulls up to `out.len()` samples without blocking at all, leaving unfilled
/// tail samples at whatever `out` already held (callers pre-zero it for
/// "silence on underrun") - used for the loopback tap, which is not one of
/// the four live-switchable device-picker roles.
/// - used for the loopback tap and speaker-render inputs, which are not one
/// of the four live-switchable device-picker roles.
fn pull_frame_from_fixed(consumer: &mut Consumer<f32>, out: &mut [f32]) -> usize {
    let mut n = 0;
    for slot in out.iter_mut() {
        match consumer.pop() {
            Ok(sample) => {
                *slot = sample;
                n += 1;
            }
            Err(_) => break,
        }
    }
    n
}

/// Pushes `frame` into `producer`. A `None` swap (no device configured for
/// this role) silently discards the frame - there is nowhere for it to go,
/// which is the expected steady state before a device is ever selected for
/// this role, not an error.
fn push_frame(producer: &SwapProducer, frame: &[f32]) {
    let mut guard = producer.lock().unwrap();
    let Some(p) = guard.as_mut() else { return };
    push_frame_to_fixed(p, frame);
}

/// Same as `push_frame` but for a plain (non-swappable) `Producer<f32>` -
/// used for the speaker path's fixed hardware render output, which is not
/// one of the four live-switchable device-picker roles.
fn push_frame_to_fixed(producer: &mut Producer<f32>, frame: &[f32]) {
    for &sample in frame {
        if producer.push(sample).is_err() {
            // Downstream (render thread) isn't keeping up; drop the rest of
            // this frame rather than block the processing thread.
            crate::log_error!("[clearnairt] output ring buffer full, dropping frame");
            return;
        }
    }
}

/// Pipeline 1: mic capture -> AEC -> Noise -> BVC -> Studio -> virtual mic
/// render. Runs on its own thread for the life of the process.
#[allow(clippy::too_many_arguments)]
fn spawn_mic_pipeline(
    mic_in: SwapConsumer,
    mic_out: SwapProducer,
    monitor_out: SwapProducer,
    mut loopback_in: Consumer<f32>,
    handles: EngineHandles,
    mut aec_engine: Box<dyn AecEngine>,
    mut noise_stage: Box<dyn Stage>,
    mut bvc_stage: Box<dyn Stage>,
    mut studio_stage: Box<dyn Stage>,
) {
    thread::Builder::new()
        .name("clearnai-mic-pipeline".into())
        .spawn(move || {
            let mut frame = vec![0.0f32; FRAME_SAMPLES];
            let mut reference = vec![0.0f32; FRAME_SAMPLES];
            let silence = vec![0.0f32; FRAME_SAMPLES];
            let mut last_studio_preset = handles.mic_studio_preset.load();
            loop {
                pull_frame_blocking(&mic_in, &mut frame);
                let started = Instant::now();

                // Deliberate, narrow exception to "no allocation in the
                // realtime hot path": a preset change is a rare, deliberate
                // user action (not per-frame data), so once per frame we
                // cheaply load the shared atomic and only on an actual
                // change do we allocate a fresh `StudioStage` and swap it
                // into this thread-local variable. The common case (no
                // change) is a single relaxed atomic load, same cost as any
                // other toggle check in this loop.
                let current_preset = handles.mic_studio_preset.load();
                if current_preset != last_studio_preset {
                    studio_stage = Box::new(studio_dsp::StudioStage::new(
                        crate::shared::studio_preset_from_index(current_preset),
                    ));
                    last_studio_preset = current_preset;
                }

                // Reference frame for AEC, from the read-only hardware
                // output loopback tap. See `EngineHandles::speaker_tap`
                // docs: when that toggle is off, real tapped audio is
                // discarded in favor of silence here rather than the tap
                // thread itself being stopped/restarted.
                for s in reference.iter_mut() {
                    *s = 0.0;
                }
                if handles.speaker_tap.is_on() {
                    pull_frame_from_fixed(&mut loopback_in, &mut reference);
                } else {
                    // Still drain the tap's ring buffer so it doesn't fill
                    // up and start dropping frames at the OS thread level
                    // while merely "not forwarded" - drained samples are
                    // discarded, never used.
                    let mut discard = [0.0f32; FRAME_SAMPLES];
                    pull_frame_from_fixed(&mut loopback_in, &mut discard[..reference.len().min(FRAME_SAMPLES)]);
                }

                if handles.aec.is_on() {
                    aec_engine.process(&mut frame, &reference);
                }
                run_if_enabled(noise_stage.as_mut(), &handles.mic_noise, &mut frame);
                run_if_enabled(bvc_stage.as_mut(), &handles.mic_bvc, &mut frame);
                run_if_enabled(studio_stage.as_mut(), &handles.mic_studio, &mut frame);

                let peak = frame.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                handles.level.store(peak);

                let elapsed_ns = started.elapsed().as_nanos() as u64;
                handles.latency_log.record_ns(elapsed_ns);
                handles.counters.frames_processed.fetch_add(1, Ordering::Relaxed);
                if elapsed_ns > DEADLINE_NS {
                    handles.counters.deadline_misses.fetch_add(1, Ordering::Relaxed);
                }

                // Hand a *copy* of the one final processed frame to each
                // independent downstream path that's actually active. This
                // is the fan-out `dsp_core`'s SPSC ring buffers can't do on
                // their own (one producer, one consumer per ring buffer,
                // per `RingBuffer::new`'s docs) - each path got its own
                // ring buffer pair back in `engine::start`, and here we just
                // push the same samples into whichever of the (up to two)
                // producers exist.
                push_frame(&mic_out, &frame);
                // When the Monitor toggle is off, push real silence instead
                // of skipping the push entirely, so the monitor render
                // thread doesn't starve/underrun-log just because the user
                // wants it muted rather than torn down. If no monitor device
                // is configured at all, `push_frame` on a `None` swap is
                // already a no-op either way.
                if handles.monitor.is_on() {
                    push_frame(&monitor_out, &frame);
                } else {
                    push_frame(&monitor_out, &silence);
                }
            }
        })
        .expect("spawning mic pipeline thread");
}

/// Pipeline 2: virtual-speaker-source capture -> Noise -> BVC -> Studio
/// (each independently toggleable, mirroring the mic pipeline exactly) ->
/// hardware speaker render.
fn spawn_speaker_pipeline(
    spk_in: SwapConsumer,
    mut spk_out: Producer<f32>,
    handles: EngineHandles,
    mut noise_stage: Box<dyn Stage>,
    mut bvc_stage: Box<dyn Stage>,
    mut studio_stage: Box<dyn Stage>,
) {
    thread::Builder::new()
        .name("clearnai-speaker-pipeline".into())
        .spawn(move || {
            let mut frame = vec![0.0f32; FRAME_SAMPLES];
            let mut last_studio_preset = handles.speaker_studio_preset.load();
            loop {
                pull_frame_blocking(&spk_in, &mut frame);

                // Same deliberate, narrow exception as the mic pipeline's
                // preset swap (see its comment): a preset change is rare and
                // deliberate, so only on an actual change do we allocate a
                // fresh `StudioStage` and swap it in.
                let current_preset = handles.speaker_studio_preset.load();
                if current_preset != last_studio_preset {
                    studio_stage = Box::new(studio_dsp::StudioStage::new(
                        crate::shared::studio_preset_from_index(current_preset),
                    ));
                    last_studio_preset = current_preset;
                }

                run_if_enabled(noise_stage.as_mut(), &handles.speaker_noise, &mut frame);
                run_if_enabled(bvc_stage.as_mut(), &handles.speaker_bvc, &mut frame);
                run_if_enabled(studio_stage.as_mut(), &handles.speaker_studio, &mut frame);
                push_frame_to_fixed(&mut spk_out, &frame);
            }
        })
        .expect("spawning speaker pipeline thread");
}
