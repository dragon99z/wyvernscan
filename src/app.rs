use crate::scanner::{
    exclusions_shrink_total, list_local_roots, merge_roots, scan_excluding, split_excluded_roots,
    Node, RootEntry, ScanMessage, ScanResult,
};
use crate::theme;
use eframe::egui;
use egui::{Color32, RichText, Ui};
use humansize::{format_size, BINARY};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The flattened, visible rows of the tree-style list view (node index and
/// nesting depth), plus the inputs they were built from. Rebuilding means
/// re-sorting every expanded directory's children, which for one huge
/// expanded folder is too much to redo on every repaint while scrolling —
/// so it's only redone when the key actually changes.
struct TreeCache {
    key: (usize, u64, usize, SortBy, u64),
    rows: Vec<(usize, u32)>,
}

/// The label of the synthetic root `merge_roots` creates for an "Entire
/// System" scan. Also doubles as `root_path` in that case, so anything that
/// wants to *re-run* the current scan must check for it rather than treat
/// it as a real filesystem path.
const ENTIRE_SYSTEM_LABEL: &str = "Entire System";

#[derive(PartialEq, Clone, Copy)]
enum ViewMode {
    List,
    Treemap,
}

#[derive(PartialEq, Clone, Copy)]
enum SortBy {
    Size,
    Name,
}

/// Which scanning strategy to use. The Windows-only fast path still exists
/// as a variant on every platform (simpler than cfg-gating the enum
/// itself) — it's just never offered in the UI, and never attempted, on
/// anything other than Windows.
#[derive(PartialEq, Eq, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
enum ScanMode {
    Auto,
    Normal,
    MftFast,
}

impl Default for ScanMode {
    fn default() -> Self {
        ScanMode::Auto
    }
}

impl ScanMode {
    fn label(self) -> &'static str {
        match self {
            ScanMode::Auto => "Auto (fastest available)",
            ScanMode::Normal => "Normal (all platforms)",
            ScanMode::MftFast => "MFT fast (Windows, Admin)",
        }
    }
}

/// The small slice of state worth remembering between runs.
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Persisted {
    recent_folders: Vec<PathBuf>,
    scan_mode: ScanMode,
    /// `default` so settings saved by an older version (without this field)
    /// still load instead of discarding the recent folders too.
    #[serde(default)]
    excluded_folders: Vec<PathBuf>,
}

pub struct WyvernScanApp {
    root_path: Option<PathBuf>,
    arena: Vec<Node>,
    root_idx: usize,
    current_idx: usize,
    view_mode: ViewMode,
    sort_by: SortBy,

    scan_mode: ScanMode,
    available_roots: Vec<RootEntry>,
    recent_folders: Vec<PathBuf>,
    show_custom_path_input: bool,
    custom_path_buf: String,
    /// Folders every scan leaves out (the GUI twin of `--exclude`). Applied
    /// when a scan starts, so edits take effect on the next scan / Rescan.
    excluded_folders: Vec<PathBuf>,
    show_excludes: bool,
    exclude_input_buf: String,

    scanning: bool,
    files_seen: u64,
    /// Total bytes of file content seen so far in the running scan.
    bytes_seen: u64,
    /// What `bytes_seen` is measured against for a real "X of Y" progress
    /// bar: the volume's used space, but only when the scan target is a
    /// whole drive/volume root (see `disk_space.rs` for why a subfolder
    /// scan gets `None` instead of a misleading whole-drive number).
    expected_total_bytes: Option<u64>,
    current_scanning_path: String,
    scan_rx: Option<crossbeam_channel::Receiver<ScanMessage>>,
    /// Shared with the background scan thread(s); setting this asks them to
    /// stop early and still hand back whatever was found so far.
    cancel: Option<Arc<AtomicBool>>,

    /// Which directory nodes are currently expanded in the tree-style list
    /// view. Indices are only meaningful within one scan's arena, so this
    /// is cleared whenever a new scan starts (stale indices from a previous
    /// scan would otherwise silently match unrelated nodes in the new one).
    expanded: HashSet<usize>,
    /// Bumped whenever `arena` is replaced or `expanded` changes; part of
    /// the tree-row cache key so cached rows are rebuilt exactly when the
    /// inputs they were derived from could have changed.
    arena_gen: u64,
    expanded_gen: u64,
    tree_cache: Option<TreeCache>,

    file_count: u64,
    dir_count: u64,
    error_count: u64,
    elapsed_secs: f64,
    used_fast_path: bool,

    pending_delete: Option<usize>,
    status_message: Option<String>,
    /// Whether this process is running as Administrator, passed in from
    /// main() at startup (detection lives in the Windows-only
    /// win_integration module; always false elsewhere). Drives both the
    /// "(Admin)" window title and whether "Restart as Admin" is shown.
    elevated: bool,
    /// The brand logo, uploaded once at startup. `None` only in `Default`
    /// (tests, headless construction) where there is no egui context.
    logo: Option<egui::TextureHandle>,
}

impl Default for WyvernScanApp {
    fn default() -> Self {
        Self {
            root_path: None,
            arena: Vec::new(),
            root_idx: 0,
            current_idx: 0,
            view_mode: ViewMode::Treemap,
            sort_by: SortBy::Size,
            scan_mode: ScanMode::default(),
            available_roots: Vec::new(),
            recent_folders: Vec::new(),
            show_custom_path_input: false,
            custom_path_buf: String::new(),
            excluded_folders: Vec::new(),
            show_excludes: false,
            exclude_input_buf: String::new(),
            scanning: false,
            files_seen: 0,
            bytes_seen: 0,
            expected_total_bytes: None,
            current_scanning_path: String::new(),
            scan_rx: None,
            cancel: None,
            expanded: HashSet::new(),
            arena_gen: 0,
            expanded_gen: 0,
            tree_cache: None,
            file_count: 0,
            dir_count: 0,
            error_count: 0,
            elapsed_secs: 0.0,
            used_fast_path: false,
            pending_delete: None,
            status_message: None,
            elevated: false,
            logo: None,
        }
    }
}

