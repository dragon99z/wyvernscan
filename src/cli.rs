//! Console-only mode: `wyvernscan --cli <path>`. For servers with no display
//! at all — this module never imports or touches `eframe`/`egui`/winit, so
//! running it doesn't require (or attempt to open) any graphical context.
//! Runs the same scanner as the GUI, prints a text or JSON report to
//! stdout, and exits with a real process exit code (0 on success, nonzero
//! on a bad argument or a scan that failed outright) for use in scripts
//! and cron jobs.

use crate::scanner::{self, merge_roots, Node, RootEntry, ScanMessage, ScanResult};
use humansize::{format_size, BINARY};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Auto,
    Normal,
    Mft,
}

pub struct CliArgs {
    target: Target,
    mode: Mode,
    top: usize,
    depth: usize,
    json: bool,
    banner: bool,
    excludes: Vec<PathBuf>,
}

enum Target {
    Path(PathBuf),
    EntireSystem,
}

/// Every exit from console mode goes through here. On Windows, a console
/// window this process opened for itself would vanish instantly, taking the
/// output (including `--help` and error messages) with it, so it is held open
/// until Enter is pressed.
pub fn exit(code: i32) -> ! {
    crate::debug_log::log(&format!("exiting with code {code}"));
    #[cfg(windows)]
    crate::win_integration::pause_if_new_console();
    std::process::exit(code)
}

/// `None` means "--cli wasn't given at all, launch the GUI as normal" —
/// the caller in `main.rs` never even looks at any other argument unless
/// this returns `Some`, so a plain `wyvernscan.exe` with no arguments is
/// completely unaffected by anything in this module.
pub fn parse_args() -> Option<CliArgs> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--cli") {
        return None;
    }

    let mut target = None;
    let mut mode = Mode::Auto;
    let mut top = 20usize;
    let mut depth = 3usize;
    let mut json = false;
    let mut full = false;
    let mut banner = true;
    let mut excludes: Vec<PathBuf> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i).cloned()
        };
        match arg {
            "--cli" | "--debug" => {} // --debug is handled separately in main.rs
            "--entire-system" => target = Some(Target::EntireSystem),
            "--json" => json = true,
            "--no-banner" => banner = false,
            "--full" => full = true,
            "--exclude" => match next() {
                // One or several folders; several are separated like PATH
                // (':' on Unix, ';' on Windows, where ':' is part of 'C:').
                Some(v) if !v.is_empty() => {
                    excludes.extend(std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()));
                }
                _ => {
                    eprintln!("--exclude needs a folder (or several, separated like PATH). Try --help.");
                    exit(2);
                }
            },
            "--mode" => {
                mode = match next().as_deref() {
                    Some("normal") => Mode::Normal,
                    Some("mft") => Mode::Mft,
                    Some("auto") | None => Mode::Auto,
                    Some(other) => {
                        eprintln!("Unknown --mode '{other}' (expected auto, normal, or mft).");
                        exit(2);
                    }
                };
            }
            "--top" => {
                top = next().and_then(|v| v.parse().ok()).unwrap_or(top);
            }
            "--depth" => {
                depth = next().and_then(|v| v.parse().ok()).unwrap_or(depth);
            }
            "--help" | "-h" => {
                print_help();
                exit(0);
            }
            other if !other.starts_with('-') => {
                target = Some(Target::Path(PathBuf::from(other)));
            }
            other => {
                eprintln!("Unknown option '{other}'. Try --help.");
                exit(2);
            }
        }
        i += 1;
    }

    if full {
        top = usize::MAX;
        depth = usize::MAX;
    }

    let target = match target {
        Some(t) => t,
        None => {
            eprintln!("--cli needs a path to scan, or --entire-system. Try --help.");
            exit(2);
        }
    };

    Some(CliArgs { target, mode, top, depth, json, banner, excludes })
}

