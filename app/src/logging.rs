//! Minimal file-backed logger for this app crate only.
//!
//! With `#![windows_subsystem = "windows"]` (see `main.rs`) the process has
//! no console, so `eprintln!`/`println!` output is simply never seen by
//! anyone. This module replaces those calls (in `main.rs`, `gui.rs`,
//! `engine.rs`, `setup.rs`, `settings.rs` only - other crates' own
//! `eprintln!`s are a known, unfixed gap, see the top-level report) with a
//! tiny appender that writes timestamped lines to
//! `<app_data_dir>/clearnai.log`.
//!
//! Deliberately not the `log` crate + a custom `Log` impl: a couple of thin
//! wrapper functions plus two macros is less code than wiring up a facade
//! crate for a single-binary app with no library consumers of the log
//! output.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// A long-running session logging a variety of *distinct* messages over
/// days/weeks (not just one repeated one - that case is already collapsed
/// by the de-duplication below) would otherwise grow `clearnai.log`
/// unbounded forever, same failure shape as the runaway-I/O bug described
/// below just on a much longer timescale. Past this size the file is
/// truncated back to empty (keeping the single open handle - no reopen,
/// same reasoning as the rest of this module) before the next line is
/// written, so it never grows past roughly this bound.
const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

/// Real-hardware finding: some of this app's realtime audio threads (mic
/// capture, hardware render) call `log_error!` on *every* dropped/underrun
/// frame - if a stall starts, that's once every ~10ms, forever, with no
/// automatic recovery. Two things about the old implementation made that
/// genuinely dangerous rather than just noisy:
///
/// 1. It reopened the log file from scratch (`OpenOptions::open`) on every
///    single call instead of keeping a handle open. Disk I/O for a fresh
///    open+append+close, repeated every ~10ms on a realtime audio thread,
///    can itself take longer than the 10ms frame budget once the file has
///    grown large (observed: 180+ MB from a single run) - which causes
///    *more* drops, which logs more, in a genuine runaway feedback loop.
///    This was a real, measured contributor to audible glitching, not a
///    theoretical concern.
/// 2. It had no de-duplication, so an unbroken run of identical messages
///    ("output ring buffer full, dropping frame", frame after frame) wrote
///    one line each, unbounded.
///
/// `LoggerState` now keeps the file open for the process lifetime and
/// collapses runs of the exact same message into "seen N times in the last
/// second", both fixing the runaway I/O and keeping the file readable.
struct LoggerState {
    file: Option<File>,
    last_msg: String,
    suppressed: u64,
    last_flush: Instant,
}

static STATE: OnceLock<Mutex<LoggerState>> = OnceLock::new();

/// Must be called once, as the very first thing in `main()`, before any
/// other code in this crate might log. `dir` is `setup::app_data_dir()`.
pub fn init(dir: &std::path::Path) {
    let _ = LOG_PATH.set(Some(dir.join("clearnai.log")));
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

fn state() -> &'static Mutex<LoggerState> {
    STATE.get_or_init(|| {
        let file = LOG_PATH
            .get()
            .and_then(|p| p.as_ref())
            .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());
        Mutex::new(LoggerState {
            file,
            last_msg: String::new(),
            suppressed: 0,
            // Far enough in the past that the very first message is never
            // itself treated as a suppressed repeat.
            last_flush: Instant::now() - Duration::from_secs(3600),
        })
    })
}

/// Appends one line to the log file, unless `init` was never called (then
/// this is a no-op). An unbroken run of the exact same message is collapsed
/// to at most one line per second (see `LoggerState` docs) - logging must
/// never itself become a source of realtime-audio-thread stalls.
pub fn write_line(level: &str, msg: &str) {
    if LOG_PATH.get().and_then(|p| p.as_ref()).is_none() {
        return;
    }
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());

    if msg == guard.last_msg && guard.last_flush.elapsed() < Duration::from_secs(1) {
        guard.suppressed += 1;
        return;
    }

    // A genuinely new message (or the same one, but a second+ has passed):
    // first flush a summary for whatever was being suppressed, describing
    // *that* message, not this new one.
    if guard.suppressed > 0 {
        let suppressed = guard.suppressed;
        if let Some(f) = guard.file.as_mut() {
            let _ = writeln!(
                f,
                "[{}] [{level}] (previous message repeated {suppressed} more time(s) in the last second)",
                timestamp()
            );
        }
        guard.suppressed = 0;
    }

    guard.last_msg = msg.to_string();
    guard.last_flush = Instant::now();
    if let Some(f) = guard.file.as_mut() {
        if f.metadata().map(|m| m.len()).unwrap_or(0) >= MAX_LOG_BYTES {
            if f.set_len(0).is_ok() {
                let _ = f.seek(SeekFrom::Start(0));
                let _ = writeln!(
                    f,
                    "[{}] [{level}] (log file exceeded {MAX_LOG_BYTES} bytes; truncated)",
                    timestamp()
                );
            }
        }
        let _ = writeln!(f, "[{}] [{level}] {msg}", timestamp());
    }
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::write_line("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::write_line("ERROR", &format!($($arg)*))
    };
}
