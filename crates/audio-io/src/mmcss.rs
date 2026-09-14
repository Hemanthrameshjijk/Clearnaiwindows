//! MMCSS ("Multimedia Class Scheduler Service") thread elevation.
//!
//! Real hardware finding: with capture/render/pipeline threads all running
//! at plain `THREAD_PRIORITY_NORMAL`, real logs show sustained, chronic
//! `mic capture ring buffer full`/`render ring buffer underrun` storms - not
//! just an occasional blip, but every frame, for minutes at a time. That is
//! the textbook symptom of a realtime audio thread not being scheduled
//! promptly enough under system load, which on Windows is exactly what
//! MMCSS's "Pro Audio" task category exists to fix: it asks the scheduler to
//! treat this thread the way a DAW's or a voice-chat app's audio thread is
//! treated (short, guaranteed scheduling quanta, boosted above normal
//! background work) instead of competing as an ordinary thread. This module
//! was the "real-time-priority gap" already called out in `capture.rs`'s and
//! `render.rs`'s doc comments as a known, not-yet-implemented gap.
//!
//! Every WASAPI worker thread (mic capture, hardware render, loopback tap)
//! and both DSP pipeline threads (`app::engine::spawn_mic_pipeline`/
//! `spawn_speaker_pipeline`) should call `elevate_current_thread` once, right
//! after the thread starts, and hold onto the returned guard for the
//! thread's entire lifetime (dropping it reverts the elevation, which should
//! only happen at thread exit).

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW};

/// RAII guard for one thread's MMCSS elevation. Reverts automatically when
/// dropped (i.e. hold this for as long as the thread should stay elevated -
/// typically its entire lifetime).
pub struct MmcssGuard(windows::Win32::Foundation::HANDLE);

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned by a successful
        // `AvSetMmThreadCharacteristicsW` call and has not been reverted yet
        // (this is the only place that reverts it, and it runs at most once
        // per guard).
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

/// Elevates the calling thread to the MMCSS "Pro Audio" task category.
/// Best-effort: real hardware finding is that this project cannot assume
/// every target machine grants this (group policy, older Windows builds,
/// a locked-down environment), so a failure here must never be fatal - the
/// thread simply keeps running at normal priority, same as before this
/// module existed. Returns `None` on failure; callers that get `None` are
/// not expected to log it as an error (see call sites), only to proceed.
pub fn elevate_current_thread() -> Option<MmcssGuard> {
    let mut task_index: u32 = 0;
    // "Pro Audio" is the standard MMCSS task name for exclusive, low-latency
    // realtime audio processing (the same category real-world DAWs and
    // voice-chat apps register their audio threads under). Must be a
    // null-terminated UTF-16 string for the `PCWSTR` FFI boundary.
    let name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    // SAFETY: `name` is a valid null-terminated UTF-16 buffer kept alive for
    // the duration of this call; `task_index` is a valid `&mut u32` for the
    // out-parameter.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(name.as_ptr()), &mut task_index) };
    match handle {
        Ok(h) if !h.is_invalid() => Some(MmcssGuard(h)),
        _ => None,
    }
}
