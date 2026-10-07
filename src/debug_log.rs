//! Minimal debug logging, active only when the process is launched with
//! `--debug`. Every call to `log()` is written to the console and appended
//! to a log file next to the executable. The console is stdout for the GUI
//! (on Windows a console window freshly allocated for this — see `main.rs`)
//! but **stderr in `--cli` mode** (`route_to_stderr`), so `--json` output on
//! stdout is never mixed with log lines.
//!
//! Deliberately hand-rolled instead of pulling in `log`/`tracing`/
//! `env_logger`: the ask here is "print it and also save it to a file",
//! not a full logging framework with levels, filters, and subscribers.
//! Same reasoning for the timestamp — a small hand-rolled UTC calendar
//! conversion instead of adding `chrono`/`time` just to format one string.

use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

static ENABLED: AtomicBool = AtomicBool::new(false);
static TO_STDERR: AtomicBool = AtomicBool::new(false);

/// Send console log lines to stderr instead of stdout. Called for `--cli`
/// before `init()`: stdout carries the report there (`--json > report.json`).
pub fn route_to_stderr() {
    TO_STDERR.store(true, Ordering::Relaxed);
}

// Per-thread counter for `log_item`.
//
// This is intentionally thread-local rather than a process-global atomic.
// A scan resets its own budget at the beginning of the scan, and all
// per-item logging performed by that scan happens on the same scan thread.
// Keeping the counter thread-local means a parallel test/scan cannot reset
// another thread's budget.
//
// This also preserves the existing `reset_item_budget()` API, so callers
// do not need to change.
thread_local! {
    static ITEM_LOGS: Cell<usize> = const { Cell::new(0) };
}

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
    install_panic_hook();

    // Next to the executable first, then the temp dir (the executable's
    // folder is often read-only, e.g. /usr/local/bin or Program Files).
    let name = "wyvernscan-debug.log";
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        candidates.push(dir.join(name));
    }
    candidates.push(std::env::temp_dir().join(name));

    let mut failures = Vec::new();
    let mut opened: Option<(std::path::PathBuf, std::fs::File)> = None;
    for path in candidates {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                opened = Some((path, f));
                break;
            }
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }

    let (where_to, file) = match opened {
        Some((p, f)) => (format!("logging to {}", p.display()), Some(f)),
        None => ("console only, no log file could be created".to_string(), None),
    };
    let _ = LOG_FILE.set(Mutex::new(file));

    log(&format!("=== WyvernScan debug session started; {where_to} ==="));
    for f in failures {
        log(&format!("could not open log file {f}"));
    }
    log_system_info();
}

/// Facts that explain most "works here, not there" reports: version, OS,
/// environment (WSL/container), user, CPU count, terminal state, arguments.
/// Pure data gathering, so it is testable.
pub fn system_info_lines() -> Vec<String> {
    use std::io::IsTerminal;
    let mut v = Vec::new();
    v.push(format!(
        "WyvernScan {} ({}/{}, {} build)",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) { "debug" } else { "release" }
    ));
    v.push(format!("arguments: {:?}", std::env::args().skip(1).collect::<Vec<_>>()));
    if let Ok(exe) = std::env::current_exe() {
        v.push(format!("executable: {}", exe.display()));
    }
    if let Ok(cwd) = std::env::current_dir() {
        v.push(format!("working directory: {}", cwd.display()));
    }
    v.push(format!(
        "process id {}, {} CPU(s) (scan worker threads: {})",
        std::process::id(),
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        num_cpus::get()
    ));
    v.push(format!(
        "stdout is a terminal: {}, stderr is a terminal: {}",
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal()
    ));
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        v.push(format!(
            "effective user id {uid}{} -- unreadable folders are counted as errors",
            if uid == 0 { " (root)" } else { " (not root)" }
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let wsl = osrelease.to_lowercase().contains("microsoft");
        v.push(format!("kernel {osrelease}{}", if wsl { " (WSL)" } else { "" }));
        if std::path::Path::new("/.dockerenv").exists() {
            v.push("running inside a Docker container".to_string());
        }
    }
    v
}