impl WyvernScanApp {
    pub fn new(cc: &eframe::CreationContext<'_>, elevated: bool) -> Self {
        let mut app = Self {
            available_roots: list_local_roots(),
            elevated,
            logo: Some(load_logo(&cc.egui_ctx)),
            ..Self::default()
        };
        if let Some(storage) = cc.storage {
            if let Some(persisted) = eframe::get_value::<Persisted>(storage, eframe::APP_KEY) {
                app.recent_folders = persisted.recent_folders;
                app.scan_mode = persisted.scan_mode;
                app.excluded_folders = persisted.excluded_folders;
            }
        }
        app
    }

    fn remember_recent(&mut self, path: &Path) {
        self.recent_folders.retain(|p| p != path);
        self.recent_folders.insert(0, path.to_path_buf());
        self.recent_folders.truncate(3);
    }

    /// Clears everything tied to a previous scan's tree at the moment a new
    /// scan begins. The old arena's indices mean nothing in the new one, so
    /// leaving it (or `expanded`, which is keyed by those indices) around
    /// would show stale data with a mismatched status bar until fresh
    /// results arrive.
    fn reset_for_new_scan(&mut self) {
        self.arena.clear();
        self.arena_gen += 1;
        self.root_idx = 0;
        self.current_idx = 0;
        self.expanded.clear();
        self.expanded_gen += 1;
        self.scanning = true;
        self.files_seen = 0;
        self.bytes_seen = 0;
        self.expected_total_bytes = None;
        self.current_scanning_path.clear();
        self.status_message = None;
    }

