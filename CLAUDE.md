# CLAUDE.md — WyvernScan

This project's agent instructions live in `AGENTS.md`, kept as the single
source of truth so this file and that one don't drift out of sync. Claude
Code imports it automatically below; if you're reading this file directly
instead, open `AGENTS.md` for the module map, critical design decisions,
and the sandbox build workaround.

@AGENTS.md

## Claude Code specifics

- Before touching `mft.rs` or `win_integration.rs`, read AGENTS.md's "What
  can and can't be verified here" and "Design decisions" sections in full —
  several past changes to this codebase were straightforward-looking but
  caused real, reported bugs (a crash, a 107-second "fast" scan, a RAM
  blowup). Each is documented there with why the fix looks the way it does.
- Before touching `scanner::scan()`, read AGENTS.md's "The normal scan must
  never buffer unboundedly ahead of its consumer" decision. The ~20GB RAM bug
  there had two independent causes (an unbounded `jwalk` queue and piled-up
  live snapshots); the bounded channel, workers-do-all-syscalls split, and
  the `tx.is_empty()` snapshot guard are load-bearing. To measure a change,
  use AGENTS.md's "Measuring scanner memory and speed" recipe — and inject
  `stat` latency, or a hot cache will make a regression look fine.
- `mft.rs` is pure `std` and fully testable here, including against a real
  NTFS image made with `mkntfs` + `ntfs-3g` (recipe in AGENTS.md). Use it:
  a change to the record parser isn't verified until a real-image diff
  against an independent walker comes back identical (and a fragmented-`$MFT`
  image can be built too, and direct-I/O alignment can be checked with
  `O_DIRECT` — see AGENTS.md). Only `win_integration.rs`
  and a real Windows raw-volume handle remain unverifiable — say so.
- When verifying a change by running `cargo check`/`cargo test` in a
  sandboxed environment, use AGENTS.md's scratch-Cargo.toml pin workaround
  rather than editing the real `Cargo.toml` — committing those pins would
  break real users' builds for no reason, since they only exist to work
  around this one sandbox's outdated system Rust.
- `cli.rs` is the one module you can actually run end-to-end here (no
  display needed). Prefer doing that over reasoning from code alone when
  changing it.
- Branding lives in three places that must stay consistent: `theme.rs` (palette), `banner.rs` (CLI art) and `assets/` (icon). When changing
  the look, change `assets/icon_base.svg` / `icon_stripes.svg` / `dragon_head.png` and rerun `assets/make_icon.py` rather than
  editing the generated `.ico`/`.rgba` files, and keep the banner pure ASCII.
