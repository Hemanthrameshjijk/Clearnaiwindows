//! Real-time microphone capture via WASAPI shared-mode, event-driven
//! capture, feeding fixed `dsp_core::FRAME_SAMPLES` frames into a
//! `dsp_core::Producer<f32>`.
//!
//! Real-time-priority gap: the `wasapi` crate (as of 0.24, confirmed by
//! reading its source) does not expose any thread/MMCSS priority helper
//! (no wrapper around `AvSetMmThreadCharacteristicsW` or
//! `SetThreadPriority`). This module spawns a plain OS thread and does
//! *not* fake priority elevation - if the engine needs MMCSS "Pro Audio"
//! characteristics for this thread, that has to be done separately (e.g.
//! via a small `windows-sys` call wrapped around this thread's body), which
//! is out of scope for what this crate/dependency can honestly provide.

#![cfg(windows)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};
use dsp_core::{Producer, FRAME_SAMPLES};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, StreamMode};

use crate::bytes::pop_f32_le;
use crate::devices::{find_capture_device_by_name, open_capture_device_by_id};
use crate::{engine_wave_format, warn_if_buffer_size_mismatched, REQUESTED_BUFFER_DURATION_HNS};

// NOTE: `wasapi::Device` wraps a raw COM pointer (`IMMDevice`) and is not
// `Send` - confirmed by `cargo check` (E0277 on the very first draft of this
// module). WASAPI/COM objects are meant to be activated on the thread that
// uses them anyway (each thread needs its own COM apartment via
// `initialize_mta`), so instead of moving a `Device` into the capture
// thread, we move its `String` id across and re-open it with a fresh
// `DeviceEnumerator` from inside the thread.

/// Owns the mic-capture OS thread. Dropping (or calling `stop`) signals the
/// thread to stop and joins it.
pub struct MicCapture {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MicCapture {
    /// Opens the system default capture device and starts streaming
    /// 480-sample frames into `producer`.
    pub fn start_default(producer: Producer<f32>) -> Result<Self> {
        // Resolve the device id on the calling thread just to validate a
        // default device exists and fail fast; the id (not the Device
        // itself - see note above) is what actually crosses into the
        // capture thread.
        let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Capture)
            .context("opening default capture device")?;
        let device_id = device.get_id().context("reading default capture device id")?;
        Self::start_on_device_id(&device_id, producer)
    }

    /// Opens the capture device whose friendly name contains
    /// `name_substr` (case-insensitive) and starts streaming.
    pub fn start_by_name(name_substr: &str, producer: Producer<f32>) -> Result<Self> {
        let device = find_capture_device_by_name(name_substr)?
            .with_context(|| format!("no capture device matching '{name_substr}'"))?;
        let device_id = device.get_id().context("reading capture device id")?;
        Self::start_on_device_id(&device_id, producer)
    }

    /// Opens a specific capture device by its enumerated device id (see
    /// `devices::list_capture_devices`) and starts streaming.
    pub fn start_on_device_id(device_id: &str, producer: Producer<f32>) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let device_id = device_id.to_string();

        let handle = thread::Builder::new()
            .name("audio-io-mic-capture".into())
            .spawn(move || {
                let mut producer = producer;
                if let Err(e) = capture_loop(&device_id, &mut producer, &thread_stop) {
                    crate::report_error(&format!("[audio-io] mic capture thread exited with error: {e:#}"));
                }
            })
            .context("spawning mic capture thread")?;

        Ok(Self {
            stop_flag,
            handle: Some(handle),
        })
    }

    /// Signals the capture thread to stop and blocks until it exits.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn capture_loop(
    device_id: &str,
    producer: &mut Producer<f32>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<()> {
    // Every thread that touches WASAPI needs its own COM apartment, and
    // COM objects (like the `Device`/`IAudioClient` below) should be
    // activated on the thread that will use them - hence re-resolving the
    // device from its id here rather than receiving a `Device` from the
    // spawning thread.
    let _ = initialize_mta();

    let device = open_capture_device_by_id(device_id)
        .with_context(|| format!("re-opening capture device '{device_id}' on capture thread"))?;
    let mut audio_client = device
        .get_iaudioclient()
        .context("activating IAudioClient on capture device")?;

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

    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .context("initializing capture AudioClient (shared, event-driven)")?;

    // Speculative defense, unverified without real hardware: confirm WASAPI
    // actually honored the request above before we start streaming.
    warn_if_buffer_size_mismatched(&audio_client, "mic capture");

    let event_handle = audio_client
        .set_get_eventhandle()
        .context("getting capture event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("getting IAudioCaptureClient")?;

    audio_client
        .start_stream()
        .context("starting capture stream")?;

    let mut byte_queue: VecDeque<u8> = VecDeque::with_capacity(FRAME_SAMPLES * 4 * 4);
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);

    while !stop_flag.load(Ordering::Relaxed) {
        // A stalled event most likely means the device was removed/disabled
        // out from under us; surface that as a real error rather than
        // spinning silently forever.
        if event_handle.wait_for_event(1000).is_err() {
            bail!("capture event wait failed or timed out; capture device likely became unavailable");
        }

        loop {
            match capture_client
                .get_next_packet_size()
                .context("polling capture packet size")?
            {
                Some(_) => {}
                None => break,
            }

            capture_client
                .read_from_device_to_deque(&mut byte_queue)
                .context("reading captured audio from device")?;

            while let Some(sample) = pop_f32_le(&mut byte_queue) {
                pending.push(sample);
                if pending.len() == FRAME_SAMPLES {
                    push_frame(producer, &pending);
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

/// Pushes one full frame into the ring buffer. If the consumer isn't
/// keeping up and the ring buffer is full, the whole frame is dropped
/// (never block the realtime capture thread waiting for space) - this is a
/// real audible glitch under overload, and preferable to blocking WASAPI's
/// callback thread indefinitely.
fn push_frame(producer: &mut Producer<f32>, frame: &[f32]) {
    debug_assert_eq!(frame.len(), FRAME_SAMPLES);
    for &sample in frame {
        if producer.push(sample).is_err() {
            // Ring buffer full: drop the remainder of this frame rather
            // than blocking. Downstream will simply see a short gap.
            crate::report_error("[audio-io] mic capture ring buffer full, dropping frame");
            return;
        }
    }
}