fn print_help() {
    crate::banner::print_to_stdout();
    println!(
        "\
WyvernScan console mode — for servers/scripts with no graphical display.

USAGE:
    wyvernscan --cli <path> [options]
    wyvernscan --cli --entire-system [options]

OPTIONS:
    --entire-system   Scan every detected local drive/volume, merged into one report
    --mode <auto|normal|mft>
                      Scan strategy. 'mft' only applies on Windows, needs Administrator,
                      and only works scanning a whole drive root. Default: auto
    --exclude <paths> Don't read these folders; they show up as empty 0 B entries. Repeat the
                      option, or separate several paths like PATH (':' on Unix, ';' on
                      Windows). Typical use: --exclude /mnt/c to keep WSL's Windows drive
                      out of a scan of /. A folder that is also a drive/volume is dropped
                      from --entire-system. Folders only (not single files); disables the
                      Windows MFT fast scan, which always reads the whole drive.
    --top <N>         Show at most N entries per folder level. Default: 20
    --depth <N>       Show at most N levels deep. Default: 3
    --full            Ignore --top/--depth limits and print everything
    --json            Print a JSON report instead of human-readable text
    --no-banner       Don't print the ASCII-art banner (it is only shown on a terminal)
    --debug           Log full detail to stderr and wyvernscan-debug.log: system info,
                      scan decisions, skipped virtual filesystems, progress every 2s,
                      every unreadable path, and a result summary. stdout stays clean,
                      so --json output is still valid.
    --help            Show this message

EXIT CODES:
    0   scan completed (even if some files/folders were unreadable)
    1   scan failed outright (bad path, nothing readable, etc.)
    2   bad arguments
"
    );
}

/// Runs the requested scan to completion (blocking — there's no GUI to
/// keep responsive) and prints the report. Returns the process exit code.
pub fn run(args: CliArgs) -> i32 {
    if args.banner {
        crate::banner::print_to_stderr();
    }
    let threads = num_cpus::get();
    let cancel = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::unbounded();

    let target_desc = match &args.target {
        Target::Path(p) => p.display().to_string(),
        Target::EntireSystem => "Entire System".to_string(),
    };
    eprintln!("Scanning {target_desc}...");
    if !args.excludes.is_empty() {
        let list: Vec<String> = args.excludes.iter().map(|p| p.display().to_string()).collect();
        crate::debug_log::log(&format!("excluding: {}", list.join(", ")));
    }
    crate::debug_log::log(&format!(
        "CLI options: target={target_desc} mode={:?} top={} depth={} json={} threads={threads}",
        args.mode,
        if args.top == usize::MAX { "all".to_string() } else { args.top.to_string() },
        if args.depth == usize::MAX { "all".to_string() } else { args.depth.to_string() },
        args.json
    ));

    let result = match &args.target {
        Target::Path(path) => std::thread::scope(|scope| {
            scope.spawn(|| run_single(path.clone(), args.mode, threads, &tx, &cancel, &args.excludes));
            drain_for_result(&rx)
        }),
        Target::EntireSystem => run_entire_system(args.mode, threads, &cancel, &args.excludes),
    };

    let result = match result {
        Some(r) => r,
        None => {
            eprintln!("Scan failed: nothing could be read.");
            crate::debug_log::log("scan produced no result (see the lines above for why)");
            return 1;
        }
    };
    if crate::debug_log::is_enabled() {
        log_result_summary(&result, &args.target);
    }

    if args.json {
        print_json(&result, args.top, args.depth);
    } else {
        print_text(&result, args.top, args.depth);
    }
    0
}

