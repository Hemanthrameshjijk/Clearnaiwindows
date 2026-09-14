//! Real-time hardware render via WASAPI shared-mode, event-driven playback,
//! pulling fixed `dsp_core::FRAME_SAMPLES` frames from a
//! `dsp_core::Consumer<f32>` and writing them to the device.
//!
//! Same real-time-priority gap as `capture.rs`: the `wasapi` crate exposes
//! no MMCSS/thread-priority helper, so this thread runs at normal OS
//! scheduling priority. Not faked here.

#![cfg(windows)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};
use dsp_core::{Consumer, FRAME_SAMPLES};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, StreamMode};

use crate::bytes::push_f32_le;
use crate::devices::{find_render_device_by_name, open_render_device_by_id};
use crate::{
    engine_wave_format, sleep_respecting_stop, warn_if_buffer_size_mismatched, RECONNECT_BACKOFF,
    REQUESTED_BUFFER_DURATION_HNS,
};

// NOTE: same as capture.rs - `wasapi::Device` is not `Send` (raw COM
// pointer), so only the device id `String` crosses into the render thread;
// the `Device`/`IAudioClient` are (re)activated from inside that thread.

/// Owns the hardware-render OS thread. Dropping (or calling `stop`) signals
/// the thread to stop and joins it.
pub struct HardwareRender {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HardwareRender {
    /// Opens the system default render device and starts pulling frames
    /// from `consumer` to play out.
    pub fn start_default(consumer: Consumer<f32>) -> Result<Self> {
        let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .context("opening default render device")?;
        let device_id = device.get_id().context("reading default render device id")?;
        Self::start_on_device_id(&device_id, consumer)
    }

    /// Opens the render device whose friendly name contains `name_substr`
    /// (case-insensitive) - e.g. "CABLE Input" to render into VB-Cable
    /// instead of the user's real speakers.
    pub fn start_by_name(name_substr: &str, consumer: Consumer<f32>) -> Result<Self> {
        let device = find_render_device_by_name(name_substr)?
            .with_context(|| format!("no render device matching '{name_substr}'"))?;
        let device_id = device.get_id().context("reading render device id")?;
        Self::start_on_device_id(&device_id, consumer)
    }

    /// Opens a specific render device by its enumerated device id (see
    /// `devices::list_render_devices` / `devices::find_vb_cable_endpoints`).
    /// This is the entry point the virtual-mic pipeline uses to render
    /// specifically into "CABLE Input" rather than whatever the OS default
    /// happens to be.
    pub fn start_on_device_id(device_id: &str, consumer: Consumer<f32>) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let device_id = device_id.to_string();

        let handle = thread::Builder::new()
            .name("audio-io-hw-render".into())
            .spawn(move || {
                let mut consumer = consumer;
                // Best-effort: see `mmcss.rs` - failure is silent and
                // harmless (thread just stays at normal priority). Held for
                // the whole thread's lifetime, across reconnects, not
                // re-acquired per `render_loop` call below.
                let _mmcss_guard = crate::mmcss::elevate_current_thread();
                // Real hardware finding: a WASAPI failure here (device
                // disabled/removed, sleep/wake, another app grabbing
                // exclusive mode) is very often transient, but this loop
                // used to just exit on the first `render_loop` error -
                // permanently killing playback until the whole app was
                // restarted, while the pipeline thread on the other end of
                // `consumer` kept running and spun forever logging "output
                // ring buffer full" once nothing was left to drain it. Retry
                // with a backoff instead, so the device is transparently
                // reopened once it comes back (or once shared/exclusive
                // contention clears), matching the "always warm, self
                // healing" behavior the rest of this crate already has for
                // toggles.
                while !thread_stop.load(Ordering::Relaxed) {
                    if let Err(e) = render_loop(&device_id, &mut consumer, &thread_stop) {
                        crate::report_error(&format!(
                            "[audio-io] hardware render thread error, retrying in {}s: {e:#}",
                            RECONNECT_BACKOFF.as_secs()
                        ));
                    }
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    sleep_respecting_stop(RECONNECT_BACKOFF, &thread_stop);
                }
            })
            .context("spawning hardware render thread")?;

        Ok(Self {
            stop_flag,
            handle: Some(handle),
        })
    }

    /// Signals the render thread to stop and blocks until it exits.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for HardwareRender {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn render_loop(
    device_id: &str,
    consumer: &mut Consumer<f32>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<()> {
    let _ = initialize_mta();

    let device = open_render_device_by_id(device_id)
        .with_context(|| format!("re-opening render device '{device_id}' on render thread"))?;
    let mut audio_client = device
        .get_iaudioclient()
        .context("activating IAudioClient on render device")?;

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
        .initialize_client(&format, &Direction::Render, &mode)
        .context("initializing render AudioClient (shared, event-driven)")?;

    // Speculative defense, unverified without real hardware: confirm WASAPI
    // actually honored the request above before we start streaming.
    warn_if_buffer_size_mismatched(&audio_client, "hardware render");

    let event_handle = audio_client
        .set_get_eventhandle()
        .context("getting render event handle")?;
    let render_client = audio_client
        .get_audiorenderclient()
        .context("getting IAudioRenderClient")?;

    // Pre-fill one buffer's worth of silence before starting the stream, as
    // WASAPI shared-mode render expects the buffer primed before the clock
    // starts ticking.
    let buffer_frames = audio_client
        .get_buffer_size()
        .context("querying render buffer size")?;
    {
        let mut silence: VecDeque<u8> = VecDeque::with_capacity(buffer_frames as usize * 4);
        for _ in 0..buffer_frames {
            push_f32_le(&mut silence, 0.0);
        }
        render_client
            .write_to_device_from_deque(buffer_frames as usize, &mut silence, None)
            .context("priming render buffer with silence")?;
    }

    audio_client
        .start_stream()
        .context("starting render stream")?;

    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);
    let mut out_bytes: VecDeque<u8> = VecDeque::with_capacity(FRAME_SAMPLES * 4 * 4);

    while !stop_flag.load(Ordering::Relaxed) {
        if event_handle.wait_for_event(1000).is_err() {
            bail!("render event wait failed or timed out; render device likely became unavailable");
        }

        let available = audio_client
            .get_available_space_in_frames()
            .context("querying available render buffer space")?;
        if available == 0 {
            continue;
        }

        // Pull whole FRAME_SAMPLES-sized chunks from the ring buffer until
        // we have enough samples to fill the available device buffer space,
        // padding the tail with silence on underrun so WASAPI never sees a
        // short write.
        while pending.len() < available as usize {
            let start = pending.len();
            pending.resize(start + FRAME_SAMPLES, 0.0);
            let got = pull_frame(consumer, &mut pending[start..]);
            if got < FRAME_SAMPLES {
                crate::report_error(&format!(
                    "[audio-io] render ring buffer underrun, padding {} samples of silence",
                    FRAME_SAMPLES - got
                ));
            }
        }

        out_bytes.clear();
        for &sample in pending.iter().take(available as usize) {
            push_f32_le(&mut out_bytes, sample);
        }
        render_client
            .write_to_device_from_deque(available as usize, &mut out_bytes, None)
            .context("writing render audio to device")?;

        pending.drain(0..available as usize);

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
    }

    audio_client.stop_stream().ok();
    Ok(())
}

/// Pulls up to `out.len()` samples from `consumer` into `out`, returning how
/// many real samples were copied. Any remaining tail of `out` is left as
/// whatever it already was (callers pre-zero it), i.e. silence on underrun.
fn pull_frame(consumer: &mut Consumer<f32>, out: &mut [f32]) -> usize {
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