    /// Try the requested scan for a single root, honoring `self.scan_mode`:
    /// `Normal` always uses the cross-platform walker; `Auto`/`MftFast` try
    /// the Windows MFT fast path first when the target is a whole drive
    /// root, falling back to the normal walker otherwise.
    fn start_scan(&mut self, path: PathBuf) {
        self.remember_recent(&path);
        let (tx, rx) = crossbeam_channel::unbounded();
        self.scan_rx = Some(rx);
        self.reset_for_new_scan();
        self.root_path = Some(path.clone());
        // Only a whole drive/volume root gets a "of Y" total: the volume's
        // used space is a meaningful target for that, and a misleading one
        // for anything narrower (see disk_space.rs).
        // ...and not when an exclusion removes part of that volume: the bar
        // would stall short of 100%.
        let excludes = self.excluded_folders.clone();
        if self.available_roots.iter().any(|r| r.path == path)
            && !exclusions_shrink_total(&[path.clone()], &[], &excludes)
        {
            self.expected_total_bytes = crate::disk_space::disk_used_bytes(&path).map(|(used, _)| used);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let threads = num_cpus::get();
        let mode = self.scan_mode;

        std::thread::spawn(move || {
            #[allow(unused_mut)]
            let mut handled = false;

            #[cfg(windows)]
            {
                if !excludes.is_empty() && matches!(mode, ScanMode::Auto | ScanMode::MftFast) {
                    // The MFT scan reads the whole volume at once and can't
                    // leave a folder out.
                    crate::debug_log::log(
                        "exclusions are set: the MFT fast scan can't skip folders, using the normal scan",
                    );
                } else if matches!(mode, ScanMode::Auto | ScanMode::MftFast) {
                    match crate::mft::drive_letter_of_root(&path) {
                        Some(drive) => match crate::mft::scan_volume(drive, &tx, &cancel) {
                            Ok(true) => handled = true,
                            Ok(false) => {
                                // scan_volume already sent its own Info message.
                            }
                            Err(e) => {
                                crate::debug_log::log(&format!(
                                    "MFT scan_volume failed to start on drive {drive}: {e}"
                                ));
                                let _ = tx.send(ScanMessage::Info("Using normal scan.".to_string()));
                            }
                        },
                        None => {
                            if mode == ScanMode::MftFast {
                                crate::debug_log::log(&format!(
                                    "MFT fast mode requested but {} is not a whole drive root.",
                                    path.display()
                                ));
                                let _ = tx.send(ScanMessage::Info("Using normal scan.".to_string()));
                            }
                        }
                    }
                }
            }
            #[cfg(not(windows))]
            let _ = mode; // only meaningful on Windows; avoid an unused-variable warning elsewhere

            if !handled {
                let _ = scan_excluding(&path, threads, &tx, &cancel, &excludes);
            }
        });
    }

    /// Scan every detected local drive/volume and merge them under one
    /// synthetic "Entire System" root. Each drive runs on its own thread so
    /// this thread can relay progress messages live as they arrive, rather
    /// than only after each drive finishes.
    fn start_scan_entire_system(&mut self) {
        let (tx, rx) = crossbeam_channel::unbounded();
        self.scan_rx = Some(rx);
        self.reset_for_new_scan();
        self.root_path = Some(PathBuf::from(ENTIRE_SYSTEM_LABEL));
        // Sum of every drive's used space, so the progress indicator has a
        // meaningful whole-system denominator. If any single drive can't be
        // queried, skip the total entirely rather than show a denominator
        // that's silently too small.
        //
        // Roots inside an excluded folder are dropped entirely (so excluding
        // /mnt/c also stops it being scanned as its own root).
        let excludes = self.excluded_folders.clone();
        let (roots, dropped) = split_excluded_roots(self.available_roots.clone(), &excludes);
        let paths = |v: &[RootEntry]| v.iter().map(|r| r.path.clone()).collect::<Vec<PathBuf>>();
        let totals: Option<Vec<u64>> = roots
            .iter()
            .map(|r| crate::disk_space::disk_used_bytes(&r.path).map(|(used, _)| used))
            .collect();
        self.expected_total_bytes = totals
            .map(|v| v.iter().sum())
            .filter(|t| *t > 0)
            .filter(|_| !exclusions_shrink_total(&paths(&roots), &paths(&dropped), &excludes));
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let threads = num_cpus::get();
        let mode = self.scan_mode;

        std::thread::spawn(move || {
            if roots.is_empty() {
                let _ = tx.send(ScanMessage::Failed("No local drives left to scan.".to_string()));
                return;
            }

            let mut results = Vec::new();
            let mut base: u64 = 0;
            let mut base_bytes: u64 = 0;

            for root in &roots {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let _ = tx.send(ScanMessage::Info(format!("Scanning {}…", root.label)));

                let (sub_tx, sub_rx) = crossbeam_channel::unbounded();
                let root_path = root.path.clone();
                let sub_cancel = cancel.clone();
                let sub_excludes = excludes.clone();
                let handle = std::thread::spawn(move || {
                    #[allow(unused_mut)]
                    let mut handled = false;
                    #[cfg(windows)]
                    {
                        if sub_excludes.is_empty() && matches!(mode, ScanMode::Auto | ScanMode::MftFast) {
                            if let Some(drive) = crate::mft::drive_letter_of_root(&root_path) {
                                match crate::mft::scan_volume(drive, &sub_tx, &sub_cancel) {
                                    Ok(true) => handled = true,
                                    Ok(false) => {}
                                    Err(e) => {
                                        crate::debug_log::log(&format!(
                                            "MFT scan_volume failed to start on drive {drive} \
                                             during Entire System scan: {e}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    #[cfg(not(windows))]
                    let _ = mode;

                    if !handled {
                        let _ = scan_excluding(&root_path, threads, &sub_tx, &sub_cancel, &sub_excludes);
                    }
                });

                for msg in sub_rx.iter() {
                    match msg {
                        ScanMessage::Progress { files_seen, current, bytes_seen } => {
                            let _ = tx.send(ScanMessage::Progress {
                                files_seen: base + files_seen,
                                current,
                                bytes_seen: base_bytes + bytes_seen,
                            });
                        }
                        ScanMessage::Info(s) => {
                            let _ = tx.send(ScanMessage::Info(s));
                        }
                        ScanMessage::Partial(_) => {
                            // Skipped for the multi-drive case: merging a
                            // still-growing per-drive tree into the overall
                            // synthetic root on every snapshot isn't worth
                            // the complexity here. The per-file progress
                            // line above still updates live either way.
                        }
                        ScanMessage::Done(r) => {
                            base += r.file_count + r.dir_count;
                            base_bytes += r.arena[r.root].size;
                            results.push(*r);
                        }
                        ScanMessage::Failed(_) => {}
                    }
                }
                let _ = handle.join();
            }

            if results.is_empty() {
                let _ = tx.send(ScanMessage::Failed("Could not read any local drive.".to_string()));
                return;
            }

            let merged = merge_roots(ENTIRE_SYSTEM_LABEL, results);
            let _ = tx.send(ScanMessage::Done(Box::new(merged)));
        });
    }

    /// Relaunches the app elevated (triggers the UAC prompt) and, if that
    /// succeeds, closes this non-elevated instance so the user isn't left
    /// with two copies running.
    #[cfg(windows)]
    fn restart_as_admin(&mut self, ctx: &egui::Context) {
        let carry_debug = std::env::args().any(|a| a == "--debug");
        match crate::win_integration::restart_as_admin(carry_debug) {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => {
                crate::debug_log::log(&format!("Restart as Admin failed: {e}"));
                self.status_message = Some("Could not restart as Administrator.".to_string());
            }
        }
    }
    #[cfg(not(windows))]
    fn restart_as_admin(&mut self, _ctx: &egui::Context) {}

    /// Re-runs whatever scan produced the current results. An "Entire
    /// System" result's `root_path` is just its display label, not a real
    /// path, so it has to be dispatched to the multi-drive scan instead of
    /// being handed to the single-path scanner as if it were a folder.
    fn rescan(&mut self) {
        if let Some(p) = self.root_path.clone() {
            if p == Path::new(ENTIRE_SYSTEM_LABEL) {
                self.start_scan_entire_system();
            } else {
                self.start_scan(p);
            }
        }
    }

    /// Ask the running scan to stop early. It still hands back whatever was
    /// found so far as a normal result, rather than nothing.
    fn cancel_scan(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.status_message = Some("Cancelling…".to_string());
    }

    fn apply_scan_result(&mut self, result: ScanResult) {
        self.arena = result.arena;
        self.arena_gen += 1;
        self.root_idx = result.root;
        self.current_idx = result.root;
        self.root_path = Some(result.root_path);
        self.file_count = result.file_count;
        self.dir_count = result.dir_count;
        self.error_count = result.error_count;
        self.elapsed_secs = result.elapsed_secs;
        self.used_fast_path = result.used_fast_path;
        self.scanning = false;
        self.cancel = None;
    }

    /// Like `apply_scan_result`, but for a still-in-progress snapshot:
    /// swaps in the bigger tree so the view keeps growing, without ending
    /// the scan or resetting where the user is looking. Existing arena
    /// indices stay valid across this swap — both scanners only ever
    /// append nodes, never reorder or remove them — so `current_idx`
    /// doesn't need to move even if the user is browsing mid-scan.
    fn apply_partial_result(&mut self, result: ScanResult) {
        self.arena = result.arena;
        self.arena_gen += 1;
        self.root_idx = result.root;
        if self.root_path.is_none() {
            self.root_path = Some(result.root_path);
        }
        self.file_count = result.file_count;
        self.dir_count = result.dir_count;
        self.error_count = result.error_count;
        self.elapsed_secs = result.elapsed_secs;
        self.used_fast_path = result.used_fast_path;
    }

    fn poll_scan(&mut self) {
        if let Some(rx) = &self.scan_rx {
            let mut result = None;
            let mut partial = None;
            let mut failed = None;
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ScanMessage::Progress { files_seen, current, bytes_seen } => {
                        self.files_seen = files_seen;
                        self.bytes_seen = bytes_seen;
                        self.current_scanning_path = current;
                    }
                    ScanMessage::Partial(r) => partial = Some(*r),
                    ScanMessage::Info(s) => self.status_message = Some(s),
                    ScanMessage::Done(r) => result = Some(*r),
                    ScanMessage::Failed(e) => failed = Some(e),
                }
            }
            // A final Done supersedes any partial snapshot from this batch.
            if let Some(p) = partial {
                if result.is_none() {
                    self.apply_partial_result(p);
                }
            }
            if let Some(r) = result {
                self.apply_scan_result(r);
            }
            if let Some(e) = failed {
                self.scanning = false;
                self.cancel = None;
                self.status_message = Some(e);
            }
        }
    }

    fn breadcrumbs(&self) -> Vec<usize> {
        let mut v = Vec::new();
        let mut cur = Some(self.current_idx);
        while let Some(i) = cur {
            v.push(i);
            cur = self.arena[i].parent;
        }
        v.reverse();
        v
    }

    fn sorted_children(&self, idx: usize) -> Vec<usize> {
        let mut kids = self.arena[idx].children.clone();
        match self.sort_by {
            SortBy::Size => kids.sort_by(|a, b| self.arena[*b].size.cmp(&self.arena[*a].size)),
            SortBy::Name => kids.sort_by(|a, b| self.arena[*a].name.cmp(&self.arena[*b].name)),
        }
        kids
    }

}

impl eframe::App for WyvernScanApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let persisted = Persisted {
            recent_folders: self.recent_folders.clone(),
            scan_mode: self.scan_mode,
            excluded_folders: self.excluded_folders.clone(),
        };
        eframe::set_value(storage, eframe::APP_KEY, &persisted);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.scanning {
            self.poll_scan();
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if let Some(logo) = &self.logo {
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(
                        logo.id(),
                        egui::vec2(24.0, 24.0),
                    )));
                }
                ui.label(RichText::new("WyvernScan").strong().color(theme::colors::ACCENT));
                ui.separator();
                ui.label(RichText::new("Location").color(theme::colors::MUTED).small());
                ui.add_enabled_ui(!self.scanning, |ui| {
                    self.draw_root_picker(ui);
                });

                ui.add_space(12.0);
                ui.label(RichText::new("Scan mode").color(theme::colors::MUTED).small());
                egui::ComboBox::from_id_source("scan_mode")
                    .selected_text(self.scan_mode.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.scan_mode, ScanMode::Auto, ScanMode::Auto.label());
                        ui.selectable_value(&mut self.scan_mode, ScanMode::Normal, ScanMode::Normal.label());
                        if cfg!(windows) {
                            ui.selectable_value(
                                &mut self.scan_mode,
                                ScanMode::MftFast,
                                ScanMode::MftFast.label(),
                            );
                        }
                    });

                if !self.scanning && self.root_path.is_some() {
                    if ui.button("Rescan").clicked() {
                        self.rescan();
                    }
                }

                let exclude_label = if self.excluded_folders.is_empty() {
                    "Exclude…".to_string()
                } else {
                    format!("Excluded ({})", self.excluded_folders.len())
                };
                if ui
                    .selectable_label(self.show_excludes, exclude_label)
                    .on_hover_text("Folders to leave out of every scan")
                    .clicked()
                {
                    self.show_excludes = !self.show_excludes;
                }

                ui.separator();
                ui.selectable_value(&mut self.view_mode, ViewMode::Treemap, "Treemap");
                ui.selectable_value(&mut self.view_mode, ViewMode::List, "List");
                ui.separator();
                ui.label(RichText::new("Sort").color(theme::colors::MUTED).small());
                ui.selectable_value(&mut self.sort_by, SortBy::Size, "Size");
                ui.selectable_value(&mut self.sort_by, SortBy::Name, "Name");

                if self.scanning {
                    ui.separator();
                    ui.spinner();
                    ui.label(RichText::new(format!("{} items", self.files_seen)).color(theme::colors::MUTED));
                    if ui.button("Cancel").clicked() {
                        self.cancel_scan();
                    }
                }

                if cfg!(windows) && !self.elevated {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(RichText::new("Restart as Admin").color(theme::colors::ACCENT))
                            .on_hover_text("Needed for the MFT fast scan mode on most drives.")
                            .clicked()
                        {
                            self.restart_as_admin(ctx);
                        }
                    });
                }
            });

            if self.show_custom_path_input {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Path").color(theme::colors::MUTED).small());
                    ui.text_edit_singleline(&mut self.custom_path_buf);
                    if ui.button("Go").clicked() {
                        let candidate = PathBuf::from(self.custom_path_buf.trim());
                        if candidate.exists() {
                            self.show_custom_path_input = false;
                            self.start_scan(candidate);
                        } else {
                            self.status_message =
                                Some(format!("Path not found: {}", self.custom_path_buf));
                        }
                    }
                });
            }

            if self.show_excludes {
                self.draw_excludes_panel(ui);
            }

            if let Some(msg) = &self.status_message {
                ui.colored_label(theme::colors::ACCENT, msg);
            }
            ui.add_space(4.0);
        });

        if !self.arena.is_empty() {
            egui::TopBottomPanel::top("breadcrumbs").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let crumbs = self.breadcrumbs();
                    let last = crumbs.len().saturating_sub(1);
                    for (n, idx) in crumbs.iter().enumerate() {
                        if n > 0 {
                            ui.label(RichText::new("›").color(theme::colors::MUTED));
                        }
                        let label = if self.arena[*idx].name.is_empty() {
                            "/".to_string()
                        } else {
                            self.arena[*idx].name.to_string()
                        };
                        let text = if n == last {
                            RichText::new(label).color(theme::colors::ACCENT).strong()
                        } else {
                            RichText::new(label)
                        };
                        if ui.link(text).clicked() {
                            self.current_idx = *idx;
                        }
                    }
                });
            });

            egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                ui.add_space(2.0);
                if self.scanning {
                    self.draw_scan_progress(ui, ui.available_width().min(520.0));
                }
                ui.horizontal(|ui| {
                    if self.scanning {
                        ui.spinner();
                        ui.label(
                            RichText::new(if self.current_scanning_path.is_empty() {
                                "Scanning…".to_string()
                            } else {
                                self.current_scanning_path.clone()
                            })
                            .monospace()
                            .color(theme::colors::MUTED),
                        );
                    } else {
                        ui.label(
                            RichText::new(format!("Done in {}", crate::scanner::format_duration(self.elapsed_secs)))
                                .strong()
                                .color(theme::colors::ACCENT),
                        );
                        ui.label(
                            RichText::new(format!(
                                "— {} files, {} folders, {} errors{}",
                                self.file_count,
                                self.dir_count,
                                self.error_count,
                                if self.used_fast_path { " · MFT fast scan" } else { "" }
                            ))
                            .color(theme::colors::MUTED),
                        );
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(format!("Total: {}", format_size(self.arena[self.root_idx].size, BINARY)))
                            .strong()
                            // Explicit: `.strong()` alone takes the "active widget" foreground,
                            // which this theme sets dark, so it vanished on the status bar.
                            .color(theme::colors::TEXT),
                    );
                });
                ui.add_space(2.0);
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.arena.is_empty() {
                if self.scanning {
                    self.draw_scanning_overlay(ui);
                } else {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.28).max(24.0));
                        if let Some(logo) = &self.logo {
                            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                logo.id(),
                                egui::vec2(96.0, 96.0),
                            )));
                        }
                        ui.add_space(8.0);
                        ui.label(RichText::new("WyvernScan").size(26.0).strong().color(theme::colors::ACCENT));
                        ui.label(RichText::new("See what's eating your disk.").color(theme::colors::MUTED));
                        ui.add_space(14.0);
                        ui.label(RichText::new("Choose a location above to scan.").color(theme::colors::MUTED));
                    });
                }
                return;
            }
            match self.view_mode {
                ViewMode::List => self.draw_list(ui),
                ViewMode::Treemap => self.draw_treemap(ui),
            }
        });

        self.draw_delete_confirm(ctx);
    }
}