/// Runs one path through the normal scanner, trying the Windows MFT fast
/// path first when the mode/path combination calls for it — the same
/// decision `app.rs::start_scan` makes for the GUI, duplicated here rather
/// than shared because that function is tangled up with GUI state
/// (`self.status_message`, `self.cancel`, ...) that doesn't exist here.
fn run_single(
    path: PathBuf,
    mode: Mode,
    threads: usize,
    tx: &crossbeam_channel::Sender<ScanMessage>,
    cancel: &AtomicBool,
    excludes: &[PathBuf],
) {
    #[allow(unused_mut)]
    let mut handled = false;

    #[cfg(windows)]
    {
        // The MFT scan reads the whole volume at once and can't leave a
        // folder out, so excludes force the normal walk.
        if !excludes.is_empty() && matches!(mode, Mode::Auto | Mode::Mft) {
            eprintln!("--exclude can't be applied by the MFT fast scan; using normal scan.");
        } else if matches!(mode, Mode::Auto | Mode::Mft) {
            if let Some(drive) = crate::mft::drive_letter_of_root(&path) {
                match crate::mft::scan_volume(drive, tx, cancel) {
                    Ok(true) => handled = true,
                    Ok(false) => {}
                    Err(e) => eprintln!("Fast NTFS scan unavailable ({e}); using normal scan."),
                }
            } else if mode == Mode::Mft {
                eprintln!("--mode mft only works on a whole drive root (e.g. C:\\); using normal scan.");
            }
        }
    }
    #[cfg(not(windows))]
    let _ = mode;

    if !handled {
        crate::debug_log::log(&format!(
            "using the normal directory walk for {}{}",
            path.display(),
            if cfg!(windows) { "" } else { " (the NTFS fast scan only exists on Windows)" }
        ));
        let _ = scanner::scan_excluding(&path, threads, tx, cancel, excludes);
    }
}

fn run_entire_system(
    mode: Mode,
    threads: usize,
    cancel: &AtomicBool,
    excludes: &[PathBuf],
) -> Option<ScanResult> {
    // A root the user excluded (or that lies inside an excluded folder) is
    // not scanned at all, so `--exclude /mnt/c` also removes /mnt/c as its own
    // root -- not only from the walk of `/`.
    let abs = |p: &PathBuf| p.canonicalize().unwrap_or_else(|_| p.clone());
    let excluded: Vec<PathBuf> = excludes.iter().map(abs).collect();
    let roots: Vec<RootEntry> = scanner::list_local_roots()
        .into_iter()
        .filter(|r| {
            let keep = !excluded.iter().any(|e| abs(&r.path).starts_with(e));
            if !keep {
                crate::debug_log::log(&format!("entire system: root {} is excluded", r.path.display()));
            }
            keep
        })
        .collect();
    if roots.is_empty() {
        eprintln!("No local drives left to scan.");
        return None;
    }

    let mut results = Vec::new();
    for root in &roots {
        eprintln!("  {}...", root.label);
        crate::debug_log::log(&format!("entire system: scanning {} ({})", root.label, root.path.display()));
        let (tx, rx) = crossbeam_channel::unbounded();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| run_single(root.path.clone(), mode, threads, &tx, cancel, excludes));
            drain_for_result(&rx)
        });
        if let Some(r) = result {
            results.push(r);
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(merge_roots("Entire System", results))
    }
}

/// Drains a scan's channel to completion, printing a single, periodically
/// updated progress line to stderr (so it doesn't pollute the actual
/// report on stdout, which is meant to be pipeable/redirectable) and
/// returning the final result, if any.
///
/// Under `--debug` the `\r` progress line is replaced by a normal log line
/// every 2 seconds: a rewritten line and log lines on the same stream would
/// garble each other, and the log file should record how the scan went too.
fn drain_for_result(rx: &crossbeam_channel::Receiver<ScanMessage>) -> Option<ScanResult> {
    let debug = crate::debug_log::is_enabled();
    let started = std::time::Instant::now();
    let mut last_log = std::time::Instant::now();
    for msg in rx.iter() {
        match msg {
            ScanMessage::Progress { files_seen, bytes_seen, current } => {
                if !debug {
                    eprint!("\r  {files_seen} items, {} scanned...   ", format_size(bytes_seen, BINARY));
                } else if last_log.elapsed() >= std::time::Duration::from_secs(2) {
                    let rate = files_seen as f64 / started.elapsed().as_secs_f64().max(0.001);
                    crate::debug_log::log(&format!(
                        "progress: {files_seen} items, {} scanned, {rate:.0} items/s, in {current}",
                        format_size(bytes_seen, BINARY)
                    ));
                    last_log = std::time::Instant::now();
                }
            }
            ScanMessage::Info(s) => {
                if debug {
                    crate::debug_log::log(&format!("info: {s}"));
                } else {
                    eprintln!("\n  {s}");
                }
            }
            ScanMessage::Partial(_) => {} // console mode has no live view to feed
            ScanMessage::Done(r) => {
                if !debug {
                    eprintln!();
                }
                return Some(*r);
            }
            ScanMessage::Failed(e) => {
                if debug {
                    crate::debug_log::log(&format!("scan failed: {e}"));
                } else {
                    eprintln!("\n  {e}");
                }
                return None;
            }
        }
    }
    None
}

