# AGENTS.md — WyvernScan

Context and ground rules for AI agents working in this repository. User-facing
docs are in `README.md`; this file is for anyone (human or agent) about to
change code here.

## What this is

A TreeSize/WizTree-style disk usage visualizer in Rust: an `egui`/`eframe`
GUI, an NTFS-MFT fast-scan path for whole Windows drives, and a headless `--cli` mode. Built
and iterated entirely in a Linux sandbox with **no Windows machine ever
available** — this one fact shapes several decisions below.

## Module map

| File | Purpose |
|---|---|
| `scanner.rs` | Core cross-platform walker (worker threads doing `std::fs::read_dir` + a bounded result channel — no `jwalk`), the arena tree (`Node`/`ScanResult`), size aggregation, multi-root merge, drive/volume enumeration. Shared by the GUI and the CLI. |
| `mft.rs` | NTFS fast scan: streams the `$MFT` front to back in big aligned chunks (a reader thread overlaps disk reads with parsing) and grows the tree incrementally from each record's parent reference, with live snapshots like the normal scan. Follows an `$ATTRIBUTE_LIST` when the `$MFT` itself is fragmented. Hand-written record parser, **no `ntfs` crate**. Pure `std`, so it compiles and is tested on every platform; only opening `\\.\C:` is Windows-specific. |
| `app.rs` | The egui GUI: app state, the tree-list and treemap views, scan orchestration, location/mode pickers. |
| `cli.rs` | Headless `--cli` mode. **Must never import `eframe`/`egui`/winit** — see Design decisions. |
| `theme.rs` | Visual design tokens (colors, the treemap palette). See its header comment for the design direction. |
| `banner.rs` | The ASCII-art startup banner for `--cli`. Pure `std`, 7-bit ASCII, printed to stderr only when stderr is a terminal. |
| `treemap.rs` | Squarified treemap layout algorithm — pure math, no UI dependency. |
| `disk_space.rs` | Cross-platform "used space on this volume" query (`GetDiskFreeSpaceExW` / `statvfs`), one fast OS call, not a directory walk. |
| `debug_log.rs` | `--debug` file + console logging (stderr in `--cli`, stdout in the GUI), system-info header, used everywhere including inside `mft.rs`'s top-level panic handler. |
| `build.rs` | Embeds `assets/icon.ico` into the Windows `.exe` via `winresource` (Windows hosts only, fail-soft). |
| `assets/` | `icon_base.svg` (ring + field), `icon_stripes.svg` and `dragon_head.png` (the artwork) are the sources of truth; `make_icon.py` composites them and regenerates `icon.ico`, `icon-512.png` and the raw `icon-256.rgba` / `icon-96.rgba` that `main.rs` and `app.rs` embed with `include_bytes!`. |
| `win_integration.rs` | **Windows only.** Hand-rolled FFI: console allocation (`--debug`), console attach for `--cli`, elevation check, restart-as-admin. |

## Building and testing here (sandbox-only workaround)

The sandbox's system Rust (installed via `apt`, not `rustup`) is old enough
that current crates.io has drifted past what it can resolve — a plain
`cargo build` here hits MSRV/edition2024 failures across roughly a dozen
transitive dependencies, none of which are this project's own code.

**A real, current Rust toolchain (via rustup) needs none of this.** The
shipped `Cargo.toml` must stay clean of sandbox-only pins. When verifying
changes in this sandbox, append pins to a **scratch copy**, never the real
file:

```toml
idna_adapter = "=1.2.0"
wayland-protocols = "=0.32.12"
home = "=0.5.9"
smithay-clipboard = "=0.7.2"
litemap = "=0.7.4"
quick-xml = "=0.39.4"
wayland-scanner = "=0.31.10"
mime_guess2 = "=2.0.5"
indexmap = "=2.11.4"
```

...and change the `rfd` line to
`rfd = { version = "0.14", default-features = false, features = ["gtk3"] }`
(avoids an unrelated icu4x/edition2024 chain pulled in by `rfd`'s default
xdg-portal backend on Linux).

If a future dependency bump breaks this again: `cargo check` will name the
offending crate and its MSRV; pin it to the newest version whose declared
`rust-version` the sandbox toolchain satisfies, then re-check. Do this in
the scratch copy only.