pub fn log_system_info() {
    for line in system_info_lines() {
        log(&line);
    }
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

    // Errors are ignored on purpose: a closed pipe (`| head`) must not turn
    // a diagnostic line into a panic, least of all inside the panic hook.
    if TO_STDERR.load(Ordering::Relaxed) {
        let _ = writeln!(std::io::stderr(), "{stamped}");
    } else {
        let _ = writeln!(std::io::stdout(), "{stamped}");
    }

    if let Some(mutex) = LOG_FILE.get() {
        write_stamped_line(mutex, &stamped);
    }
}

/// Log one *per-item* problem (a directory that couldn't be read, a corrupt
/// record) — the things behind the "N errors" count in a finished scan.
///
/// At most `MAX_ITEM_LOGS` are written per scan/thread; after that a single
/// notice says the rest are suppressed (the scan's own error count stays
/// exact).
///
/// The counter is thread-local, so parallel scans/tests have independent
/// budgets and cannot reset one another's counters.
///
/// Returns whether `message` was actually logged.
pub fn log_item(message: &str) -> bool {
    if !is_enabled() {
        return false;
    }

    ITEM_LOGS.with(|count| {
        let n = count.get();
        count.set(n.saturating_add(1));

        if n < MAX_ITEM_LOGS {
            log(message);
            true
        } else {
            if n == MAX_ITEM_LOGS {
                log(&format!(
                    "... more than {MAX_ITEM_LOGS} per-item messages; \
                     the rest are suppressed (the error count is still exact)"
                ));
            }

            false
        }
    })
}

/// Start a fresh `log_item` budget; called at the start of each scan.
///
/// The budget is thread-local, so resetting it only affects the scan/test
/// running on the current thread. This makes the logging budget safe when
/// Rust's test harness runs tests in parallel.
pub fn reset_item_budget() {
    ITEM_LOGS.with(|count| {
        count.set(0);
    });
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

    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// Days-since-epoch to a proleptic Gregorian (year, month, day). Public
/// domain algorithm (Howard Hinnant's `civil_from_days`); avoids needing a
/// date/time crate for just this one conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;

    let era = if z >= 0 {
        z
    } else {
        z - 146096
    } / 146097;

    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;

    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;

    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;

    let m = if mp < 10 {
        mp + 3
    } else {
        mp - 9
    } as u32;

    (
        if m <= 2 {
            y + 1
        } else {
            y
        },
        m,
        d,
    )
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
    ///
    /// The test deliberately leaves the Rust test harness free to run this
    /// in parallel with other tests. `ITEM_LOGS` is thread-local, so another
    /// test calling `reset_item_budget()` cannot interfere with this test.
    #[test]
    fn per_item_logging_is_capped_and_resettable() {
        ENABLED.store(true, Ordering::Relaxed);

        reset_item_budget();

        let logged = (0..MAX_ITEM_LOGS + 50)
            .filter(|i| log_item(&format!("item {i}")))
            .count();

        assert_eq!(logged, MAX_ITEM_LOGS);

        reset_item_budget();

        assert!(log_item("fresh budget"));

        ENABLED.store(false, Ordering::Relaxed);

        assert!(!log_item("disabled"));
    }

    #[test]
    fn system_info_names_version_and_os() {
        let lines = system_info_lines();
        assert!(lines[0].contains(env!("CARGO_PKG_VERSION")));
        assert!(lines[0].contains(std::env::consts::OS));
        assert!(lines.iter().any(|l| l.starts_with("arguments:")));
    }

    #[test]
    fn write_stamped_line_appends_to_file() {
        let path = std::env::temp_dir().join(format!(
            "wyvernscan_debug_log_test_{}.log",
            std::process::id()
        ));

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();

        let mutex = Mutex::new(Some(file));

        write_stamped_line(&mutex, "first line");
        write_stamped_line(&mutex, "second line");

        let contents = std::fs::read_to_string(&path).unwrap();

        assert!(contents.contains("first line"));
        assert!(contents.contains("second line"));
        assert!(
            contents.find("first line").unwrap()
                < contents.find("second line").unwrap()
        );

        std::fs::remove_file(&path).unwrap();
    }
}