/// `--debug` only: what a finished scan looked like, and a sanity check on
/// its size. The check is what pins down "it scanned 128 TiB in a second"
/// reports: a total larger than the volume's used space names the chain of
/// largest folders down to the file responsible.
fn log_result_summary(result: &ScanResult, target: &Target) {
    use crate::debug_log::log;
    let total = result.arena[result.root].size;
    let items = result.file_count + result.dir_count;
    log(&format!(
        "result: {} ({total} bytes), {} files, {} folders, {} errors, {} ({:.0} items/s){}",
        format_size(total, BINARY),
        result.file_count,
        result.dir_count,
        result.error_count,
        scanner::format_duration(result.elapsed_secs),
        items as f64 / result.elapsed_secs.max(0.001),
        if result.used_fast_path { ", MFT fast scan" } else { "" }
    ));

    let mut top = result.arena[result.root].children.clone();
    top.sort_by(|a, b| result.arena[*b].size.cmp(&result.arena[*a].size));
    for (i, &c) in top.iter().take(5).enumerate() {
        let n = &result.arena[c];
        log(&format!(
            "  largest #{}: {:>12}  {}{}",
            i + 1,
            format_size(n.size, BINARY),
            n.name,
            if n.is_dir { "/" } else { "" }
        ));
    }

    // Follow the biggest child at each level down to a file.
    let mut chain = Vec::new();
    let mut idx = result.root;
    while let Some(&next) = result.arena[idx].children.iter().max_by_key(|&&c| result.arena[c].size) {
        let n = &result.arena[next];
        chain.push(format!("{} ({})", n.name, format_size(n.size, BINARY)));
        idx = next;
    }
    if !chain.is_empty() {
        log(&format!("largest path: {}", chain.join(" > ")));
    }

    if let Target::Path(path) = target {
        if let Some((used, _)) = crate::disk_space::disk_used_bytes(path) {
            log(&format!(
                "volume holding {}: {} used",
                path.display(),
                format_size(used, BINARY)
            ));
            if total > used {
                log(&format!(
                    "WARNING: the scanned total ({}) is larger than the used space of its volume ({}). \
                     Usual causes: sparse or virtual files (e.g. /proc/kcore), hard links counted once per \
                     name, or other filesystems mounted below the target. Look at 'largest path' above.",
                    format_size(total, BINARY),
                    format_size(used, BINARY)
                ));
            }
        }
    }
}

