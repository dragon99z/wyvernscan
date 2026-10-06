use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One entry in the size tree. Stored in a flat arena (`Vec<Node>`) and
/// linked by index rather than pointers, so the whole tree is a single
/// contiguous allocation with no Rc/RefCell overhead.
///
/// `name` is `Arc<str>` rather than `String` specifically because the live
/// "Partial" snapshot feature clones the *entire* arena periodically during
/// a scan to hand a point-in-time copy to the GUI thread — on a large
/// drive that can mean cloning millions of `Node`s repeatedly. Cloning a
/// `String` allocates and copies its bytes every time; cloning an
/// `Arc<str>` is one atomic increment. The name is never mutated after
/// creation, so there's no downside to the shared ownership.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: Arc<str>,
    pub is_dir: bool,
    /// Own size for a file; for a directory, the recursive sum of
    /// everything below it. Kept correct at all times via incremental
    /// propagation as each entry is inserted (see `propagate_size_up` in
    /// both this module's `scan()` and `mft.rs`'s `walk_dir()`) rather than
    /// a separate aggregation pass over the whole tree — see that
    /// function's doc comment for why.
    pub size: u64,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// Only set on the synthetic children `merge_roots()` creates when
    /// combining several independent scans (e.g. "Entire System"): the real
    /// absolute path of that root, since `name` there is just a display
    /// label and can't be joined onto `root_path` like a normal path
    /// component. `None` for every ordinary node.
    pub abs_path: Option<PathBuf>,
}

pub struct ScanResult {
    pub arena: Vec<Node>,
    pub root: usize,
    pub root_path: PathBuf,
    pub file_count: u64,
    pub dir_count: u64,
    pub error_count: u64,
    pub elapsed_secs: f64,
    /// True if this came from the Windows NTFS-MFT fast path (mft.rs)
    /// instead of the normal cross-platform directory walk.
    pub used_fast_path: bool,
}

/// Progress updates sent back to the GUI thread while a scan is running.
pub enum ScanMessage {
    Progress {
        files_seen: u64,
        current: String,
        /// Total bytes of file content seen so far (directories don't
        /// count). Compared against a volume's total used space, when
        /// known, to show a real "X of Y scanned" indicator.
        bytes_seen: u64,
    },
    /// A snapshot of the tree as scanned so far, sent periodically so the
    /// treemap/list can fill in live instead of showing a static "scanning"
    /// message. Not a final result — `scanning` stays true when this
    /// arrives. Sizes reflect only what's been walked so far, so
    /// directories will visibly grow between snapshots.
    Partial(Box<ScanResult>),
    /// A short, non-fatal note to show the user without stopping the scan
    /// in progress — e.g. confirming which scan strategy is actually
    /// running.
    Info(String),
    Done(Box<ScanResult>),
    Failed(String),
}

/// Adds `size` to `node_idx` and every one of its ancestors, walking up via
/// `parent` links. Called once per newly inserted leaf (a file's size never
/// changes after creation, so this is the only time it needs to propagate).
///
/// This replaces what used to be a separate full-array reverse pass
/// (`aggregate()`) run periodically over the *entire* tree so far: with
/// millions of entries and a live snapshot taken every second or so during
/// a long scan, those repeated full passes added up to real, unnecessary
/// work. Propagating incrementally instead costs O(tree depth) — typically
/// a few dozen steps — per file, regardless of how large the tree
/// eventually gets, and keeps sizes correct at every instant rather than
/// only after an explicit aggregation step.
#[inline]
pub fn propagate_size_up(arena: &mut [Node], mut node_idx: Option<usize>, size: u64) {
    if size == 0 {
        return;
    }
    while let Some(idx) = node_idx {
        arena[idx].size += size;
        node_idx = arena[idx].parent;
    }
}

