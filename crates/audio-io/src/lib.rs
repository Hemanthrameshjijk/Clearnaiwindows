//! Windows OS-audio boundary for the voice engine: WASAPI mic capture,
//! hardware render, loopback tap, and device enumeration (incl. VB-Audio
//! Virtual Cable endpoint discovery).
//!
//! Everything that actually calls into WASAPI is gated `#[cfg(windows)]`
//! (transitively, via the `wasapi` dependency only being pulled in under
//! `cfg(windows)` in Cargo.toml). The device-name substring matching in
//! `devices` is OS-independent and always compiled/tested.
//!
//! None of this crate's WASAPI-facing code has been exercised against real
//! audio hardware from this development environment (Linux, no Windows
//! machine available) - it has only been type-checked against the `wasapi`
//! crate's public API via `cargo check --target x86_64-pc-windows-gnu`.

pub mod capture;
pub mod devices;
pub mod loopback;
pub mod render;

/// Initializes COM (STA, not MTA) on the *calling* thread. Every
/// `capture.rs`/`render.rs`/`loopback.rs` worker thread already calls
/// `wasapi::initialize_mta()` internally before touching WASAPI — that part
/// is fine, those are dedicated audio threads with no GUI/OLE involvement.
/// But each of those modules' `start_default()`/`start_by_name()` entry
/// points *also* run a "does a matching device exist" pre-check with
/// `DeviceEnumerator` on the **caller's** thread before ever spawning the
/// worker thread, and `devices::list_render_devices`/`list_capture_devices`/
/// `find_vb_cable_endpoints` (used directly by the GUI's device pickers) run
/// entirely on the caller's thread with no worker thread at all. In this
/// app, "the caller's thread" for all of that is the **main/GUI thread**.
///
/// COM apartment state is per-thread: initializing it inside a spawned
/// worker thread does nothing for the thread that spawned it. Real hardware
/// testing surfaced two conflicting gaps here, in order:
///
/// 1. First, calling nothing at all on the main thread: `CoInitialize has
///    not been called (0x800401F0)` on both the device-enumeration-for-
///    GUI-pickers path and the "starting default mic capture" pre-check
///    path.
/// 2. After fixing (1) by calling `wasapi::initialize_mta()` here: a new,
///    different real-hardware crash, `OleInitialize failed! Result was:
///    'RPC_E_CHANGED_MODE'`, from deep inside `winit` (which `iced` uses for
///    window creation) — `OleInitialize` (needed for the window's
///    drag-and-drop support) hard-requires an **STA** apartment, and once a
///    thread has committed to MTA, a later `OleInitialize` on that same
///    thread fails outright rather than being silently compatible. Since
///    this app's main thread both runs the GUI event loop *and* does the
///    WASAPI pre-checks above, it must pick the apartment model the GUI
///    needs (STA) — WASAPI's core interfaces (`IMMDeviceEnumerator`,
///    `IMMDevice`, `IAudioClient`) are documented as apartment-agnostic, so
///    STA works fine for the device-enumeration/pre-check calls made from
///    here too. Worker threads calling `initialize_mta()` on their own,
///    separate threads remain unaffected either way.
///
/// Safe to call more than once on the same thread with the same apartment
/// type (COM reference-counts nested compatible `CoInitializeEx` calls,
/// including the one `winit`/`OleInitialize` performs internally afterward).
/// Logs a warning rather than swallowing a genuine failure - deliberately
/// not propagated as an `Err` since a caller failing to start over this
/// would be worse than proceeding and letting the real WASAPI call
/// downstream fail with its own, more specific error.
#[cfg(windows)]
pub fn initialize_com_for_this_thread() {
    let hr = wasapi::initialize_sta();
    if hr.is_err() {
        eprintln!("[audio-io] WARNING: CoInitializeEx(STA) on this thread returned {hr:?}");
    }
}

/// The single WASAPI wire format used for every stream this crate opens:
/// 32-bit float, mono, at `dsp_core::SAMPLE_RATE_HZ`. We rely on WASAPI
/// shared-mode `autoconvert` (see `wasapi::StreamMode::EventsShared`) to
/// handle any resampling/channel-mixing between this and whatever a given
/// device's actual mix format is, so the rest of the pipeline (built around
/// `dsp_core::FRAME_SAMPLES` @ `SAMPLE_RATE_HZ`) never has to know about it.
#[cfg(windows)]
pub(crate) fn engine_wave_format() -> wasapi::WaveFormat {
    wasapi::WaveFormat::new(
        32,
        32,
        &wasapi::SampleType::Float,
        dsp_core::SAMPLE_RATE_HZ as usize,
        1,
        None,
    )
}