## What can and can't be verified here

`mft.rs` is pure `std` and compiled on every platform, so unlike earlier
versions of this project it is **fully testable in the Linux sandbox**:

- `cargo test` runs a fully synthetic NTFS volume built in memory (hard links,
  8.3 aliases, extension records, deleted/orphan records, torn records,
  cycles, a fragmented-`$MFT` rejection), a device that rejects any
  non-4096-aligned read, reads forced to straddle record boundaries, and a
  garbage-input fuzz loop proving the parsers never panic.
- **Against a real NTFS filesystem** — the only thing that proves the format
  knowledge is right, since hand-built test records only encode what the
  author already believed:
  `apt install ntfs-3g`, then
  `truncate -s 3G img && mkntfs -F -Q -q img &&
  ntfs-3g -o rw,big_writes,show_sys_files,streams_interface=none img mnt`
  (FUSE and mounting work in the sandbox). Populate it with a nasty tree
  (tens of thousands of files, a huge directory, deep nesting, unicode names,
  hard links incl. one file with 60 long-named links, sparse and heavily
  fragmented files, deletions), dump `path<TAB>D|F<TAB>size` for everything
  with an *independent* walker (Python `os.walk` + `lstat`), `umount`, then
  call `mft::scan_reader` on a `File` of the image from a scratch test and
  diff the two dumps. The last run was **byte-for-byte identical** on 30,855
  and on 255,116 entries, and the images did contain extension records and
  `$ATTRIBUTE_LIST`s (check, don't assume — a vacuous pass is worthless).
  A **fragmented `$MFT`** — what a real, heavily used `C:` looks like, and what
  the first version wrongly refused — can be made too: a 300MB image, fill it
  with 64KiB files until ENOSPC, delete every other one (scattered holes),
  then create ~60,000 empty files so the `$MFT` must grow into the holes.
  That yielded 162 extents and an `$ATTRIBUTE_LIST` in record 0, and the scan
  matched ground truth exactly (the previous version fails on that same image
  with "`$MFT` is fragmented across extension records").
  Cold-cache timings in the sandbox are meaningless (a 250MB `$MFT` read from
  the sandbox disk takes seconds); re-run warm before drawing conclusions.
  Mutation-check new tests too: breaking the chunk carry-over logic must make
  the straddle test fail.

**Direct I/O can be verified here too**: open the image with
`OpenOptionsExt::custom_flags(libc::O_DIRECT)` in the scratch test. `O_DIRECT`
has the same rules as `FILE_FLAG_NO_BUFFERING` (aligned address, offset and
length) and the kernel enforces them with `EINVAL` ("Invalid argument", the
twin of `ERROR_INVALID_PARAMETER`), so a clean run proves the I/O path obeys
them; deliberately mis-aligning `AlignedBuf` makes it fail. The unit tests'
`StrictDevice` enforces the same three alignments in memory.

What genuinely can't be verified here: `win_integration.rs` (it uses
`std::os::windows` and Win32 FFI), and the behaviour of a real raw `\\.\C:`
handle on Windows (alignment rules, access rights, antivirus interaction).
That is why every volume read goes through `read_exact_aligned`, why
`--debug` exists, and why any failure in the fast path falls back to the
normal scan. State uncertainty honestly for those two things.

Measured on real hardware by the project owner for the *previous* design (a
per-directory index walk through the `ntfs` crate): 2TB NVMe, 3.6M files —
fast scan 1m47s vs 18s for the normal scan; 1TB HDD, 94k files — 1m46s vs
1m29s. The fast scan was *slower*, because it did roughly one dependent read
per folder (~180µs each on NVMe, a head seek each on the HDD). Reference
hardware (CrystalDiskMark, sequential 1MiB / 4KiB random at queue depth 1):
2TB NVMe 2.7-3.7GB/s / 45MB/s; 256GB SATA SSD 430-560MB/s / 5-9MB/s; 1TB
HDD 96MB/s / 0.5MB/s. On the HDD the fast scan (~9s for a ~860MB `$MFT`) is
already at the disk's sequential floor, so only reading *less* could help
there; its "0.3s" normal scan was a warm RAM cache from a previous scan (the
cold one took 1m24s), so compare cold to cold or not at all. That is why
the sequential `$MFT` read exists. The first sequential version then fell back
to the normal scan on the owner's real `C:` because of the fragmented `$MFT`
(fixed, see above). Re-measure on real hardware after changes.