/// How long to wait between live "Partial" snapshots, as a function of how
/// big the tree has grown so far. A snapshot clones the *entire* tree so
/// far (sizes are already correct at every instant via incremental
/// propagation, so no re-aggregation pass is needed — just the clone), so
/// on a small folder a fixed short interval feels nice and live; on a
/// multi-million-entry drive, doing that every 400ms regardless of size
/// would mean repeatedly cloning a many-hundred-MB structure just to throw
/// the previous copy away moments later. This scales the interval up as
/// the tree grows so the clone cost stays roughly bounded rather than
/// compounding with scan size.
/// "12.3s" for anything under a minute, "5m 12s" beyond that — a scan of a
/// large, full drive can run long enough that raw seconds (e.g.
/// "1823.40s") stops being a readable number at a glance. Shared by the GUI
/// status bar and the console report.
pub fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = secs.round() as u64;
        format!("{}m {}s", total / 60, total % 60)
    }
}

pub fn partial_interval_for(arena_len: usize) -> Duration {
    const BASE: Duration = Duration::from_millis(400);
    const STEP: Duration = Duration::from_millis(400);
    const PER_STEP: usize = 200_000;
    BASE + STEP * (arena_len / PER_STEP) as u32
}

/// One directory's worth of entries, read on a worker thread. Workers do
/// *all* the syscalls (`read_dir` + per-entry `metadata`) so the single
/// consumer thread in `scan()` only ever touches memory.
struct Batch {
    /// Arena index of the directory these entries belong to.
    parent: usize,
    /// That directory's real path, handed back only for the progress line.
    path: PathBuf,
    /// `(name, size, Some(path) if it's a directory to descend into)`.
    /// The path is kept as a `PathBuf` (not rebuilt from the lossy `name`)
    /// so non-UTF-8 names still resolve to the right directory.
    entries: Vec<(Arc<str>, u64, Option<PathBuf>)>,
    errors: u64,
}

/// Under `--debug`, record exactly which path failed and why — the detail
/// behind the "N errors" count the UI shows.
fn note_error(what: &str, path: &Path, err: &std::io::Error) {
    if crate::debug_log::is_enabled() {
        crate::debug_log::log_item(&format!("scan error: {what}: {} ({err})", path.display()));
    }
}

fn read_batch(parent: usize, path: PathBuf) -> Batch {
    let mut entries = Vec::new();
    let mut errors = 0;
    match std::fs::read_dir(&path) {
        Err(err) => {
            errors += 1;
            note_error("cannot read directory", &path, &err);
        }
        Ok(rd) => {
            for e in rd {
                let e = match e {
                    Ok(e) => e,
                    Err(err) => {
                        errors += 1;
                        note_error("cannot read an entry of directory", &path, &err);
                        continue;
                    }
                };
                let ft = match e.file_type() {
                    Ok(ft) => ft,
                    Err(err) => {
                        errors += 1;
                        note_error("cannot determine entry type", &e.path(), &err);
                        continue;
                    }
                };
                let name: Arc<str> = Arc::from(&*e.file_name().to_string_lossy());
                // `file_type()` doesn't follow symlinks, so links and
                // junctions are leaves, never descended into (no cycles).
                if ft.is_dir() {
                    entries.push((name, 0, Some(e.path())));
                } else {
                    // Free on Windows (the size already came back with the
                    // directory listing); a cheap `fstatat` on Unix.
                    let size = match e.metadata() {
                        Ok(m) => m.len(),
                        Err(err) => {
                            // Counted, so the error total and the debug log
                            // always agree: a file whose size is unknown is
                            // an error, not a silent zero.
                            errors += 1;
                            note_error("cannot read size, counted as 0 bytes", &e.path(), &err);
                            0
                        }
                    };
                    entries.push((name, size, None));
                }
            }
        }
    }
    Batch { parent, path, entries, errors }
}