fn print_text(result: &ScanResult, top: usize, depth: usize) {
    println!(
        "\n{}  ({} {}, {} {}, {} {}, {}{})\n",
        format_size(result.arena[result.root].size, BINARY),
        result.file_count,
        plural(result.file_count, "file"),
        result.dir_count,
        plural(result.dir_count, "folder"),
        result.error_count,
        plural(result.error_count, "error"),
        scanner::format_duration(result.elapsed_secs),
        if result.used_fast_path { ", MFT fast scan" } else { "" }
    );
    print_text_node(result, result.root, 0, top, depth, "");
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn print_text_node(result: &ScanResult, idx: usize, level: usize, top: usize, depth: usize, prefix: &str) {
    let node = &result.arena[idx];
    if level > 0 {
        let marker = if node.is_dir && !node.name.ends_with('/') && !node.name.ends_with('\\') {
            "/"
        } else {
            ""
        };
        println!("{prefix}{:>12}  {}{}", format_size(node.size, BINARY), node.name, marker);
    }
    if level >= depth || !node.is_dir {
        return;
    }

    let mut children = node.children.clone();
    children.sort_by(|a, b| result.arena[*b].size.cmp(&result.arena[*a].size));
    let shown = children.len().min(top);
    let child_prefix = format!("{prefix}  ");
    for &child in &children[..shown] {
        print_text_node(result, child, level + 1, top, depth, &child_prefix);
    }
    if children.len() > shown {
        println!(
            "{child_prefix}{:>12}  ... and {} more",
            "",
            children.len() - shown
        );
    }
}

fn print_json(result: &ScanResult, top: usize, depth: usize) {
    let value = json_node(result, result.root, 0, top, depth);
    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
}

fn json_node(result: &ScanResult, idx: usize, level: usize, top: usize, depth: usize) -> serde_json::Value {
    let node: &Node = &result.arena[idx];
    let mut obj = serde_json::json!({
        "name": node.name,
        "size_bytes": node.size,
        "size": format_size(node.size, BINARY).to_string(),
        "is_dir": node.is_dir,
    });

    if node.is_dir && level < depth {
        let mut children = node.children.clone();
        children.sort_by(|a, b| result.arena[*b].size.cmp(&result.arena[*a].size));
        let shown = children.len().min(top);
        let child_values: Vec<serde_json::Value> = children[..shown]
            .iter()
            .map(|&c| json_node(result, c, level + 1, top, depth))
            .collect();
        obj["children"] = serde_json::Value::Array(child_values);
        if children.len() > shown {
            obj["children_omitted"] = serde_json::json!(children.len() - shown);
        }
    }

    if level == 0 {
        obj["total_files"] = serde_json::json!(result.file_count);
        obj["total_folders"] = serde_json::json!(result.dir_count);
        obj["errors"] = serde_json::json!(result.error_count);
        obj["elapsed_secs"] = serde_json::json!(result.elapsed_secs);
        obj["used_fast_path"] = serde_json::json!(result.used_fast_path);
    }

    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::Node;

    /// root/ (200) -> a.txt (50), sub/ (150) -> b.txt (100), c.txt (50)
    fn sample_result() -> ScanResult {
        ScanResult {
            arena: vec![
                Node { name: "root".into(), is_dir: true, size: 200, parent: None, children: vec![1, 2], abs_path: None },
                Node { name: "a.txt".into(), is_dir: false, size: 50, parent: Some(0), children: vec![], abs_path: None },
                Node { name: "sub".into(), is_dir: true, size: 150, parent: Some(0), children: vec![3, 4], abs_path: None },
                Node { name: "b.txt".into(), is_dir: false, size: 100, parent: Some(2), children: vec![], abs_path: None },
                Node { name: "c.txt".into(), is_dir: false, size: 50, parent: Some(2), children: vec![], abs_path: None },
            ],
            root: 0,
            root_path: PathBuf::from("/root"),
            file_count: 3,
            dir_count: 2,
            error_count: 0,
            elapsed_secs: 1.5,
            used_fast_path: false,
        }
    }

    #[test]
    fn json_report_reflects_actual_tree_and_respects_limits() {
        let result = sample_result();
        let value = json_node(&result, result.root, 0, 10, 10);

        assert_eq!(value["name"], "root");
        assert_eq!(value["size_bytes"], 200);
        assert_eq!(value["total_files"], 3);
        let children = value["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        // Sorted by size descending: sub/ (150) before a.txt (50).
        assert_eq!(children[0]["name"], "sub");
        assert_eq!(children[0]["children"].as_array().unwrap().len(), 2);
        assert_eq!(children[1]["name"], "a.txt");
        assert!(children[1].get("children").is_none()); // a file has no children key at all
    }

    #[test]
    fn json_report_top_and_depth_limits_are_applied() {
        let result = sample_result();
        // depth=1: root's children show, but sub/'s own children don't.
        let value = json_node(&result, result.root, 0, 10, 1);
        let sub = &value["children"].as_array().unwrap()[0];
        assert_eq!(sub["name"], "sub");
        assert!(sub.get("children").is_none());

        // top=1: only the single largest child kept, rest counted as omitted.
        let value = json_node(&result, result.root, 0, 1, 10);
        assert_eq!(value["children"].as_array().unwrap().len(), 1);
        assert_eq!(value["children_omitted"], 1);
    }
}
