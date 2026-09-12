//! WASAPI loopback capture: a non-destructive read-only tap of whatever is
//! currently playing on a render device.
//!
//! Per the `wasapi` crate's actual mechanism (confirmed by reading its
//! source: `src/api.rs`, the `initialize_client` implementation): loopback
//! is *not* a separate constructor or an explicit boolean flag. Instead you
//! open a **render** device's `AudioClient` as usual, then call
//! `initialize_client` with `Direction::Capture` (the render/capture
//! mismatch, in shared mode, is what makes the crate set
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` internally). That is the pattern used
//! below. (The crate separately exposes
//! `AudioClient::new_application_loopback_client` for *per-process* loopback
//! capture, which is a different feature - single-process capture rather
//! than "whatever is playing on this endpoint" - and is not what this
//! module uses.)

#![cfg(windows)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dsp_core::{Producer, FRAME_SAMPLES, SAMPLE_RATE_HZ};
use wasapi::{initialize_mta, AudioClient, Device, DeviceEnumerator, Direction, StreamMode};

use crate::bytes::pop_f32_le;
use crate::devices::{find_render_device_by_name, open_render_device_by_id};
use crate::{engine_wave_format, warn_if_buffer_size_mismatched, REQUESTED_BUFFER_DURATION_HNS};

// NOTE: same as capture.rs/render.rs - `wasapi::Device` is not `Send` (raw
// COM pointer), so only the device id `String` crosses into the tap thread;
// `open_loopback` is called from inside that thread with a freshly
// re-opened `Device`. `record_loopback_to_wav` doesn't spawn a thread at
// all (it's a synchronous, blocking helper), so it opens the `Device`
// directly.

/// Owns the loopback-tap OS thread. Dropping (or calling `stop`) signals
/// the thread to stop and joins it.
pub struct LoopbackTap {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackTap {
    /// Taps whatever is playing on the system default render device.
    pub fn start_default(producer: Producer<f32>) -> Result<Self> {
        let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .context("opening default render device for loopback tap")?;
        let device_id = device.get_id().context("reading default render device id")?;
        Self::start_on_device_id(&device_id, producer)
    }

    /// Taps whatever is playing on the render device whose friendly name
    /// contains `name_substr` (case-insensitive) - e.g. "CABLE Input" to
    /// tap what the engine itself just rendered into VB-Cable.
    pub fn start_by_name(name_substr: &str, producer: Producer<f32>) -> Result<Self> {
        let device = find_render_device_by_name(name_substr)?
            .with_context(|| format!("no render device matching '{name_substr}'"))?;
        let device_id = device.get_id().context("reading render device id")?;
        Self::start_on_device_id(&device_id, producer)
    }

    /// Opens a specific render device by its enumerated device id and taps
    /// it, without going through name matching or the OS default.
    pub fn start_on_device_id(device_id: &str, producer: Producer<f32>) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let device_id = device_id.to_string();

        let handle = thread::Builder::new()
            .name("audio-io-loopback-tap".into())
            .spawn(move || {
                let mut producer = producer;
                if let Err(e) = loopback_loop(&device_id, &mut producer, &thread_stop) {
                    crate::report_error(&format!("[audio-io] loopback tap thread exited with error: {e:#}"));
                }
            })
            .context("spawning loopback tap thread")?;

        Ok(Self {
            stop_flag,
            handle: Some(handle),
        })
    }

    /// Signals the tap thread to stop and blocks until it exits.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for LoopbackTap {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Opens `device` (a **render** device) in WASAPI loopback mode and returns
/// the ready-to-use `AudioClient` plus its event handle and capture client.
/// Shared helper used by both the streaming tap and the WAV test-mode
/// capture below.
fn open_loopback(device: &Device, stream_label: &str) -> Result<(AudioClient, wasapi::Handle, wasapi::AudioCaptureClient)> {
    let mut audio_client = device
        .get_iaudioclient()
        .context("activating IAudioClient on render device (for loopback)")?;

    let format = engine_wave_format();

    // Explicitly request a 10ms (480-sample) buffer rather than trusting
    // whatever `get_device_period()`'s default/min period happens to be -
    // see `REQUESTED_BUFFER_DURATION_HNS` docs in `lib.rs` for why (the
    // reference PipeWire port silently got a 256ms buffer until it did the
    // analogous thing explicitly).
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: REQUESTED_BUFFER_DURATION_HNS,
    };

    // The Render-device + Capture-direction mismatch here is what puts this
    // AudioClient into loopback mode - see module docs.
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .context("initializing loopback AudioClient (render device, capture direction, shared/event-driven)")?;

    // Speculative defense, unverified without real hardware: confirm WASAPI
    // actually honored the request above before we start streaming.
    warn_if_buffer_size_mismatched(&audio_client, stream_label);

    let event_handle = audio_client
        .set_get_eventhandle()
        .context("getting loopback event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("getting IAudioCaptureClient for loopback")?;

    Ok((audio_client, event_handle, capture_client))
}

fn loopback_loop(
    device_id: &str,
    producer: &mut Producer<f32>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<()> {
    let _ = initialize_mta();

    let device = open_render_device_by_id(device_id)
        .with_context(|| format!("re-opening render device '{device_id}' on loopback thread"))?;
    let (audio_client, event_handle, capture_client) = open_loopback(&device, "loopback tap")?;
    audio_client
        .start_stream()
        .context("starting loopback capture stream")?;

    let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(FRAME_SAMPLES * 4 * 4);
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);

    while !stop_flag.load(Ordering::Relaxed) {
        if event_handle.wait_for_event(1000).is_err() {
            bail!("loopback event wait failed or timed out; render device likely became unavailable");
        }

        loop {
            match capture_client
                .get_next_packet_size()
                .context("polling loopback packet size")?
            {
                Some(_) => {}
                None => break,
            }

            capture_client
                .read_from_device_to_deque(&mut byte_queue)
                .context("reading loopback audio from device")?;

            while let Some(sample) = pop_f32_le(&mut byte_queue) {
                pending.push(sample);
                if pending.len() == FRAME_SAMPLES {
                    for &s in &pending {
                        if producer.push(s).is_err() {
                            crate::report_error("[audio-io] loopback ring buffer full, dropping frame");
                            break;
                        }
                    }
                    pending.clear();
                }
            }
        }

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
    }

    audio_client.stop_stream().ok();
    Ok(())
}

/// Test-mode verification hook: captures `seconds` of loopback audio from
/// the system default render device and writes it to a real 32-bit-float
/// mono WAV file at `out_path`. This is a synchronous, blocking call meant
/// to be run standalone (e.g. from a small test binary or `#[test]` marked
/// `#[ignore]` and run manually on real Windows hardware with something
/// audible playing) to concretely prove the loopback tap works, since none
/// of this can be exercised from this Linux development environment.
pub fn record_loopback_to_wav(seconds: f32, out_path: &Path) -> Result<()> {
    if seconds <= 0.0 {
        bail!("seconds must be positive, got {seconds}");
    }

    let _ = initialize_mta();

    let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .context("opening default render device for loopback test capture")?;

    let (audio_client, event_handle, capture_client) = open_loopback(&device, "loopback WAV test capture")?;
    audio_client
        .start_stream()
        .context("starting loopback capture stream")?;

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE_HZ,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(out_path, spec)
        .with_context(|| format!("creating WAV file at {}", out_path.display()))?;

    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(FRAME_SAMPLES * 4 * 4);

    while Instant::now() < deadline {
        if event_handle.wait_for_event(1000).is_err() {
            bail!("loopback event wait failed or timed out during test capture");
        }

        loop {
            match capture_client
                .get_next_packet_size()
                .context("polling loopback packet size")?
            {
                Some(_) => {}
                None => break,
            }

            capture_client
                .read_from_device_to_deque(&mut byte_queue)
                .context("reading loopback audio from device")?;

            while let Some(sample) = pop_f32_le(&mut byte_queue) {
                writer
                    .write_sample(sample)
                    .context("writing sample to WAV file")?;
            }
        }
    }

    audio_client.stop_stream().ok();
    writer.finalize().context("finalizing WAV file")?;
    Ok(())
}