/// Walk `root` with `threads` workers, each reading whole directories and
/// sending them back as a `Batch`; this thread builds the arena from them.
///
/// Why not `jwalk` (which this used to use): it hands results back through
/// an *unbounded* queue, so its parallel readers ran arbitrarily far ahead of
/// this thread, buffering every unconsumed `DirEntry` in RAM — the cause of
/// the ~20GB blowup on big drives. It also exposes only `stat`-by-path, run
/// serially on the consumer, which dominated scan time. Here the result
/// channel is bounded, so workers block when this thread falls behind and
/// memory stays proportional to the tree, not to how far readers got ahead.
///
/// `cancel` is checked between batches; setting it stops the walk early and
/// still sends whatever was found as a normal `Done`, rather than nothing.
pub fn scan(
    root: &Path,
    threads: usize,
    tx: &crossbeam_channel::Sender<ScanMessage>,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    use crossbeam_channel::RecvTimeoutError;
    use std::sync::atomic::Ordering;

    let start = Instant::now();
    let threads = threads.max(1);
    crate::debug_log::reset_item_budget();

    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_meta = match std::fs::metadata(&root) {
        Ok(m) => m,
        Err(err) => {
            note_error("cannot read scan root", &root, &err);
            let _ = tx.send(ScanMessage::Failed(
                "No accessible entries found (permissions?)".to_string(),
            ));
            return Ok(());
        }
    };

    let root_name: Arc<str> = Arc::from(
        &*root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy(),
    );
    let root_is_dir = root_meta.is_dir();
    let root_size = if root_is_dir { 0 } else { root_meta.len() };

    let mut arena: Vec<Node> = Vec::with_capacity(1 << 16);
    arena.push(Node {
        name: root_name,
        is_dir: root_is_dir,
        size: root_size,
        parent: None,
        children: Vec::new(),
        abs_path: None,
    });

    let mut file_count: u64 = if root_is_dir { 0 } else { 1 };
    let mut dir_count: u64 = if root_is_dir { 1 } else { 0 };
    let mut error_count: u64 = 0;
    let mut seen: u64 = 1;
    let mut bytes_seen: u64 = root_size;
    let mut last_sent = Instant::now();
    let mut last_partial = Instant::now();
    const PROGRESS_INTERVAL: Duration = Duration::from_millis(60);

    // ponytail: FIFO job queue = breadth-first, so the pending-directory
    // frontier (one PathBuf each) can reach a fraction of all dirs on a
    // pathological tree; a work-stealing stack would cut that if it matters.
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<(usize, PathBuf)>();
    let (res_tx, res_rx) = crossbeam_channel::bounded::<Batch>(threads * 4);
    let mut pending = 0usize; // directories queued or in flight
    if root_is_dir {
        let _ = job_tx.send((0, root.clone()));
        pending = 1;
    }

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let job_rx = job_rx.clone();
            let res_tx = res_tx.clone();
            scope.spawn(move || {
                for (parent, path) in job_rx {
                    if res_tx.send(read_batch(parent, path)).is_err() {
                        break; // consumer is gone (cancelled)
                    }
                }
            });
        }
        drop(res_tx); // so a dead worker pool disconnects instead of hanging
        drop(job_rx);

        while pending > 0 {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let batch = match res_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(b) => b,
                Err(RecvTimeoutError::Timeout) => continue, // recheck `cancel`
                Err(RecvTimeoutError::Disconnected) => break,
            };
            pending -= 1;
            error_count += batch.errors;

            // The batch's nodes are contiguous in the arena, so `children`
            // is one exact-size range instead of a doubling-growth Vec, and
            // sizes propagate once per directory, not once per file.
            let first = arena.len();
            let mut total = 0u64;
            for (name, size, dir_path) in batch.entries {
                let idx = arena.len();
                arena.push(Node {
                    name,
                    is_dir: dir_path.is_some(),
                    size,
                    parent: Some(batch.parent),
                    children: Vec::new(),
                    abs_path: None,
                });
                if let Some(p) = dir_path {
                    dir_count += 1;
                    pending += 1;
                    let _ = job_tx.send((idx, p));
                } else {
                    file_count += 1;
                    total += size;
                }
            }
            bytes_seen += total;
            seen += (arena.len() - first) as u64;
            arena[batch.parent].children = (first..arena.len()).collect();
            propagate_size_up(&mut arena, Some(batch.parent), total);

            // Checked before the progress message below so that message
            // (still sitting in the channel) doesn't make this skip itself.
            // A snapshot clones the whole tree, so only take one when the
            // GUI has drained everything sent so far. A GUI that isn't
            // polling (minimized window) otherwise lets full-tree copies
            // queue up in the channel without limit.
            if last_partial.elapsed() >= partial_interval_for(arena.len()) && tx.is_empty() {
                let partial = ScanResult {
                    arena: arena.clone(),
                    root: 0,
                    root_path: root.clone(),
                    file_count,
                    dir_count,
                    error_count,
                    elapsed_secs: start.elapsed().as_secs_f64(),
                    used_fast_path: false,
                };
                let _ = tx.send(ScanMessage::Partial(Box::new(partial)));
                last_partial = Instant::now();
            }

            if last_sent.elapsed() >= PROGRESS_INTERVAL {
                let _ = tx.send(ScanMessage::Progress {
                    files_seen: seen,
                    current: batch.path.to_string_lossy().into_owned(),
                    bytes_seen,
                });
                last_sent = Instant::now();
            }
        }
        // Unblock workers so the scope can join: no more jobs, and a closed
        // result channel makes any worker mid-`send` bail out.
        drop(job_tx);
        drop(res_rx);
    });

    let result = ScanResult {
        arena,
        root: 0,
        root_path: root,
        file_count,
        dir_count,
        error_count,
        elapsed_secs: start.elapsed().as_secs_f64(),
        used_fast_path: false,
    };

    crate::debug_log::log(&format!(
        "scan finished: {} files, {} folders, {} error(s) in {:.1}s",
        result.file_count, result.dir_count, result.error_count, result.elapsed_secs
    ));
    let _ = tx.send(ScanMessage::Done(Box::new(result)));
    Ok(())
}