impl WyvernScanApp {
    /// Adds a folder to the exclusion list. Takes effect on the next scan,
    /// which the status line says, since the current tree still contains it.
    fn add_exclude(&mut self, path: PathBuf) {
        if path.as_os_str().is_empty() {
            return;
        }
        if !path.exists() {
            self.status_message = Some(format!("Path not found: {}", path.display()));
            return;
        }
        if self.excluded_folders.contains(&path) {
            self.status_message = Some(format!("Already excluded: {}", path.display()));
            return;
        }
        self.status_message = Some(format!(
            "Excluded {} — press Rescan to apply.",
            path.display()
        ));
        self.excluded_folders.push(path);
    }

    /// The editable list of excluded folders (shown under the toolbar).
    fn draw_excludes_panel(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Excluded folders — left out of every scan; changes apply on the next scan or Rescan")
                .color(theme::colors::MUTED)
                .small(),
        );
        let mut remove: Option<usize> = None;
        for (i, p) in self.excluded_folders.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("Remove").on_hover_text("Stop excluding this folder").clicked() {
                    remove = Some(i);
                }
                ui.label(p.display().to_string());
            });
        }
        if let Some(i) = remove {
            self.excluded_folders.remove(i);
            self.status_message = Some("Exclusion removed — press Rescan to apply.".to_string());
        }

        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.exclude_input_buf)
                    .hint_text("Folder to exclude, e.g. /mnt/c")
                    .desired_width(260.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Add").clicked() || enter {
                let candidate = PathBuf::from(self.exclude_input_buf.trim());
                self.add_exclude(candidate);
                if self.status_message.as_deref().map_or(false, |m| m.starts_with("Excluded")) {
                    self.exclude_input_buf.clear();
                }
            }
            if ui.button("Browse…").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.add_exclude(folder);
                }
            }
            if !self.excluded_folders.is_empty() && ui.button("Clear all").clicked() {
                self.excluded_folders.clear();
                self.status_message = Some("Exclusions cleared — press Rescan to apply.".to_string());
            }
        });
    }

    fn draw_root_picker(&mut self, ui: &mut Ui) {
        let current_label = self
            .root_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Choose a location…".to_string());

        let mut browse_clicked = false;
        let mut entire_system_clicked = false;
        let mut pick: Option<PathBuf> = None;

        egui::ComboBox::from_id_source("root_picker")
            .width(200.0)
            .selected_text(current_label)
            .show_ui(ui, |ui| {
                if ui.selectable_label(false, "Browse…").clicked() {
                    browse_clicked = true;
                }
                if ui
                    .selectable_label(self.show_custom_path_input, "Custom path (incl. network)…")
                    .clicked()
                {
                    self.show_custom_path_input = !self.show_custom_path_input;
                }
                ui.separator();
                if ui.selectable_label(false, ENTIRE_SYSTEM_LABEL).clicked() {
                    entire_system_clicked = true;
                }

                let drives: Vec<&RootEntry> = self
                    .available_roots
                    .iter()
                    .filter(|r| r.kind == crate::scanner::RootKind::Drive)
                    .collect();
                let volumes: Vec<&RootEntry> = self
                    .available_roots
                    .iter()
                    .filter(|r| r.kind == crate::scanner::RootKind::Volume)
                    .collect();

                if !drives.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Drives").small().color(theme::colors::MUTED));
                    for root in drives {
                        if ui.selectable_label(false, &root.label).clicked() {
                            pick = Some(root.path.clone());
                        }
                    }
                }
                if !volumes.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Mounted volumes").small().color(theme::colors::MUTED));
                    for root in volumes {
                        if ui.selectable_label(false, &root.label).clicked() {
                            pick = Some(root.path.clone());
                        }
                    }
                }

                if !self.recent_folders.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Recent").small().color(theme::colors::MUTED));
                    for p in &self.recent_folders {
                        if ui.selectable_label(false, p.display().to_string()).clicked() {
                            pick = Some(p.clone());
                        }
                    }
                }
            });

        if browse_clicked {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                self.start_scan(folder);
            }
        }
        if entire_system_clicked {
            self.start_scan_entire_system();
        }
        if let Some(p) = pick {
            self.start_scan(p);
        }
    }

    /// Fraction of the expected total scanned so far, when a total is known
    /// (whole-drive scans only — see `expected_total_bytes`).
    fn scan_progress_fraction(&self) -> Option<f32> {
        let total = self.expected_total_bytes.filter(|t| *t > 0)?;
        Some((self.bytes_seen as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }

    /// "X / Y scanned (Z%)" when the target's total is known, otherwise just
    /// how much has been found so far — there's no honest denominator for a
    /// subfolder scan until the scan itself finishes.
    fn scan_progress_label(&self) -> String {
        match (self.expected_total_bytes.filter(|t| *t > 0), self.scan_progress_fraction()) {
            (Some(total), Some(frac)) => format!(
                "{} / {} scanned ({:.0}%)",
                format_size(self.bytes_seen, BINARY),
                format_size(total, BINARY),
                frac * 100.0
            ),
            _ => format!(
                "{} scanned · {} items",
                format_size(self.bytes_seen, BINARY),
                self.files_seen
            ),
        }
    }

    fn draw_scan_progress(&self, ui: &mut Ui, width: f32) {
        let label = self.scan_progress_label();
        match self.scan_progress_fraction() {
            Some(frac) => {
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(width)
                        .fill(theme::colors::ACCENT)
                        .text(label),
                )
                .on_hover_text(
                    "Approximate: measured against the drive's used space, which also \
                     includes filesystem overhead and files a scan can't read, so this \
                     may finish a little short of 100%.",
                );
            }
            None => {
                ui.label(RichText::new(label).color(theme::colors::MUTED));
            }
        }
    }

    fn draw_scanning_overlay(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.35).max(20.0));
            ui.spinner();
            ui.add_space(10.0);
            self.draw_scan_progress(ui, ui.available_width().min(420.0));
            ui.add_space(6.0);
            if !self.current_scanning_path.is_empty() {
                ui.label(
                    RichText::new(&self.current_scanning_path)
                        .monospace()
                        .color(theme::colors::MUTED),
                );
            }
        });
    }

    /// Flattens the tree into the rows that are currently visible: the
    /// displayed root (always open), then — depth-first, children sorted per
    /// `sort_by` at every level — the contents of every directory the user
    /// has expanded. Iterative rather than recursive so it can't hit a
    /// stack limit on a very deep tree, and so no closure needs to borrow
    /// `self` recursively.
    fn build_tree_rows(&self) -> Vec<(usize, u32)> {
        let mut rows = Vec::new();
        let mut stack: Vec<(usize, u32)> = vec![(self.current_idx, 0)];
        while let Some((idx, depth)) = stack.pop() {
            rows.push((idx, depth));
            let node = &self.arena[idx];
            let open = depth == 0 || self.expanded.contains(&idx);
            if node.is_dir && open {
                // Pushed in reverse so they pop (and so appear) in order.
                for &child in self.sorted_children(idx).iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }
        rows
    }

    /// TreeSize-style hierarchical list: one continuous indented tree with
    /// expand/collapse arrows, rather than a single folder's contents at a
    /// time. Clicking the arrow expands/collapses in place; clicking a
    /// folder's name makes it the displayed root (same as clicking it in
    /// the treemap or breadcrumb bar), so both views stay in sync.
    fn draw_list(&mut self, ui: &mut Ui) {
        const INDENT: f32 = 18.0;
        const ROW_HEIGHT: f32 = 22.0;

        let key = (
            self.arena.len(),
            self.arena_gen,
            self.current_idx,
            self.sort_by,
            self.expanded_gen,
        );
        let rows = match self.tree_cache.take() {
            Some(cache) if cache.key == key => cache.rows,
            _ => self.build_tree_rows(),
        };

        let mut toggle: Option<usize> = None;
        let mut navigate_to: Option<usize> = None;
        let mut delete: Option<usize> = None;
        let mut exclude: Option<usize> = None;
        let scanning = self.scanning;

        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .column(egui_extras::Column::remainder().at_least(260.0).clip(true))
            .column(egui_extras::Column::auto().at_least(90.0))
            .column(egui_extras::Column::auto().at_least(150.0))
            .column(egui_extras::Column::exact(140.0))
            .header(24.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Name");
                });
                header.col(|ui| {
                    ui.strong("Size");
                });
                header.col(|ui| {
                    ui.strong("% of parent");
                });
                header.col(|_| {});
            })
            .body(|body| {
                // `rows()` only builds the rows actually on screen, so an
                // expanded folder with tens of thousands of entries costs
                // the same per frame as one with twenty.
                body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                    let (idx, depth) = rows[row.index()];
                    let node = &self.arena[idx];
                    let is_dir = node.is_dir;
                    let has_children = is_dir && !node.children.is_empty();
                    let size = node.size;
                    let name = node.name.clone();
                    let parent_size = node
                        .parent
                        .map(|p| self.arena[p].size)
                        .filter(|_| depth > 0)
                        .unwrap_or(size);
                    let frac = if parent_size == 0 {
                        0.0
                    } else {
                        (size as f64 / parent_size as f64).clamp(0.0, 1.0) as f32
                    };
                    let open = depth == 0 || self.expanded.contains(&idx);

                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(depth as f32 * INDENT);
                            if depth > 0 && has_children {
                                let (rect, resp) =
                                    ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::click());
                                egui::collapsing_header::paint_default_icon(
                                    ui,
                                    if open { 1.0 } else { 0.0 },
                                    &resp,
                                );
                                if resp.clicked() {
                                    toggle = Some(idx);
                                }
                                let _ = rect;
                            } else {
                                ui.add_space(16.0 + ui.spacing().item_spacing.x);
                            }
                            if is_dir {
                                if open {
                                    if ui.link(RichText::new(&*name).color(theme::colors::OTHER_ACCENT).strong()).clicked() {
                                        navigate_to = Some(idx);
                                    }
                                } else {
                                    if ui.link(RichText::new(&*name).color(theme::colors::ACCENT).strong()).clicked() {
                                        navigate_to = Some(idx);
                                    }
                                }
                            } else {
                                ui.label(RichText::new(&*name).color(theme::colors::MUTED));
                            }
                        });
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(format_size(size, BINARY)).monospace());
                    });
                    row.col(|ui| {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width().max(60.0), 14.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 2.0, theme::colors::ELEVATED);
                        let mut fill = rect;
                        fill.set_width(rect.width() * frac);
                        ui.painter().rect_filled(
                            fill,
                            2.0,
                            theme::colors::TREEMAP[(depth as usize) % theme::colors::TREEMAP.len()],
                        );
                        ui.painter().text(
                            rect.left_center() + egui::vec2(5.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            format!("{:.1}%", frac * 100.0),
                            egui::FontId::monospace(11.0),
                            theme::colors::TEXT,
                        );
                    });
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            if depth > 0
                                && is_dir
                                && ui
                                    .add(egui::Button::new(RichText::new("Exclude").color(theme::colors::ACCENT)).small())
                                    .on_hover_text("Exclude this folder from future scans")
                                    .clicked()
                            {
                                exclude = Some(idx);
                            }
                            if depth > 0
                                && ui
                                    .add_enabled(
                                        !scanning,
                                        egui::Button::new(RichText::new("✕").color(theme::colors::DANGER)).small(),
                                    )
                                    .on_hover_text("Delete")
                                    .clicked()
                            {
                                delete = Some(idx);
                            }
                        });
                    });
                });
            });

        // Put the (possibly reused) rows back under the key they were built
        // for; if this frame changed `expanded`/`current_idx`, next frame's
        // key won't match and they'll be rebuilt then.
        self.tree_cache = Some(TreeCache { key, rows });

        if let Some(idx) = toggle {
            if !self.expanded.remove(&idx) {
                self.expanded.insert(idx);
            }
            self.expanded_gen += 1;
        }
        if let Some(idx) = navigate_to {
            self.current_idx = idx;
        }
        if let Some(idx) = delete {
            self.pending_delete = Some(idx);
        }
        if let Some(idx) = exclude {
            let p = self.path_of(idx);
            self.add_exclude(p);
        }
    }

    /// Max real children laid out individually; anything past this is
    /// folded into one "Other" block. Without a cap, a folder with a
    /// thousand small files turns into an unreadable field of slivers —
    /// this keeps the treemap comprehensible regardless of folder size,
    /// at the cost of not showing every last tiny file as its own block
    /// (hovering "Other" still tells you how many are in there).
    const MAX_VISIBLE_BLOCKS: usize = 40;

    fn draw_treemap(&mut self, ui: &mut Ui) {
        // Treemap layout always goes by size, regardless of the List
        // view's Size/Name sort toggle — a name-ordered squarified treemap
        // has no coherent visual meaning (size order is what makes the
        // biggest, most important blocks land in the most stable corner).
        let mut kids = self.arena[self.current_idx].children.clone();
        kids.sort_by(|a, b| self.arena[*b].size.cmp(&self.arena[*a].size));

        if kids.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("(empty folder)").color(theme::colors::MUTED));
            });
            return;
        }

        // `idx: None` marks the synthetic "Other" block, which isn't a
        // real arena entry and isn't navigable.
        struct Block {
            idx: Option<usize>,
            size: u64,
            label: String,
            is_dir: bool,
            grouped_count: usize,
        }

        let mut blocks: Vec<Block> = Vec::new();
        let visible_count = kids.len().min(Self::MAX_VISIBLE_BLOCKS);
        for &idx in &kids[..visible_count] {
            let n = &self.arena[idx];
            blocks.push(Block {
                idx: Some(idx),
                size: n.size,
                label: n.name.to_string(),
                is_dir: n.is_dir,
                grouped_count: 1,
            });
        }
        if kids.len() > Self::MAX_VISIBLE_BLOCKS {
            let rest = &kids[Self::MAX_VISIBLE_BLOCKS..];
            let other_size: u64 = rest.iter().map(|&i| self.arena[i].size).sum();
            blocks.push(Block {
                idx: None,
                size: other_size,
                label: format!("Other ({} items)", rest.len()),
                is_dir: false,
                grouped_count: rest.len(),
            });
        }

        let total: f64 = blocks.iter().map(|b| b.size as f64).sum::<f64>().max(1.0);

        let avail = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click());
        let painter = ui.painter_at(rect);

        let sizes: Vec<f64> = blocks.iter().map(|b| b.size as f64).collect();
        let bounds = crate::treemap::Rect {
            x: rect.left(),
            y: rect.top(),
            w: rect.width(),
            h: rect.height(),
        };
        let layout = crate::treemap::squarify(&sizes, bounds);

        let mut clicked_idx: Option<usize> = None;
        let mut hovered: Option<usize> = None;
        let pointer = response.interact_pointer_pos();
        let hover_pos = response.hover_pos();

        // Small gap between blocks so adjacent regions read as distinct
        // pieces instead of one solid mass, especially with many blocks.
        const GUTTER: f32 = 1.5;

        for (n, block) in blocks.iter().enumerate() {
            let r = layout[n];
            if r.w < 2.0 || r.h < 2.0 {
                continue;
            }
            let inset = GUTTER.min(r.w * 0.15).min(r.h * 0.15);
            let egui_rect = egui::Rect::from_min_size(
                egui::pos2(r.x + inset, r.y + inset),
                egui::vec2((r.w - inset * 2.0).max(1.0), (r.h - inset * 2.0).max(1.0)),
            );

            let is_hovered = hover_pos.map_or(false, |p| egui_rect.contains(p));
            if is_hovered {
                hovered = Some(n);
            }

            let base = if block.idx.is_none() {
                theme::colors::OTHER_BUCKET
            } else {
                color_for(n, block.is_dir)
            };
            let fill = if is_hovered { brighten(base, 22) } else { base };

            painter.rect_filled(egui_rect, 3.0, fill);
            painter.rect_stroke(
                egui_rect,
                3.0,
                if is_hovered {
                    egui::Stroke::new(2.0_f32, theme::colors::ACCENT)
                } else {
                    egui::Stroke::new(1.0_f32, theme::colors::BG)
                },
            );

            // Labels are clipped to their own block (the shared `painter`
            // only clips to the whole treemap area, so without this a long
            // name spills over its neighbors) and ellipsized to the block's
            // width; text color is chosen for contrast against this
            // block's actual fill rather than assumed.
            let text_color = text_on(fill);
            let block_painter = painter.with_clip_rect(egui_rect);
            let pad = 5.0;
            let usable_w = egui_rect.width() - pad * 2.0;
            let name_size = (11.0 + r.w.min(r.h) / 80.0).clamp(11.0, 16.0);
            if usable_w > 24.0 && egui_rect.height() > name_size + 4.0 {
                paint_ellipsized(
                    &block_painter,
                    egui_rect.left_top() + egui::vec2(pad, 3.0),
                    &block.label,
                    egui::FontId::proportional(name_size),
                    text_color,
                    usable_w,
                );
                // Second line (size, plus % of this folder when there's
                // room for both) only if a whole extra line actually fits.
                if egui_rect.height() > name_size * 2.0 + 8.0 {
                    let pct = block.size as f64 / total * 100.0;
                    let detail = if usable_w > 110.0 {
                        format!("{} · {:.1}%", format_size(block.size, BINARY), pct)
                    } else {
                        format_size(block.size, BINARY)
                    };
                    paint_ellipsized(
                        &block_painter,
                        egui_rect.left_top() + egui::vec2(pad, 3.0 + name_size + 3.0),
                        &detail,
                        egui::FontId::proportional((name_size - 1.0).max(10.0)),
                        text_color.gamma_multiply(0.85),
                        usable_w,
                    );
                }
            }

            if response.clicked() {
                if let Some(p) = pointer {
                    if egui_rect.contains(p) {
                        if let (Some(idx), true) = (block.idx, block.is_dir) {
                            clicked_idx = Some(idx);
                        }
                    }
                }
            }
        }

        // Hover tooltip: the main way to identify a block too small to
        // carry its own inline label, and to see the exact byte count and
        // percentage for any block, large or small.
        if let Some(n) = hovered {
            let block = &blocks[n];
            let pct = block.size as f64 / total * 100.0;
            egui::show_tooltip_at_pointer(ui.ctx(), egui::Id::new("wyvernscan_treemap_tooltip"), |ui| {
                ui.label(format!("{}\n{} ({:.1}%)", block.label, format_size(block.size, BINARY), pct));
                if block.grouped_count > 1 {
                    ui.label(
                        RichText::new(format!("{} smaller items grouped together", block.grouped_count))
                            .color(theme::colors::MUTED)
                            .small(),
                    );
                }
            });
        }

        if let Some(idx) = clicked_idx {
            self.current_idx = idx;
        }

        // Right-click anywhere in the treemap to go up a level.
        if response.secondary_clicked() {
            if let Some(parent) = self.arena[self.current_idx].parent {
                self.current_idx = parent;
            }
        }
    }

    fn draw_delete_confirm(&mut self, ctx: &egui::Context) {
        if let Some(idx) = self.pending_delete {
            let name = self.arena[idx].name.clone();
            let is_dir = self.arena[idx].is_dir;
            let mut close = false;
            let mut do_delete = false;

            egui::Window::new("Confirm delete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Permanently delete {} \"{}\"? This cannot be undone.",
                        if is_dir { "folder" } else { "file" },
                        name
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        if ui
                            .button(RichText::new("Delete").color(theme::colors::DANGER))
                            .clicked()
                        {
                            do_delete = true;
                            close = true;
                        }
                    });
                });

            if do_delete {
                self.delete_node(idx);
            }
            if close {
                self.pending_delete = None;
            }
        }
    }

    fn delete_node(&mut self, idx: usize) {
        let path = self.path_of(idx);
        let is_dir = self.arena[idx].is_dir;
        let result = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => self.rescan(),
            Err(e) => {
                self.status_message = Some(format!("Delete failed: {e}"));
            }
        }
    }

    /// Reconstruct the absolute path for `idx`. Normally that just means
    /// walking up to the root and joining components onto `root_path`, but
    /// a node created by `merge_roots` (e.g. under "Entire System") carries
    /// its own `abs_path` instead, since its `name` there is a display
    /// label like "C:\" rather than a joinable path component.
    fn path_of(&self, idx: usize) -> PathBuf {
        let mut components: Vec<String> = Vec::new();
        let mut cur = idx;
        let base = loop {
            if let Some(abs) = &self.arena[cur].abs_path {
                break abs.clone();
            }
            match self.arena[cur].parent {
                Some(parent) => {
                    components.push(self.arena[cur].name.to_string());
                    cur = parent;
                }
                None => break self.root_path.clone().unwrap_or_default(),
            }
        };
        components.reverse();
        let mut path = base;
        for c in components {
            path.push(c);
        }
        path
    }
}