/// Requested WASAPI shared-mode buffer duration, in 100-nanosecond units
/// (the unit `wasapi::StreamMode::EventsShared::buffer_duration_hns`
/// expects), sized to exactly `dsp_core::FRAME_SAMPLES` (480 samples) at
/// `dsp_core::SAMPLE_RATE_HZ` (48kHz) = 10ms = 100_000 hns.
///
/// # Why this is requested explicitly instead of using `get_device_period()`
///
/// The reference Linux port (`clearnai-pipewire`) hit a real, measured bug
/// on PipeWire: without an explicit buffer-size parameter sent at
/// stream-connect time, the OS silently handed the stream a fixed 256ms
/// (12288-sample) buffer regardless of what any consumer requested — a
/// `node.latency` *property* string alone did nothing; only an explicit
/// `SPA_PARAM_Buffers` object at connect time actually constrained it (see
/// `clearnai-pipewire/src/format.rs::fixed_buffer_size_param` and
/// `virtual_mic.rs`'s "LATENCY FIX" comment for the measured before/after).
///
/// `capture.rs`/`render.rs`/`loopback.rs` previously used whatever
/// `AudioClient::get_device_period()` returned as `buffer_duration_hns`
/// (its `min_period` value) rather than requesting our own 10ms figure —
/// i.e. exactly the "trust the OS default" pattern that bit the reference
/// project on PipeWire. WASAPI's `EventsShared` mode already accepts an
/// explicit requested duration, so all three call `initialize_client` with
/// this constant instead, mirroring the lesson learned there: ask for the
/// buffer size the pipeline actually needs, don't assume the platform
/// default matches it.
///
/// This is speculative defense, not a verified fix: nobody has run this
/// against real WASAPI hardware from this Linux development environment,
/// so it is unconfirmed whether WASAPI shared-mode ever exhibits the
/// analogous "ignores the request" behavior PipeWire did. Explicitly
/// requesting 10ms is the correct thing to do regardless (it's what the
/// pipeline needs), but its *effectiveness* at actually getting a 480-frame
/// buffer on real hardware is unverified — see
/// `warn_if_buffer_size_mismatched` below for the runtime check that
/// surfaces it if the negotiated size doesn't match.
#[cfg(windows)]
pub(crate) const REQUESTED_BUFFER_DURATION_HNS: i64 =
    (dsp_core::FRAME_SAMPLES as i64 * 10_000_000) / dsp_core::SAMPLE_RATE_HZ as i64;

/// Speculative defense, unverified without real hardware: right after
/// `initialize_client()` (and before `start_stream()`), call this with the
/// just-initialized `AudioClient` to check what WASAPI actually negotiated
/// via `get_buffer_size()`. If it isn't exactly `dsp_core::FRAME_SAMPLES`
/// (480) — nor a whole multiple/divisor of it that the frame-chunking logic
/// in `capture.rs`/`render.rs`/`loopback.rs` can cleanly adapt to — this
/// logs a clear warning instead of silently assuming the request was
/// honored. This directly mirrors the reference PipeWire project's
/// hard-won lesson (see `REQUESTED_BUFFER_DURATION_HNS` docs above): a
/// buffer-size request can silently not be honored by the platform, and
/// that needs to be visible rather than assumed away. Not a substitute for
/// actually testing on real Windows hardware.
#[cfg(windows)]
pub(crate) fn warn_if_buffer_size_mismatched(audio_client: &wasapi::AudioClient, stream_label: &str) {
    match audio_client.get_buffer_size() {
        Ok(actual_frames) => {
            let actual = actual_frames as usize;
            let expected = dsp_core::FRAME_SAMPLES;
            let cleanly_adaptable = actual != 0
                && (actual == expected || actual % expected == 0 || expected % actual == 0);
            if !cleanly_adaptable {
                eprintln!(
                    "[audio-io] WARNING: {stream_label} negotiated a WASAPI buffer of {actual} \
                     frames, which is not {expected} (10ms @ {}Hz) nor a clean multiple/divisor \
                     of it. Requested {REQUESTED_BUFFER_DURATION_HNS} hns explicitly, but the \
                     platform may have ignored/rounded that request (unverified without real \
                     hardware - the reference Linux/PipeWire port hit exactly this kind of \
                     silent mismatch). Frame-based chunking downstream may behave unexpectedly.",
                    dsp_core::SAMPLE_RATE_HZ
                );
            }
        }
        Err(e) => {
            eprintln!(
                "[audio-io] WARNING: could not query negotiated WASAPI buffer size for \
                 {stream_label} to verify it matches the requested 10ms/{} frames: {e:#}",
                dsp_core::FRAME_SAMPLES
            );
        }
    }
}

/// Byte<->f32 helpers shared by capture/render/loopback for shuffling
/// samples in/out of the `VecDeque<u8>` buffers the `wasapi` crate's
/// `read_from_device_to_deque` / `write_to_device_from_deque` use.
#[cfg(windows)]
pub(crate) mod bytes {
    use std::collections::VecDeque;

    /// Pops one little-endian f32 sample off the front of `q`. Returns
    /// `None` if fewer than 4 bytes remain (caller should stop draining).
    pub fn pop_f32_le(q: &mut VecDeque<u8>) -> Option<f32> {
        if q.len() < 4 {
            return None;
        }
        let raw = [
            q.pop_front().unwrap(),
            q.pop_front().unwrap(),
            q.pop_front().unwrap(),
            q.pop_front().unwrap(),
        ];
        Some(f32::from_le_bytes(raw))
    }

    /// Appends one f32 sample to the back of `q` as little-endian bytes.
    pub fn push_f32_le(q: &mut VecDeque<u8>, sample: f32) {
        for b in sample.to_le_bytes() {
            q.push_back(b);
        }
    }
}
