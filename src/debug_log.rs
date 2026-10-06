//! Minimal debug logging, active only when the process is launched with
//! `--debug`. Every call to `log()` is written to stdout (which, on
//! Windows, is a console window freshly allocated for this — see
//! `main.rs`) and appended to a log file next to the executable.
//!
//! Deliberately hand-rolled instead of pulling in `log`/`tracing`/
//! `env_logger`: the ask here is "print it and also save it to a file",
//! not a full logging framework with levels, filters, and subscribers.
//! Same reasoning for the timestamp — a small hand-rolled UTC calendar
//! conversion instead of adding `chrono`/`time` just to format one string.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ITEM_LOGS: AtomicUsize = AtomicUsize::new(0);

/// Per-scan cap on `log_item` lines, so a volume with millions of unreadable
/// entries can't produce a multi-gigabyte log.
const MAX_ITEM_LOGS: usize = 2000;
static LOG_FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Call once at startup when `--debug` was passed. Opens (or creates and
/// appends to) `wyvernscan-debug.log` next to the running executable, falling
/// back to the system temp directory if that location isn't writable, and
/// installs a panic hook so any panic anywhere in the app — not just the
/// ones already caught explicitly in `mft.rs` — gets recorded with its
/// message before the default handler also prints it.
pub fn init() {
    ENABLED.store(true, Ordering::Relaxed);

    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("wyvernscan-debug.log")))
        .unwrap_or_else(|| std::env::temp_dir().join("wyvernscan-debug.log"));

    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    let opened_path = path.display().to_string();
    let _ = LOG_FILE.set(Mutex::new(file));

    install_panic_hook();

    log(&format!("=== WyvernScan debug session started; logging to {opened_path} ==="));
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("PANIC: {info}"));
        default_hook(info);
    }));
}

pub fn log(message: &str) {
    if !is_enabled() {
        return;
    }
    let stamped = format!("[{}] {}", timestamp(), message);
    println!("{stamped}");
    if let Some(mutex) = LOG_FILE.get() {
        write_stamped_line(mutex, &stamped);
    }
}

/// Log one *per-item* problem (a directory that couldn't be read, a corrupt
/// record) — the things behind the "N errors" count in a finished scan. At
/// most `MAX_ITEM_LOGS` are written per scan; after that a single notice says
/// the rest are suppressed (the scan's own error count stays exact).
/// Returns whether `message` was actually logged.
pub fn log_item(message: &str) -> bool {
    if !is_enabled() {
        return false;
    }
    let n = ITEM_LOGS.fetch_add(1, Ordering::Relaxed);
    if n < MAX_ITEM_LOGS {
        log(message);
        true
    } else {
        if n == MAX_ITEM_LOGS {
            log(&format!(
                "... more than {MAX_ITEM_LOGS} per-item messages; the rest are suppressed (the error count is still exact)"
            ));
        }
        false
    }
}

/// Start a fresh `log_item` budget; called at the start of each scan.
pub fn reset_item_budget() {
    ITEM_LOGS.store(0, Ordering::Relaxed);
}

/// The actual file-writing step, pulled out of `log()` so it can be tested
/// against a throwaway file instead of the real global log (which, being a
/// `OnceLock`, can only be initialized once per process — awkward for a
/// test harness that may run many tests in one process).
fn write_stamped_line(mutex: &Mutex<Option<std::fs::File>>, stamped_line: &str) {
    if let Ok(mut guard) = mutex.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "{stamped_line}");
            let _ = file.flush();
        }
    }
}

/// Best-effort extraction of a human-readable message from a caught
/// panic's payload, for logging. Covers the two payload types the
/// standard panic machinery actually produces (`&str` for a string
/// literal, `String` for a formatted message); anything else is unusual
/// enough to just note as such.
///
/// Only called from the Windows-only `mft` module today, so this is
/// legitimately unused dead code on other platforms — not a bug.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(panic payload was not a string)".to_string()
    }
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (y, mo, d) = civil_from_days((now.as_secs() / 86400) as i64);
    let secs_of_day = now.as_secs() % 86400;
    let (h, mi, s) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Days-since-epoch to a proleptic Gregorian (year, month, day). Public
/// domain algorithm (Howard Hinnant's `civil_from_days`); avoids needing a
/// date/time crate for just this one conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01 is day 0 by definition.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01 is a well-known reference date for this algorithm.
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
    }

    /// Exercises the actual file-writing path end to end against a real
    /// temp file, rather than just trusting the logic by inspection.
    #[test]
    fn per_item_logging_is_capped_and_resettable() {
        ENABLED.store(true, Ordering::Relaxed);
        reset_item_budget();
        let logged = (0..MAX_ITEM_LOGS + 50).filter(|i| log_item(&format!("item {i}"))).count();
        assert_eq!(logged, MAX_ITEM_LOGS);
        reset_item_budget();
        assert!(log_item("fresh budget"));
        ENABLED.store(false, Ordering::Relaxed);
        assert!(!log_item("disabled"));
    }

    #[test]
    fn write_stamped_line_appends_to_file() {
        let path = std::env::temp_dir().join(format!("wyvernscan_debug_log_test_{}.log", std::process::id()));
        let file = OpenOptions::new().create(true).write(true).truncate(true).open(&path).unwrap();
        let mutex = Mutex::new(Some(file));

        write_stamped_line(&mutex, "first line");
        write_stamped_line(&mutex, "second line");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("first line"));
        assert!(contents.contains("second line"));
        assert!(contents.find("first line").unwrap() < contents.find("second line").unwrap());

        std::fs::remove_file(&path).unwrap();
    }
}