/// Uploads the brand logo as a texture. The pixels are raw RGBA baked in at
/// compile time (generated by `assets/make_icon.py`), so no image-decoding
/// dependency is needed. 96px: shown at 24pt in the toolbar and 96pt on the
/// empty screen, which is crisp at 1x and 2x scaling respectively.
fn load_logo(ctx: &egui::Context) -> egui::TextureHandle {
    const LOGO_PX: usize = 96;
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [LOGO_PX, LOGO_PX],
        include_bytes!("../assets/icon-96.rgba"),
    );
    ctx.load_texture("wyvernscan-logo", image, egui::TextureOptions::LINEAR)
}

fn color_for(n: usize, is_dir: bool) -> Color32 {
    let c = theme::colors::TREEMAP[n % theme::colors::TREEMAP.len()];
    if is_dir {
        c
    } else {
        // Lighter, desaturated version so files visually recede behind folders.
        Color32::from_rgb(
            c.r() / 2 + 90,
            c.g() / 2 + 90,
            c.b() / 2 + 90,
        )
    }
}

/// Perceived brightness (0..=255), for picking a readable text color.
fn luminance(c: Color32) -> f32 {
    0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32
}

/// Light or dark text, whichever contrasts better with `bg`.
fn text_on(bg: Color32) -> Color32 {
    if luminance(bg) > 150.0 {
        theme::colors::BG
    } else {
        theme::colors::TEXT
    }
}