/// What kind of thing a `RootEntry` points at, used to group entries in the
/// picker (see `app::draw_root_picker`). `Volume` is only ever constructed
/// on macOS/Linux (mounted volumes under `/Volumes`, `/media`, `/mnt`) — on
/// a Windows build every entry is `Drive`, so this variant is legitimately
/// unreachable there. That's expected, not dead code to remove.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootKind {
    Drive,
    Volume,
}

#[derive(Clone, Debug)]
pub struct RootEntry {
    pub label: String,
    pub path: PathBuf,
    pub kind: RootKind,
}

/// Best-effort list of local drives/volumes to offer in the root picker.
/// Deliberately simple: no free-space lookups or drive-type probing (that
/// would mean either a new platform-specific dependency or hand-written
/// FFI, for a purely cosmetic improvement). Mapped network drives on
/// Windows already show up here automatically, since they occupy an
/// ordinary drive letter.
pub fn list_local_roots() -> Vec<RootEntry> {
    let mut out = Vec::new();

    #[cfg(windows)]
    {
        for letter in b'A'..=b'Z' {
            let letter = letter as char;
            let path = PathBuf::from(format!("{letter}:\\"));
            if path.exists() {
                out.push(RootEntry {
                    label: format!("{letter}:\\"),
                    path,
                    kind: RootKind::Drive,
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        out.push(RootEntry {
            label: "Macintosh HD (/)".to_string(),
            path: PathBuf::from("/"),
            kind: RootKind::Drive,
        });
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    out.push(RootEntry { label, path, kind: RootKind::Volume });
                }
            }
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        out.push(RootEntry {
            label: "/ (root)".to_string(),
            path: PathBuf::from("/"),
            kind: RootKind::Drive,
        });
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            const SKIP_FS: &[&str] = &[
                "proc", "sysfs", "tmpfs", "devtmpfs", "cgroup", "cgroup2", "debugfs", "tracefs",
                "securityfs", "pstore", "bpf", "mqueue", "devpts", "autofs", "overlay", "squashfs",
                "fusectl", "configfs", "binfmt_misc", "hugetlbfs", "efivarfs", "rpc_pipefs",
            ];
            for line in mounts.lines() {
                let mut parts = line.split_whitespace();
                let (Some(_dev), Some(mountpoint), Some(fstype)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if SKIP_FS.contains(&fstype) || mountpoint == "/" {
                    continue;
                }
                // Keep this to plausible real removable/user mounts rather
                // than every bind mount and container overlay on the box.
                if mountpoint.starts_with("/media")
                    || mountpoint.starts_with("/mnt")
                    || mountpoint.starts_with("/run/media")
                {
                    out.push(RootEntry {
                        label: mountpoint.to_string(),
                        path: PathBuf::from(mountpoint),
                        kind: RootKind::Volume,
                    });
                }
            }
        }
    }

    out
}

/// Combine several already-scanned trees into one, under a synthetic root
/// node named `label` (e.g. "Entire System"). Each input's own root
/// becomes a direct child, keeping its already-aggregated size, so this is
/// just re-indexing and one cheap sum — not a second full aggregation pass.
pub fn merge_roots(label: &str, results: Vec<ScanResult>) -> ScanResult {
    let mut arena = vec![Node {
        name: label.into(),
        is_dir: true,
        size: 0,
        parent: None,
        children: Vec::new(),
        abs_path: None,
    }];
    const ROOT: usize = 0;

    let mut file_count = 0u64;
    let mut dir_count = 1u64; // the synthetic root itself
    let mut error_count = 0u64;
    let mut elapsed_secs = 0.0f64;
    let mut used_fast_path = false;

    for result in results {
        let offset = arena.len();
        for mut node in result.arena {
            if let Some(p) = node.parent.as_mut() {
                *p += offset;
            }
            for c in node.children.iter_mut() {
                *c += offset;
            }
            arena.push(node);
        }

        let child_root_idx = result.root + offset;
        arena[child_root_idx].parent = Some(ROOT);
        arena[child_root_idx].name = result.root_path.to_string_lossy().into_owned().into();
        arena[child_root_idx].abs_path = Some(result.root_path);
        arena[ROOT].children.push(child_root_idx);

        file_count += result.file_count;
        dir_count += result.dir_count;
        error_count += result.error_count;
        elapsed_secs += result.elapsed_secs;
        used_fast_path |= result.used_fast_path;
    }

    arena[ROOT].size = arena[ROOT].children.iter().map(|&c| arena[c].size).sum();

    ScanResult {
        root_path: PathBuf::from(label),
        root: ROOT,
        arena,
        file_count,
        dir_count,
        error_count,
        elapsed_secs,
        used_fast_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny tree by hand and exercises `propagate_size_up` the same
    /// way `scan()` uses it, to check subtree sums actually come out right:
    ///   root/ (dir)
    ///     a.txt (100 bytes)
    ///     sub/ (dir)
    ///       b.txt (50 bytes)
    #[test]
    fn aggregates_nested_sizes_bottom_up() {
        let mut arena = vec![
            Node { name: "root".into(), is_dir: true, size: 0, parent: None, children: vec![1, 2], abs_path: None },
            Node { name: "a.txt".into(), is_dir: false, size: 100, parent: Some(0), children: vec![], abs_path: None },
            Node { name: "sub".into(), is_dir: true, size: 0, parent: Some(0), children: vec![3], abs_path: None },
            Node { name: "b.txt".into(), is_dir: false, size: 50, parent: Some(2), children: vec![], abs_path: None },
        ];

        // The real code path: each leaf's size is propagated up to every
        // ancestor at insertion time, not recovered by a later full-array
        // pass. Simulates the order things are actually inserted in: a.txt
        // then sub/ then b.txt, each immediately propagated.
        propagate_size_up(&mut arena, Some(0), 100); // a.txt -> root
        propagate_size_up(&mut arena, Some(0), 0); // sub/ itself, size 0 at creation
        propagate_size_up(&mut arena, Some(2), 50); // b.txt -> sub/ -> root

        assert_eq!(arena[3].size, 50); // leaf file, unchanged
        assert_eq!(arena[2].size, 50); // sub/ = b.txt
        assert_eq!(arena[1].size, 100); // leaf file, unchanged
        assert_eq!(arena[0].size, 150); // root = a.txt + sub/
    }

    /// End-to-end check of the real `scan()` function (not just the
    /// aggregation math in isolation): builds a small real directory tree
    /// on disk with known file sizes and checks the reported totals.
    #[test]
    fn scan_reports_correct_total_on_real_files() {
        let dir = std::env::temp_dir().join(format!("wyvernscan_test_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(sub.join("b.txt"), vec![0u8; 50]).unwrap();

        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        scan(&dir, 2, &tx, &cancel).unwrap();

        let mut result = None;
        while let Ok(msg) = rx.try_recv() {
            if let ScanMessage::Done(r) = msg {
                result = Some(*r);
            }
        }
        let result = result.expect("scan should produce a result for a real, readable directory");

        assert_eq!(result.file_count, 2);
        assert_eq!(result.dir_count, 2); // root dir itself + sub/
        assert_eq!(result.arena[result.root].size, 150);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Parent tracking check: each directory's entries must be attributed to
    /// *that* directory (the parent index travels with the job), including
    /// when a deeply nested subtree sits next to a shallower sibling. A
    /// wrong parent would leak one subtree's size into the other.
    ///
    ///   root/
    ///     a.txt        (100)
    ///     sub1/
    ///       b.txt      (50)
    ///       subsub/
    ///         c.txt    (10)
    ///     sub2/
    ///       d.txt      (20)
    #[test]
    fn scan_handles_sibling_directories_and_nested_depth_correctly() {
        let dir = std::env::temp_dir().join(format!("wyvernscan_depth_test_{}", std::process::id()));
        let sub1 = dir.join("sub1");
        let subsub = sub1.join("subsub");
        let sub2 = dir.join("sub2");
        std::fs::create_dir_all(&subsub).unwrap();
        std::fs::create_dir_all(&sub2).unwrap();
        std::fs::write(dir.join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(sub1.join("b.txt"), vec![0u8; 50]).unwrap();
        std::fs::write(subsub.join("c.txt"), vec![0u8; 10]).unwrap();
        std::fs::write(sub2.join("d.txt"), vec![0u8; 20]).unwrap();

        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        // Single thread: removes parallelism as a variable, so this test is
        // purely about parent attribution.
        scan(&dir, 1, &tx, &cancel).unwrap();

        let mut result = None;
        while let Ok(msg) = rx.try_recv() {
            if let ScanMessage::Done(r) = msg {
                result = Some(*r);
            }
        }
        let result = result.expect("scan should produce a result");

        assert_eq!(result.file_count, 4);
        assert_eq!(result.arena[result.root].size, 180); // 100+50+10+20

        // Find sub2 by name and confirm it got exactly its own file's size
        // (20), not somehow inheriting subsub's 10 via a stale ancestor
        // entry left over from sub1's subtree.
        let root_node = &result.arena[result.root];
        let sub2_idx = root_node
            .children
            .iter()
            .find(|&&i| &*result.arena[i].name == "sub2")
            .copied()
            .expect("sub2 should exist");
        assert_eq!(result.arena[sub2_idx].size, 20);

        let sub1_idx = root_node
            .children
            .iter()
            .find(|&&i| &*result.arena[i].name == "sub1")
            .copied()
            .expect("sub1 should exist");
        assert_eq!(result.arena[sub1_idx].size, 60); // 50 + subsub's 10

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cancel flag already set before `scan()` starts should stop the
    /// walk essentially immediately, rather than it walking the whole tree
    /// regardless. Uses a directory with enough entries that "walked
    /// everything anyway" and "stopped almost immediately" would produce
    /// clearly different file counts.
    #[test]
    fn cancel_flag_stops_scan_early() {
        let dir = std::env::temp_dir().join(format!("wyvernscan_cancel_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..200 {
            std::fs::write(dir.join(format!("file_{i}.txt")), b"x").unwrap();
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = std::sync::atomic::AtomicBool::new(true); // already cancelled
        scan(&dir, 2, &tx, &cancel).unwrap();

        let mut done_file_count = None;
        let mut failed = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ScanMessage::Done(r) => done_file_count = Some(r.file_count),
                ScanMessage::Failed(_) => failed = true,
                _ => {}
            }
        }

        // Either outcome is acceptable proof of an early stop: it gave up
        // entirely (arena stayed empty -> Failed), or it produced a Done
        // with far fewer than the 200 files actually on disk.
        let stopped_early = failed || done_file_count.map(|n| n < 200).unwrap_or(false);
        assert!(stopped_early, "cancel flag did not stop the scan early");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Cancelling *during* a scan, while workers are likely blocked sending
    /// on the bounded result channel, must still return. A regression here
    /// (e.g. forgetting to close the channels before the thread scope joins)
    /// shows up as this test hanging forever rather than failing.
    #[test]
    fn cancel_mid_scan_does_not_deadlock() {
        let dir = std::env::temp_dir().join(format!("wyvernscan_midcancel_{}", std::process::id()));
        for d in 0..300 {
            let sub = dir.join(format!("d{d}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..5 {
                std::fs::write(sub.join(format!("f{f}")), b"x").unwrap();
            }
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(1));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            scan(&dir, 4, &tx, &cancel).unwrap();
        });
        assert!(rx.try_iter().any(|m| matches!(m, ScanMessage::Done(_))));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Not run as part of normal `cargo test` (too slow to run every time) —
    /// a manual, real-world-scale sanity check for the memory fix in this
    /// module: builds a synthetic tree with a large number of small files
    /// spread across many directories, runs the real `scan()` against it,
    /// and reports peak RSS. Run explicitly with:
    ///   cargo test --release scan_memory_stays_bounded_on_large_tree -- --ignored --nocapture
    #[test]
    #[ignore]
    fn scan_memory_stays_bounded_on_large_tree() {
        let dir = std::env::temp_dir().join(format!("wyvernscan_stress_{}", std::process::id()));
        let dirs = 500;
        let files_per_dir = 200; // 100,000 files total
        for d in 0..dirs {
            let sub = dir.join(format!("dir_{d:04}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..files_per_dir {
                std::fs::write(sub.join(format!("file_{f:04}.txt")), b"x").unwrap();
            }
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        scan(&dir, 4, &tx, &cancel).unwrap();

        let mut result = None;
        while let Ok(msg) = rx.try_recv() {
            if let ScanMessage::Done(r) = msg {
                result = Some(*r);
            }
        }
        let result = result.expect("scan should complete");
        println!(
            "scanned {} files in {} dirs, peak RSS: {}",
            result.file_count,
            result.dir_count,
            peak_rss_human()
        );
        assert_eq!(result.file_count, dirs * files_per_dir);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Best-effort peak resident set size, Linux only (reads
    /// `/proc/self/status`), purely for the stress test above to print
    /// something human-readable — not used anywhere in the real app.
    #[cfg(target_os = "linux")]
    fn peak_rss_human() -> String {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmHWM:"))
                    .map(|l| l.trim().to_string())
            })
            .unwrap_or_else(|| "unknown (couldn't read /proc/self/status)".to_string())
    }
    #[cfg(not(target_os = "linux"))]
    fn peak_rss_human() -> String {
        "unknown (peak RSS reporting only implemented for Linux)".to_string()
    }

    /// Checks the reindexing math in `merge_roots`: two independent
    /// single-node "scans" should end up as siblings under a synthetic
    /// root, with correct offsets and a summed total size.
    #[test]
    fn merge_roots_reindexes_and_sums_correctly() {
        let a = ScanResult {
            arena: vec![Node {
                name: "a".into(),
                is_dir: true,
                size: 100,
                parent: None,
                children: vec![],
                abs_path: None,
            }],
            root: 0,
            root_path: PathBuf::from("/a"),
            file_count: 1,
            dir_count: 1,
            error_count: 0,
            elapsed_secs: 1.0,
            used_fast_path: false,
        };
        let b = ScanResult {
            arena: vec![Node {
                name: "b".into(),
                is_dir: true,
                size: 200,
                parent: None,
                children: vec![],
                abs_path: None,
            }],
            root: 0,
            root_path: PathBuf::from("/b"),
            file_count: 2,
            dir_count: 1,
            error_count: 0,
            elapsed_secs: 1.0,
            used_fast_path: true,
        };

        let merged = merge_roots("Entire System", vec![a, b]);

        assert_eq!(merged.arena[merged.root].size, 300);
        assert_eq!(merged.arena[merged.root].children.len(), 2);
        assert_eq!(merged.file_count, 3);
        assert_eq!(merged.dir_count, 3); // synthetic root + a + b
        assert!(merged.used_fast_path); // true if *any* sub-scan used it

        for &child in &merged.arena[merged.root].children {
            assert_eq!(merged.arena[child].parent, Some(merged.root));
            assert!(merged.arena[child].abs_path.is_some());
        }
    }
}