## Design decisions — don't regress these

Each of these exists because an earlier, more "obvious" approach caused a
real, reported bug. Noted so the same mistake doesn't get reintroduced.

- **The normal scan must never buffer unboundedly ahead of its consumer.**
  `scanner::scan()` had a real ~20GB RAM bug on big drives, with two causes
  that both had to be fixed:
  1. It used `jwalk`, whose internal result queue is *unbounded*: its
     parallel readers raced arbitrarily far ahead of the single consumer
     thread (which was slowed further by a serial `stat` per file), holding
     every unconsumed `DirEntry` in RAM. Now `scan()` runs its own workers
     (`read_batch`) that do **all** syscalls (`read_dir` + `DirEntry::metadata`,
     which is free on Windows because the size comes with the listing) and
     send one `Batch` per directory over a **bounded** channel
     (`threads * 4`), so workers block when the consumer lags. Don't swap
     that channel for an unbounded one, and don't move `metadata()` calls back
     onto the consumer thread.
  2. Live "Partial" snapshots clone the whole arena into the channel. A GUI
     that isn't polling (`poll_scan` only runs from `update()`, which stops
     while the window is minimized/occluded) let full-tree copies queue up
     without limit — measured 67 queued snapshots = 1.26GB on only a
     400k-file tree. A snapshot is now only taken when `tx.is_empty()`. (The
     fast scan sidesteps this entirely: it takes no live snapshots.)
  Also still true from the earlier fix: **no path string per file is ever
  stored.** The parent arena index travels with each directory job
  (`(parent_idx, PathBuf)`), so only directories still waiting to be read
  hold a path — the old `HashMap<PathBuf, usize>` (and, before that, a
  depth-indexed stack that depended on jwalk's DFS ordering) are both gone.
  Traversal is therefore breadth-first, and arena children order is arrival
  order, not DFS order — every consumer (`app.rs`, `cli.rs`) sorts children
  itself, so nothing may rely on arena order.
- **The normal scan never reads or sums virtual filesystems.** `scanner::ScanFilter`
  (built once per scan from `/proc/self/mountinfo`, falling back to `/proc/mounts`) skips the
  mount points of `proc`, `sysfs`, `devtmpfs`, cgroups etc. below the scan root; they appear as
  empty 0 B folders (`EntryKind::SkippedDir`). Real bug: `/proc/kcore` reports `st_size` = 128 TiB,
  so scanning `/` showed 128 TiB within a second (reported on two servers and WSL). If the root
  itself is on a virtual fs, file sizes count as 0. Sockets/FIFOs/device nodes never contribute
  a size. `tmpfs` is deliberately *not* in the list (real data). The decision logic
  (`parse_mount_table`, `ScanFilter::from_mounts`) is pure so it is tested everywhere; the
  mutation "never skip" makes two tests fail. Other mounts below the root (e.g. `/mnt/c` in WSL)
  are still descended into unless the user passes `--exclude`: `scanner::scan_excluding` adds
  the canonicalized folders to the same `skip_dirs` map (value `EXCLUDED_TAG`), folders only --
  checking files would cost a path allocation per file. Excludes must be canonicalized because
  the walk's paths derive from the canonical root (on Windows that is a `\\?\` verbatim path).
  `scanner::split_excluded_roots` (used by `cli::run_entire_system` and the GUI's `start_scan_entire_system`) drops roots inside an excluded folder, which is what stops a
  `/mnt/c` double count. Excludes force the Windows normal scan (the MFT scan can't leave a
  folder out) -- that branch is unverified, no Windows machine.
- **GUI exclusions** live in `WyvernScanApp::excluded_folders`, persisted in `Persisted`
  (`#[serde(default)]` so settings saved before this field existed still load), snapshotted when a
  scan starts and passed to `scanner::scan_excluding`. They are edited in the toolbar panel
  (`draw_excludes_panel`) and via the per-row **Exclude** button in the list view. The progress
  bar's "of Y" total is dropped when an exclusion removes content from a scanned volume
  (`exclusions_shrink_total`). The default glyph font has no `✕`/`⊘` (they render as boxes), so new
  buttons use text labels; the pre-existing delete button's `✕` still shows as a box. The list
  table's last column is `Column::exact`: `auto` sized it after the name column and clipped it.
- **`--debug` output goes to stderr in `--cli`.** `main.rs` calls `debug_log::route_to_stderr()`
  before `init()`; stdout carries the report, and debug lines there broke `--json`. `log()` ignores
  write errors, because a panicking `println!` on a closed pipe inside the panic hook aborts the
  process. In debug mode `cli.rs` replaces the `\r` progress line with a log line every 2 s.
- **Directory entries are one contiguous arena range per directory.**
  `scan()` pushes a whole `Batch` at once, sets `children` to the exact
  `first..len` range (no doubling-growth slack), and propagates the batch's
  summed file size up the ancestors **once per directory**, not per file.
- **Sizes propagate incrementally, not via a separate aggregation pass.**
  `scanner::propagate_size_up()` adds sizes to every ancestor at insertion
  time in `scan()` (O(folder depth) per directory); `mft.rs` instead sums the
  finished arena in one reverse pass, which works because it builds
  breadth-first so a parent always has a lower index than its children. Both replace what used to be a
  periodic O(whole tree so far) reverse-pass run on every live snapshot.
  Any new code path that inserts nodes must call this rather than
  reinventing a separate aggregation step.
- **Node names are `Arc<str>`, not `String`** (`scanner::Node::name`). The
  live "Partial" snapshot feature clones the whole arena periodically to
  hand the GUI thread a point-in-time copy; `Arc<str>::clone()` is a
  refcount bump, `String::clone()` is an allocation + copy. Comparing
  against a string literal needs `&*name == "literal"`, not
  `name == "literal"` — there's no blanket `PartialEq<str>` impl for `Arc<str>`.
- **The fast scan reads the `$MFT` sequentially; never go back to walking
  directory indexes.** The old design (`ntfs` crate, one directory at a
  time) did roughly one dependent disk read per folder: ~107s on a 2TB NVMe
  and ~106s on a 1TB HDD, slower than the plain walker. The current design
  streams the whole `$MFT` in 8MiB aligned chunks on a reader thread while
  the main thread parses; tables are indexed by record number (names packed
  in one `String` pool, not one allocation each) and the tree is assembled
  breadth-first from the root record (5). Hard links are counted under every
  parent (like a directory walk), the 8.3 DOS alias namespace is skipped,
  unnamed `$DATA` gives the size (alternate streams ignored), and records not
  reachable from the root simply don't appear.
- **Raw-volume reads are unbuffered and fully 4096-aligned.** Two separate
  facts, both learned the hard way:
  1. Windows rejects *any* unaligned read on a volume handle with
     `ERROR_INVALID_PARAMETER` (a real, user-reported bug), so every read goes
     through `read_exact_aligned` (aligned offset and length; 4096 covers 512e
     and 4Kn drives).
  2. A *buffered* handle on a raw volume is served through the cache in small
     synchronous pieces. Measured on the owner's machine, the sequential
     `$MFT` read ran at ~150MB/s on an NVMe drive that streams ~3GB/s and took
     13.7s on a SATA SSD that streams ~500MB/s — i.e. nowhere near the
     device, and the effective rate tracked each drive's *latency*, not its
     bandwidth. `open_volume` therefore uses `FILE_FLAG_NO_BUFFERING`, which
     turns an 8MiB read into a few large device I/Os. That flag also requires
     the **memory address** to be sector-aligned, which a `Vec<u8>` does not
     guarantee: bulk reads land in `AlignedBuf` (4096-aligned, recycled through
     a pool so a multi-GB scan doesn't fault in fresh memory per chunk), and
     `read_exact_aligned` only reads straight into the caller's slice when the
     address is aligned, bouncing through an `AlignedBuf` otherwise.
  If an unbuffered read fails with an I/O error, `scan_volume_inner` retries
  once with buffered reads (logged), because the Windows behaviour couldn't be
  tested here — keep that fallback. `MFT timing:` in the debug log splits a
  scan into read / parser-waited-on-disk / parsing / snapshots / finishing, and
  `MFT: N of M records in use` says how much of the table was dead space, so
  the next regression is diagnosed from facts, not guessed.
- **The fast scan reads only the in-use parts of the `$MFT`.** The `$MFT` never
  shrinks: delete a venv or `node_modules` with 100,000+ files and its records
  stay in the table as free slots forever, so a churned drive's `$MFT` can be
  mostly dead space. Reading it is wasted I/O, and since free records produce
  no names the progress label froze on the last file name for seconds (a
  user-reported "stuck on `__init__.cpython-312.pyc`" — the file's size is
  irrelevant, the scan never reads file contents). `locate_mft` reads the
  `$MFT`'s own `$BITMAP` (best effort: any trouble means "read everything"),
  `needed_ranges` turns it into byte ranges (free gaps under 1 MiB are read
  through — a seek costs about that much on an HDD — and ranges are rounded to
  4096 so no edge forces a bounce read), and the reader only fetches those.
  Every chunk carries its `stream_off`; when it isn't the next byte after the
  previous chunk, the consumer clears the straddle `carry` and **renumbers**
  (`Tables::next_rec`). Getting that wrong silently orphans children of any
  directory after a gap — the test needs a directory *and* a child record
  after the gap, because a plain file there can't reveal it (a weaker version
  of that test passed under the mutation; check tests against mutations).
  Progress is also sent on a 60ms timer while waiting for the disk
  (`recv_timeout`), so a slow stretch can't look like a freeze, and any single
  read over 500ms is logged with its position, and after 1.5s of silence the
  status line says the drive is slow (restored when data flows again). Real
  case from the owner's SATA SSD: after skipping 80% of a 733 MiB `$MFT`, two
  *adjacent* 8 MiB reads at one fixed disk location took 5.0s and 4.5s
  (~1.7 MB/s) while the other 138 MiB read in ~1s — slow media (consistent with
  its 435-767us 4KiB QD1 latency in CrystalDiskMark), not the scanner. Don't
  chase that in code. A possible future mitigation is several reads in flight
  (CrystalDiskMark shows QD8 beating QD1 by 20-35% on SSDs), but a synchronous
  Windows file object serializes its I/O, so it needs one handle per reader and
  in-order reassembly with a bounded window — unmeasurable without Windows
  hardware, so not done. Real-image check: a 155,467-
  record `$MFT` that was 81% free read 28 MiB instead of 151 MiB, 3x faster,
  identical to ground truth.
- **The fast scan follows a fragmented `$MFT`.** A heavily used drive's `$MFT`
  has so many extents that record 0 can't hold the runlist: it carries an
  `$ATTRIBUTE_LIST` naming extension records that each hold one more extent
  of the unnamed `$DATA`. `locate_mft` reads the list (resident or
  non-resident), then fetches each extension record through the extents
  already known (the same bootstrap the NTFS driver does), requiring the
  extents to be contiguous in virtual-cluster order. An `$MFT` extension
  record's base reference is "record 0 + sequence number", so `Parsed::base`
  is kept **unmasked**: masking it to 0 makes it look like a base record.
  This was a real, user-reported failure on a real `C:`; don't reintroduce
  "fragmented => give up".
- **The fast scan still fails closed on what it can't verify.** A non-NTFS
  volume, an unusual cluster size, a sparse `$MFT` extent, an extension record
  outside the extents read so far, a gap in VCNs, a missing root record —
  all return `Err` and the caller falls back to the normal scan. Don't "make
  it guess" instead. The parsers bounds-check everything
  (`u16_at`/`u32_at`/`u64_at` return `Option`) and are fuzzed to never panic;
  a single torn record costs one counted error (and a debug-log line), not
  the scan.
- **The fast scan has a live view, and its tree is built incrementally and
  append-only.** The GUI keeps arena indices (what's expanded, where the user
  is looking) valid across live snapshots *because scanners only ever append*
  (`App::apply_partial_result` says so). So `Tables` attaches each name as it
  arrives if its parent directory is already in the tree, and otherwise parks
  it on a per-parent waiting list (threaded through `Entry::next`) that is
  released when that directory appears. A parent therefore always has a lower
  index than its children, so directory sizes are summed in one reverse pass
  over each snapshot clone and over the final arena. Entries that depend on
  extension records (`has_attr_list` or an extension record's own names) are
  deferred to the very end, when their sizes are final. A "rebuild the whole
  tree per snapshot" design looks simpler but silently corrupts the GUI's
  state mid-scan. Snapshots use the same pacing and `tx.is_empty()` guard as
  the normal scan, and the same rule applies: never reorder or remove nodes.
- **`--debug` logs the detail behind every counted error.** `debug_log::
  log_item` writes one line per failed directory/entry/stat in the normal scan
  (path + OS error) and per unusable `$MFT` record in the fast scan, capped at
  2000 lines per scan (then one "suppressed" notice; the on-screen count stays
  exact), plus a one-line summary at the end. The count and the log must
  agree: anything logged as an error is counted, and vice versa. Test it as an
  unprivileged user (`setpriv --reuid=65534 ...`) — root never hits permission
  errors.
- **egui table cells with more than one widget need `ui.horizontal(...)`.**
  A real bug (tree-list names invisible) came from stacking widgets in a
  cell without this — egui's default cell layout is top-to-bottom, so
  unwrapped widgets stack vertically and push later content below the row's
  visible height instead of sitting side by side.
- **`panic = "abort"` must never return to `[profile.release]`.** It
  previously turned *any* panic anywhere in the app into a full process
  crash instead of a safely caught, isolated failure (a real, reported bug).
  `mft::scan_volume` still wraps the fast scan in one `catch_unwind` and
  falls back to the normal scan if it ever fires.
- **`cli.rs` must never import `eframe`/`egui`/winit**, even transitively.
  Its entire purpose is running correctly on a machine with no display at
  all; pulling in GUI-initialization code, even unused, defeats that. The
  `--cli` flag is checked and dispatched in `main()` before any GUI setup
  runs, specifically so a headless server never touches that code path.
- **`--cli` on Windows needs `attach_console`, because the release exe has no console.**
  `main.rs` sets `windows_subsystem = "windows"` for release builds, and a
  process started that way has *no* stdout/stderr: `println!` goes nowhere.
  Debug builds keep a console, which is why this never showed up under
  `cargo run`. `win_integration::attach_console` therefore runs *before*
  `cli::parse_args` (which prints `--help` and argument errors while parsing),
  attaches to the parent terminal's console (or allocates one), and points only
  the std handles that are actually missing at `CONOUT$`, so `> file` and pipes
  keep working. Every console-mode exit goes through `cli::exit` so a console we
  allocated ourselves is held open until Enter. Known limitation of using one
  exe: cmd/PowerShell don't wait for GUI-subsystem programs, so the prompt
  returns at once and the exit code isn't reported (`start /wait` fixes both).
  The proper fix, if scripts matter, is a second console-subsystem binary
  (`wyvernscan-cli.exe`) sharing the modules. This was never run on a real
  Windows machine by the assistant that wrote it -- verify on Windows.
- **CI builds all three platforms; macOS can only be built on a Mac.**
  `.github/workflows/build.yml` builds Windows, Linux (on an older Ubuntu for
  glibc compatibility) and macOS (arm64 + x86_64 merged with `lipo`), runs the
  tests on each, and attaches binaries to a release on `v*` tags. It has not been
  run yet; the first real run may need small fixes.
- **The CLI banner is ASCII, goes to stderr, and only shows on a terminal.**
  `banner.rs` output is plain 7-bit ASCII (a unit test enforces it, and that
  every line fits 79 columns) because box-drawing characters garble on legacy
  Windows code pages. It goes to **stderr** so `--json > report.json` stays
  clean, and is skipped when stderr isn't a TTY so cron/CI logs don't fill with
  art. Color is plain 16-color ANSI, off for `NO_COLOR`/`TERM=dumb`, and on
  Windows only when `WT_SESSION`/`TERM`/`ANSICON`/`ConEmuANSI` say it will
  render (classic conhost prints escapes literally). Color must only ever add
  escapes: a test checks that stripping them gives the plain banner back.
- **Logo and icon pixels are embedded as raw RGBA, not decoded at runtime.**
  `eframe` is built without the `image` decoding path on purpose; `include_bytes!`
  of `assets/icon-*.rgba` needs no new dependency. If you change any of those three, rerun
  `assets/make_icon.py` and commit the outputs, or the app and the exe will show
  a stale icon. The head's placement constants in that script are tuned so the
  artwork's cut-off neck stump is clipped away by the badge ring while the snout
  breaks out past it. Two things are derived from the art rather than drawn: the
  horns are squeezed horizontally toward their base (the original is ~12:1 and
  cannot fit the badge; fading it looked wrong), and the open-mouth region is
  detected so the stripes are hidden there and read as passing behind the head.
  The horn and body are separate layers that must be summed premultiplied
  (`make_icon.py` does this); compositing them with normal alpha-over leaves a
  hairline seam. Re-tune the constants by eye if the artwork changes; the script
  needs `numpy` and `scipy` besides `cairosvg`/`pillow`.
- **`winresource` is the one build-dependency, and it must stay fail-soft.**
  Embedding the exe icon needs a Windows resource compile. `build.rs` only uses
  it on Windows hosts and turns any failure into a `cargo:warning`, so a machine
  without `rc.exe` still builds. This is unverified on real Windows (no Windows
  machine in the sandbox) -- say so if asked.
- **Prefer hand-rolled minimal FFI over a full bindings crate** for a small,
  known set of OS calls — see `win_integration.rs` (console allocation,
  elevation, `ShellExecuteW`) and the Windows half of `disk_space.rs`
  (`GetDiskFreeSpaceExW`). `libc` is the one accepted exception, used only
  for Unix `statvfs`, because hand-declaring that struct's layout correctly
  across Linux and macOS is genuinely risk-prone in a way that declaring a
  handful of `extern "system"` function signatures isn't.
- **No new dependency without real justification.** This project has
  deliberately avoided `windows`/`winapi`, `clap`, `log`/`tracing`/
  `env_logger`, `chrono`/`time`, and — after being removed — `jwalk`, `rayon`
  and the `ntfs` crate (which also took the `binrw` future-incompatibility
  warning with it), each in favor of a
  small hand-rolled alternative (documented inline with why). Keep that bar before adding one.

## Measuring scanner memory and speed

To check a scanner change without compiling the whole GUI on a small
sandbox, copy `scanner.rs` into a scratch binary crate that depends only on
`crossbeam-channel` and `anyhow`, scan a synthetic tree (e.g. 400k files
across ~4k dirs), and read `VmHWM` from `/proc/self/status`. Two things make
the numbers meaningful instead of flattering:

- **Inject per-`stat` latency** (e.g. `thread::sleep(50µs)` at the metadata
  call, in the scratch copy only). On a hot local cache `stat` is nearly
  free, so a consumer that is serial on `stat` looks fine; the real-world
  failure only shows up when `stat` is slow (cold NTFS, antivirus hooks).
- **Simulate a GUI that doesn't poll** (sleep tens of seconds before the first
  `try_recv`) to expose snapshot pile-up.

Results from that setup for the current design vs. the old `jwalk` one:
identical counts/sizes (checked against a Python `os.walk` ground truth),
~4x faster under injected latency on a 4-worker pool, and peak RSS 57MB vs
1.26GB with a non-polling GUI.

## Testing philosophy

Anything that *can* be tested without a Windows machine, should be — pull
platform-specific logic into its own always-compiled module with real tests
(see `mft.rs`) rather than leaving it trapped inside a `#[cfg(windows)]`
file where most development environments can't verify it at all. For the
remaining Windows-only surface (`win_integration.rs` and a real raw volume
handle), rely on careful manual review against the documented OS API, not
assumption — and say so.

`cli.rs` is the one part of this project that's fully runnable end-to-end
in a plain Linux sandbox (it's cross-platform and GUI-free by design): when
touching it, actually run it against a real directory, don't just compile
it. This has caught real bugs before shipping (a progress-reporting thread
structure bug, a doubled path separator in drive-root names).

## Code style

- Comments explain *why*, not *what* — match that standard in new code.
- Prefer deleting unused code to `#[allow(dead_code)]`, except where the
  dead-code-ness is a genuine, documented platform artifact (see the
  `#[cfg_attr(not(windows), allow(dead_code))]` pattern in
  `mft.rs`, `debug_log.rs`, and `scanner::RootKind`).
- Keep user-facing messages short and plain (e.g. "Using normal scan.");
  put full diagnostic detail — real error text, panic messages — behind
  `debug_log::log()` instead of surfacing it directly in the UI.