fn brighten(c: Color32, amount: u8) -> Color32 {
    Color32::from_rgb(
        c.r().saturating_add(amount),
        c.g().saturating_add(amount),
        c.b().saturating_add(amount),
    )
}

/// Paints one line of text that never exceeds `max_width`: anything that
/// doesn't fit is cut off and replaced with an ellipsis, instead of
/// overflowing into whatever is next to it.
fn paint_ellipsized(
    painter: &egui::Painter,
    pos: egui::Pos2,
    text: &str,
    font: egui::FontId,
    color: Color32,
    max_width: f32,
) {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, max_width);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    painter.galley(pos, galley, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_are_added_once_and_only_if_the_folder_exists() {
        let dir = std::env::temp_dir().join(format!("wyvernscan_gui_excl_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = WyvernScanApp::default();

        app.add_exclude(dir.clone());
        assert_eq!(app.excluded_folders, vec![dir.clone()]);
        assert!(app.status_message.as_deref().unwrap().starts_with("Excluded"));

        app.add_exclude(dir.clone()); // duplicate
        assert_eq!(app.excluded_folders.len(), 1);
        assert!(app.status_message.as_deref().unwrap().starts_with("Already"));

        app.add_exclude(dir.join("missing")); // nonexistent
        app.add_exclude(PathBuf::new()); // blank input
        assert_eq!(app.excluded_folders.len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Settings written by the version before exclusions existed must still
    /// load (keeping the recent folders), not fall back to defaults.
    #[test]
    fn old_saved_settings_without_exclusions_still_load() {
        let old = r#"{"recent_folders":["/home/x"],"scan_mode":"Normal"}"#;
        let p: Persisted = serde_json::from_str(old).unwrap();
        assert_eq!(p.recent_folders, vec![PathBuf::from("/home/x")]);
        assert!(p.excluded_folders.is_empty());

        let new = Persisted { excluded_folders: vec![PathBuf::from("/mnt/c")], ..p };
        let back: Persisted = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back.excluded_folders, vec![PathBuf::from("/mnt/c")]);
    }
}
