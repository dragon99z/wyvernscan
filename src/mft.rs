//! Fast whole-drive scan for NTFS, the WizTree way: instead of walking
//! directories (one or more disk seeks per folder), read the volume's
//! Master File Table (`$MFT`) **front to back in big chunks** and rebuild the
//! folder tree from the parent reference stored in every file record.
//! Sequential reads are what disks are good at: an HDD streams the `$MFT`
//! at full speed instead of seeking once per folder, and an NVMe drive isn't
//! stalled by thousands of tiny dependent reads.
//!
//! The record format is small and fixed, so it's parsed here directly
//! (no `ntfs` crate): [`apply_fixups`], [`parse_record`] and [`decode_runs`]
//! are plain functions over byte slices. That also means this whole module
//! is platform-independent and has real tests on any OS, including a fully
//! synthetic NTFS volume built in memory (see the tests at the bottom).
//!
//! Reading a raw volume needs Administrator on Windows and only works on
//! NTFS. Callers treat failure as routine and fall back to `scanner::scan`;
//! anything this module can't verify (an odd cluster size, a sparse `$MFT`,
//! inconsistent extents, ...) is reported as an `Err` for exactly that reason
//! rather than guessed at. A fragmented `$MFT` — record 0 pointing at further
//! extents through an `$ATTRIBUTE_LIST`, which is normal on an old system
//! drive — *is* supported (see `locate_mft`).
//!
//! ponytail: only whole-drive-root scans use this path (`C:\`, not
//! `C:\Users\me`). Scanning a subfolder this way would mean resolving the
//! path to a record number first; until then subfolders use the normal
//! walker, which is already fast.
//!
//! **Raw volume reads must be sector-aligned** in both offset and length on
//! Windows, even without `FILE_FLAG_NO_BUFFERING` (a real bug found by a user
//! with `--debug`: `ERROR_INVALID_PARAMETER`). Every read in this module goes
//! through [`read_exact_aligned`], which rounds to 4096 (safe for both 512e
//! and 4Kn drives) and is tested against a reader that rejects anything else.
//!
//! The tree is built incrementally and **append-only** while records stream
//! in, so the fast scan has the same live view as the normal one: the GUI
//! keeps arena indices valid across snapshots only because scanners never
//! reorder or remove nodes (see `Tables`). A record whose parent directory
//! hasn't been read yet waits until it appears.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::scanner::{partial_interval_for, Node, ScanMessage, ScanResult};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// If `path` is exactly a drive root (e.g. `C:\`), return its letter.
/// Anything else (a subfolder, a UNC path, ...) returns `None` so the
/// caller falls back to the normal scanner.
pub fn drive_letter_of_root(path: &Path) -> Option<char> {
    let mut comps = path.components();
    let prefix = match comps.next()? {
        Component::Prefix(p) => p,
        _ => return None,
    };
    if !matches!(comps.next(), Some(Component::RootDir)) || comps.next().is_some() {
        return None;
    }
    match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter as char),
        _ => None,
    }
}

/// Attempt the fast MFT-based scan of a whole drive, sending progress and
/// the final result over `tx` exactly like `scanner::scan` does. Any
/// internal panic is caught here and converted into `Ok(false)` (no `Done`
/// sent), so the caller's fallback to the normal scanner runs instead of the
/// whole app going down.
///
/// Returns `Ok(true)` if a `Done` result was sent (caller should NOT also
/// run the normal scan), `Ok(false)` if it gave up cleanly and already
/// explained why via an `Info` message (caller SHOULD fall back), or `Err`
/// if it couldn't use the fast path (couldn't open the volume, not NTFS,
/// unsupported layout, I/O error — caller should fall back and may want to
/// show `e` itself).
pub fn scan_volume(
    drive_letter: char,
    tx: &crossbeam_channel::Sender<ScanMessage>,
    cancel: &AtomicBool,
) -> anyhow::Result<bool> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scan_volume_inner(drive_letter, tx, cancel)
    }));

    match outcome {
        Ok(result) => result,
        Err(payload) => {
            crate::debug_log::log(&format!(
                "MFT scan_volume top-level panic on drive {drive_letter}: {}",
                crate::debug_log::panic_message(&*payload)
            ));
            let _ = tx.send(ScanMessage::Info(
                "Fast scan hit an internal error; falling back to normal scan.".to_string(),
            ));
            let _ = tx.send(ScanMessage::Info("Using normal scan.".to_string()));
            Ok(false)
        }
    }
}

fn scan_volume_inner(
    drive_letter: char,
    tx: &crossbeam_channel::Sender<ScanMessage>,
    cancel: &AtomicBool,
) -> anyhow::Result<bool> {
    let volume_path = format!(r"\\.\{drive_letter}:");
    let root_path = PathBuf::from(format!(r"{drive_letter}:\"));
    let label = format!("{drive_letter}:");

    // First try unbuffered ("direct") reads. A buffered handle on a raw volume
    // is served through the cache in small synchronous pieces — measured at
    // ~150 MB/s on an NVMe drive that streams ~3 GB/s — whereas an unbuffered
    // 8 MiB read goes to the device as a few large I/Os. If that fails for any
    // I/O reason (a driver that rejects it, an odd sector size), retry once
    // with plain buffered reads: slower, but the same result.
    let mut file = open_volume(&volume_path, true)?;
    crate::debug_log::log(&format!("MFT: reading {volume_path} with unbuffered (direct) I/O"));
    let result = match scan_reader(&mut file, &label, root_path.clone(), tx, cancel) {
        Ok(r) => r,
        Err(e) if cfg!(windows) && e.downcast_ref::<std::io::Error>().is_some() => {
            crate::debug_log::log(&format!(
                "MFT: unbuffered read failed ({e:#}); retrying with buffered reads"
            ));
            drop(file);
            let mut file = open_volume(&volume_path, false)?;
            scan_reader(&mut file, &label, root_path, tx, cancel)?
        }
        Err(e) => return Err(e),
    };
    let _ = tx.send(ScanMessage::Done(Box::new(result)));
    Ok(true)
}

/// Open the raw volume for reading, optionally with `FILE_FLAG_NO_BUFFERING`
/// (which requires every read's offset, length and memory address to be
/// sector-aligned — see `read_exact_aligned` and `AlignedBuf`).
fn open_volume(path: &str, unbuffered: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    if unbuffered {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        options.custom_flags(FILE_FLAG_NO_BUFFERING);
    }
    #[cfg(not(windows))]
    let _ = unbuffered;
    options.open(path)
}

// ---------------------------------------------------------------------------
// Aligned reads
// ---------------------------------------------------------------------------

/// Alignment used for every volume read. 4096 satisfies both 512-byte and
/// 4Kn-native sectors; reading a little extra and slicing is cheap.
const ALIGN: u64 = 4096;

/// A zeroed heap buffer whose *address* is 4096-aligned. A handle opened with
/// `FILE_FLAG_NO_BUFFERING` (direct I/O) requires the memory address, the file
/// offset and the length of every read to be sector-aligned; an ordinary
/// `Vec<u8>` only guarantees the first of those by luck.
///
/// `len` is the number of valid bytes; `capacity` is what was allocated, so a
/// recycled buffer can serve a shorter final read.
struct AlignedBuf {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
    layout: std::alloc::Layout,
}

// SAFETY: it uniquely owns a plain byte allocation, like a `Box<[u8]>`.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let size = (len.max(1) + ALIGN as usize - 1) & !(ALIGN as usize - 1);
        let layout = std::alloc::Layout::from_size_align(size, ALIGN as usize).expect("valid layout");
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = std::ptr::NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self { ptr, len, layout }
    }

    fn capacity(&self) -> usize {
        self.layout.size()
    }

    /// Reuse this buffer for a read of `len` bytes (`len <= capacity()`).
    fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity());
        self.len = len;
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `capacity() >= len` bytes, initialised
        // (zeroed at allocation, only ever written through this slice), and
        // `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with exactly this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// Fill `out` from `offset`, but only ever call the device at 4096-aligned
/// offsets, lengths **and memory addresses**. When the request is already
/// aligned in all three (the bulk-read case) it reads straight into `out`
/// with no copy; otherwise it goes through an aligned bounce buffer.
fn read_exact_aligned<R: Read + Seek>(vol: &mut R, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
    let start = offset & !(ALIGN - 1);
    let lead = (offset - start) as usize;
    let total = (lead + out.len() + ALIGN as usize - 1) & !(ALIGN as usize - 1);
    vol.seek(SeekFrom::Start(start))?;
    if lead == 0 && total == out.len() && out.as_ptr() as usize % ALIGN as usize == 0 {
        return vol.read_exact(out);
    }
    let mut bounce = AlignedBuf::new(total);
    let tmp = bounce.as_mut_slice();
    // Rounding up can run past the end of the device; that's fine as long as
    // everything actually requested was there.
    let mut got = 0;
    while got < total {
        match vol.read(&mut tmp[got..])? {
            0 => break,
            n => {
                got += n;
                // An unaligned count means we hit the end of the device; a
                // follow-up read would itself be unaligned, so stop here.
                if got % ALIGN as usize != 0 {
                    break;
                }
            }
        }
    }
    if got < lead + out.len() {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    out.copy_from_slice(&tmp[lead..lead + out.len()]);
    Ok(())
}

// ---------------------------------------------------------------------------
// On-disk structure parsing (pure functions over byte slices)
// ---------------------------------------------------------------------------

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..)?.get(..2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..)?.get(..4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..)?.get(..8)?.try_into().ok()?))
}

/// A file reference is a 48-bit record number plus a 16-bit sequence number.
const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Record number of the root directory in every NTFS volume.
const ROOT_RECORD: usize = 5;

const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

struct Boot {
    cluster: u64,
    record_size: usize,
    mft_offset: u64,
}

fn parse_boot(sector: &[u8]) -> Option<Boot> {
    if sector.get(3..11)? != b"NTFS    " {
        return None;
    }
    let bytes_per_sector = u64::from(u16_at(sector, 0x0B)?);
    let sectors_per_cluster = u64::from(*sector.get(0x0D)?);
    // Values above 0x80 encode cluster sizes of 2^n bytes for huge clusters;
    // far too rare to be worth supporting — the caller falls back instead.
    if bytes_per_sector < 512 || !(1..=0x80).contains(&sectors_per_cluster) {
        return None;
    }
    let cluster = bytes_per_sector * sectors_per_cluster;
    let mft_lcn = u64_at(sector, 0x30)?;
    // Positive: clusters per record. Negative: record size is 2^(-n) bytes.
    let rec = *sector.get(0x40)? as i8;
    let record_size = if rec > 0 {
        cluster * rec as u64
    } else {
        1u64.checked_shl(u32::from(rec.unsigned_abs()))?
    };
    if !(512..=65536).contains(&record_size) || record_size % 512 != 0 {
        return None;
    }
    Some(Boot { cluster, record_size: record_size as usize, mft_offset: mft_lcn.checked_mul(cluster)? })
}

/// Undo NTFS's "update sequence" protection: the last two bytes of every
/// 512-byte block of a record were overwritten with a check value, and the
/// real bytes parked in an array in the header. Returns false if the record
/// is torn or not a `FILE` record at all.
fn apply_fixups(rec: &mut [u8]) -> bool {
    fn go(rec: &mut [u8]) -> Option<()> {
        if rec.get(0..4)? != b"FILE" {
            return None;
        }
        let usa_off = u16_at(rec, 4)? as usize;
        let usa_count = u16_at(rec, 6)? as usize;
        let blocks = usa_count.checked_sub(1)?;
        if blocks == 0 || blocks * 512 > rec.len() || usa_off.checked_add(usa_count * 2)? > rec.len() {
            return None;
        }
        let usn = [rec[usa_off], rec[usa_off + 1]];
        for i in 0..blocks {
            let end = (i + 1) * 512;
            if rec[end - 2..end] != usn {
                return None;
            }
            let saved = usa_off + 2 + i * 2;
            rec[end - 2] = rec[saved];
            rec[end - 1] = rec[saved + 1];
        }
        Some(())
    }
    go(rec).is_some()
}

struct NameRef {
    parent: u64,
    namespace: u8,
    /// Byte offset and length (in UTF-16 units) of the name inside the record.
    off: usize,
    chars: usize,
}

/// What one (fixed-up) file record contributes. Reused between records.
#[derive(Default)]
struct Parsed {
    is_dir: bool,
    /// The raw base file reference: 0 for a base record, otherwise the base
    /// record's number plus a sequence number. Kept unmasked because an
    /// extension record of `$MFT` itself references record 0 — which only
    /// differs from "no base" by its non-zero sequence number.
    base: u64,
    /// Unnamed `$DATA` size, if this record holds the stream's first extent.
    size: Option<u64>,
    /// The record has an `$ATTRIBUTE_LIST`, so some of its attributes (names,
    /// data) may live in extension records and its sizes aren't final yet.
    has_attr_list: bool,
    names: Vec<NameRef>,
}

/// Parse a fixed-up record. `None` means structurally corrupt.
fn parse_record(rec: &[u8], out: &mut Parsed) -> Option<()> {
    out.names.clear();
    out.size = None;
    out.has_attr_list = false;
    let flags = u16_at(rec, 0x16)?;
    out.is_dir = flags & 0x02 != 0;
    out.base = u64_at(rec, 0x20)?;
    let used = (u32_at(rec, 0x18)? as usize).min(rec.len());
    let mut off = u16_at(rec, 0x14)? as usize;
    loop {
        let ty = u32_at(rec, off)?;
        if ty == ATTR_END {
            return Some(());
        }
        let len = u32_at(rec, off + 4)? as usize;
        if len < 24 || off.checked_add(len)? > used {
            return None;
        }
        let non_resident = *rec.get(off + 8)? != 0;
        let name_len = *rec.get(off + 9)?;
        match ty {
            ATTR_FILE_NAME if !non_resident => {
                let vlen = u32_at(rec, off + 16)? as usize;
                let voff = off + u16_at(rec, off + 20)? as usize;
                let v = rec.get(voff..voff.checked_add(vlen)?)?;
                let chars = *v.get(64)? as usize;
                if v.get(66..66 + chars * 2).is_some() {
                    out.names.push(NameRef {
                        parent: u64_at(v, 0)? & REF_MASK,
                        namespace: *v.get(65)?,
                        off: voff + 66,
                        chars,
                    });
                }
            }
            ATTR_ATTRIBUTE_LIST => out.has_attr_list = true,
            ATTR_DATA if name_len == 0 => {
                if !non_resident {
                    out.size = Some(u64::from(u32_at(rec, off + 16)?));
                } else if u64_at(rec, off + 16)? == 0 {
                    out.size = Some(u64_at(rec, off + 48)?);
                }
            }
            _ => {}
        }
        off += len;
    }
}

#[derive(Debug, PartialEq, Clone)]
struct Run {
    /// `None` is a sparse run (no clusters on disk).
    lcn: Option<u64>,
    len: u64,
}

/// Decode an NTFS runlist ("mapping pairs"): a compact list of
/// (length, signed delta to the previous run's start cluster).
fn decode_runs(b: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut i = 0;
    let mut lcn: i64 = 0;
    while let Some(&header) = b.get(i) {
        if header == 0 {
            break;
        }
        i += 1;
        let (len_bytes, off_bytes) = ((header & 0x0F) as usize, (header >> 4) as usize);
        if len_bytes == 0 || len_bytes > 8 || off_bytes > 8 {
            return None;
        }
        let len = b.get(i..i + len_bytes)?.iter().rev().fold(0u64, |a, &x| (a << 8) | u64::from(x));
        i += len_bytes;
        if off_bytes == 0 {
            runs.push(Run { lcn: None, len });
            continue;
        }
        let raw = b.get(i..i + off_bytes)?.iter().rev().fold(0u64, |a, &x| (a << 8) | u64::from(x));
        i += off_bytes;
        let shift = 64 - 8 * off_bytes as u32;
        let delta = ((raw << shift) as i64) >> shift; // sign-extend
        lcn = lcn.checked_add(delta)?;
        if lcn < 0 {
            return None;
        }
        runs.push(Run { lcn: Some(lcn as u64), len });
    }
    Some(runs)
}

/// Positions of every attribute in a record: `(type, offset, length)`.
fn attr_positions(rec: &[u8]) -> anyhow::Result<Vec<(u32, usize, usize)>> {
    let corrupt = || anyhow::anyhow!("corrupt $MFT record");
    let mut out = Vec::new();
    let mut off = u16_at(rec, 0x14).ok_or_else(corrupt)? as usize;
    loop {
        let ty = u32_at(rec, off).ok_or_else(corrupt)?;
        if ty == ATTR_END {
            return Ok(out);
        }
        let len = u32_at(rec, off + 4).ok_or_else(corrupt)? as usize;
        if len < 24 || off.checked_add(len).ok_or_else(corrupt)? > rec.len() {
            return Err(corrupt());
        }
        out.push((ty, off, len));
        off += len;
    }
}

/// One non-resident attribute extent: the first virtual cluster it covers,
/// the stream's real size (only meaningful when `lowest_vcn == 0`), and where
/// its clusters are.
#[derive(Clone)]
struct Extent {
    lowest_vcn: u64,
    size: u64,
    runs: Vec<Run>,
}

fn extent_at(rec: &[u8], off: usize, len: usize) -> anyhow::Result<Extent> {
    let bad = || anyhow::anyhow!("corrupt non-resident attribute in $MFT metadata");
    let lowest_vcn = u64_at(rec, off + 16).ok_or_else(bad)?;
    let size = u64_at(rec, off + 48).ok_or_else(bad)?;
    let run_off = off + u16_at(rec, off + 32).ok_or_else(bad)? as usize;
    let runs = rec.get(run_off..off + len).and_then(decode_runs).ok_or_else(bad)?;
    Ok(Extent { lowest_vcn, size, runs })
}

/// The content of an attribute (the `$ATTRIBUTE_LIST`, here), whether it is
/// stored inside the record or out in clusters of its own.
fn attribute_value<R: Read + Seek>(
    vol: &mut R,
    cluster: u64,
    rec: &[u8],
    off: usize,
    len: usize,
) -> anyhow::Result<Vec<u8>> {
    const MAX: u64 = 64 << 20; // a sanity bound; real lists are kilobytes
    let bad = || anyhow::anyhow!("corrupt $MFT attribute list");
    if rec.get(off + 8) == Some(&0) {
        let vlen = u32_at(rec, off + 16).ok_or_else(bad)? as usize;
        let voff = off + u16_at(rec, off + 20).ok_or_else(bad)? as usize;
        return Ok(rec.get(voff..voff.checked_add(vlen).ok_or_else(bad)?).ok_or_else(bad)?.to_vec());
    }
    let ext = extent_at(rec, off, len)?;
    if ext.size > MAX {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(ext.size as usize);
    for run in &ext.runs {
        let Some(lcn) = run.lcn else {
            return Err(bad());
        };
        let want = run.len.checked_mul(cluster).ok_or_else(bad)?.min(ext.size - out.len() as u64) as usize;
        let mut buf = vec![0u8; want];
        read_exact_aligned(vol, lcn.checked_mul(cluster).ok_or_else(bad)?, &mut buf)?;
        out.extend_from_slice(&buf);
        if out.len() as u64 >= ext.size {
            break;
        }
    }
    if (out.len() as u64) < ext.size {
        return Err(bad());
    }
    Ok(out)
}

/// Read one `$MFT` record using only the extents already known (needed to
/// fetch the extension records that describe the rest of the `$MFT`).
fn read_mapped_record<R: Read + Seek>(
    vol: &mut R,
    boot: &Boot,
    runs: &[Run],
    recno: u64,
) -> anyhow::Result<Vec<u8>> {
    let rs = boot.record_size as u64;
    let byte = recno.checked_mul(rs).ok_or_else(|| anyhow::anyhow!("bad record number"))?;
    let mut vcn = 0u64;
    for run in runs {
        let (run_start, run_end) = (vcn * boot.cluster, (vcn + run.len) * boot.cluster);
        if byte >= run_start && byte < run_end {
            if byte + rs > run_end {
                anyhow::bail!("$MFT record {recno} straddles two extents (unsupported)");
            }
            let lcn = run.lcn.ok_or_else(|| anyhow::anyhow!("$MFT has a sparse extent (unsupported)"))?;
            let mut rec = vec![0u8; rs as usize];
            read_exact_aligned(vol, lcn * boot.cluster + (byte - run_start), &mut rec)?;
            if !apply_fixups(&mut rec) {
                anyhow::bail!("$MFT extension record {recno} failed validation");
            }
            return Ok(rec);
        }
        vcn += run.len;
    }
    anyhow::bail!("$MFT extension record {recno} lies outside the extents read so far")
}

/// From record 0 (`$MFT` itself): the size and on-disk extents of the whole
/// table. Usually that is just record 0's own `$DATA`. But on a drive whose
/// `$MFT` has grown in many pieces, record 0 can't hold all the extents: it
/// carries an `$ATTRIBUTE_LIST` naming further extension records, each holding
/// one more extent of the same stream. Those are read here, in virtual-cluster
/// order, each located through the extents already known — the same bootstrap
/// the NTFS driver itself performs.
fn locate_mft<R: Read + Seek>(
    vol: &mut R,
    boot: &Boot,
    rec0: &[u8],
) -> anyhow::Result<(u64, Vec<Run>, Option<Vec<u8>>)> {
    let mut own = None;
    let mut list = None;
    let mut bitmap = None;
    for (ty, off, len) in attr_positions(rec0)? {
        match ty {
            ATTR_DATA if rec0.get(off + 9) == Some(&0) => {
                if rec0.get(off + 8) == Some(&0) {
                    anyhow::bail!("$MFT data is resident (not a real volume)");
                }
                own = Some(extent_at(rec0, off, len)?);
            }
            ATTR_ATTRIBUTE_LIST => list = Some(attribute_value(vol, boot.cluster, rec0, off, len)?),
            // Which records are in use, one bit each. Only an optimisation (it
            // lets the scan skip free records), so any trouble reading it just
            // means "read everything" rather than failing the scan.
            ATTR_BITMAP if rec0.get(off + 9) == Some(&0) => {
                let starts_at_zero = rec0.get(off + 8) == Some(&0) || u64_at(rec0, off + 16) == Some(0);
                if starts_at_zero {
                    bitmap = attribute_value(vol, boot.cluster, rec0, off, len).ok();
                }
            }
            _ => {}
        }
    }
    let own = own.ok_or_else(|| anyhow::anyhow!("$MFT record has no data attribute"))?;
    let Some(list) = list else {
        if own.lowest_vcn != 0 {
            anyhow::bail!("$MFT data attribute does not start at cluster 0");
        }
        return Ok((own.size, own.runs, bitmap));
    };

    // Attribute list entries: type u32, entry length u16, name length u8 @6,
    // starting VCN u64 @8, file reference u64 @16. Keep the unnamed $DATA ones.
    let mut wanted: Vec<(u64, u64)> = Vec::new();
    let mut p = 0;
    while let (Some(ty), Some(entry_len)) = (u32_at(&list, p), u16_at(&list, p + 4)) {
        let entry_len = entry_len as usize;
        if entry_len < 26 || p + entry_len > list.len() {
            break;
        }
        if ty == ATTR_DATA && list[p + 6] == 0 {
            if let (Some(vcn), Some(fref)) = (u64_at(&list, p + 8), u64_at(&list, p + 16)) {
                wanted.push((vcn, fref & REF_MASK));
            }
        }
        p += entry_len;
    }
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.is_empty() {
        anyhow::bail!("$MFT attribute list names no data extents");
    }

    let mut runs: Vec<Run> = Vec::new();
    let mut clusters = 0u64;
    let mut size = None;
    for &(vcn, recno) in &wanted {
        let ext = if recno == 0 {
            if own.lowest_vcn != vcn {
                anyhow::bail!("$MFT attribute list and record 0 disagree about extent {vcn}");
            }
            own.clone()
        } else {
            let rec = read_mapped_record(vol, boot, &runs, recno)?;
            let mut found = None;
            for (ty, off, len) in attr_positions(&rec)? {
                if ty == ATTR_DATA && rec.get(off + 9) == Some(&0) && rec.get(off + 8) == Some(&1) {
                    let e = extent_at(&rec, off, len)?;
                    if e.lowest_vcn == vcn {
                        found = Some(e);
                        break;
                    }
                }
            }
            found.ok_or_else(|| anyhow::anyhow!("$MFT extension record {recno} has no data extent at cluster {vcn}"))?
        };
        if clusters != vcn {
            anyhow::bail!("$MFT extents are not contiguous: expected cluster {clusters}, attribute list says {vcn}");
        }
        if vcn == 0 {
            size = Some(ext.size);
        }
        clusters += ext.runs.iter().map(|r| r.len).sum::<u64>();
        runs.extend(ext.runs);
    }
    let size = size.ok_or_else(|| anyhow::anyhow!("$MFT attribute list has no extent at cluster 0"))?;
    Ok((size, runs, bitmap))
}

// ---------------------------------------------------------------------------
// Sequential read of the $MFT
// ---------------------------------------------------------------------------

/// Bytes per read. Big enough to keep a disk streaming, small enough that the
/// couple of buffers in flight stay trivial next to the tree itself.
const CHUNK: usize = 8 << 20;

/// What the reader thread spent: bytes read and time inside the read calls.
#[derive(Default)]
struct ReadStats {
    bytes: u64,
    nanos: u64,
}

/// A piece of the `$MFT` byte stream and where in the stream it starts.
struct Chunk {
    buf: AlignedBuf,
    stream_off: u64,
}

/// Skipping a gap of free records saves reading it but costs a seek, which
/// on a spinning disk is worth roughly 1 MiB of sequential transfer; gaps
/// smaller than this are simply read through.
const MIN_GAP_BYTES: u64 = 1 << 20;

/// The byte ranges of the `$MFT` stream that contain at least one in-use
/// record, as given by its `$BITMAP` (bit `i` of byte `i / 8` = record `i`).
/// A drive that once held a huge, now-deleted tree — a removed venv or
/// `node_modules` — keeps all those free records inside the `$MFT` forever;
/// reading them is pure wasted I/O that also yields no names, so the progress
/// display appears to freeze. With no usable bitmap the whole table is read.
fn needed_ranges(bitmap: Option<&[u8]>, records: usize, record_size: u64, min_gap: u64) -> Vec<(u64, u64)> {
    let whole = || vec![(0, records as u64 * record_size)];
    let Some(bits) = bitmap else {
        return whole();
    };
    if bits.len() * 8 < records {
        return whole();
    }
    let mut ranges: Vec<(u64, u64)> = Vec::new(); // in records, [start, end)
    let mut i = 0usize;
    while i < records {
        let byte = bits[i / 8];
        if byte == 0 && i % 8 == 0 {
            i += 8;
            continue;
        }
        if byte & (1 << (i % 8)) != 0 {
            let r = i as u64;
            match ranges.last_mut() {
                Some(last) if r - last.1 < min_gap => last.1 = r + 1,
                _ => ranges.push((r, r + 1)),
            }
        }
        i += 1;
    }
    if ranges.is_empty() {
        return whole(); // an all-free bitmap can't be right (record 0 is $MFT itself)
    }
    // Round each range outwards to the read alignment (so no range edge forces
    // a bounce-buffer read) and merge ranges that now touch.
    let total = records as u64 * record_size;
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (s, e) in ranges {
        let start = (s * record_size) & !(ALIGN - 1);
        let end = ((e * record_size + ALIGN - 1) & !(ALIGN - 1)).min(total);
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Stream the given byte `ranges` of the `$MFT` (ascending, record-aligned) as
/// aligned buffers of up to `chunk` bytes, each tagged with its position. A
/// record may straddle two buffers; the consumer stitches those together.
/// Finished buffers come back through `pool` to be reused, which avoids
/// faulting in gigabytes of fresh memory over a large scan.
fn stream_mft<R: Read + Seek>(
    vol: &mut R,
    runs: &[Run],
    cluster: u64,
    ranges: &[(u64, u64)],
    chunk: usize,
    pool: &crossbeam_channel::Receiver<AlignedBuf>,
    tx: &crossbeam_channel::Sender<Chunk>,
) -> anyhow::Result<ReadStats> {
    // Where each extent sits in the stream and on the disk (None = sparse).
    let mut table: Vec<(u64, u64, Option<u64>)> = Vec::with_capacity(runs.len());
    let mut stream_start = 0u64;
    for run in runs {
        let bytes = run.len.checked_mul(cluster).ok_or_else(|| anyhow::anyhow!("bad runlist"))?;
        let disk = match run.lcn {
            Some(lcn) => Some(lcn.checked_mul(cluster).ok_or_else(|| anyhow::anyhow!("bad runlist"))?),
            None => None,
        };
        table.push((stream_start, bytes, disk));
        stream_start += bytes;
    }
    if ranges.last().map_or(false, |&(_, end)| end > stream_start) {
        anyhow::bail!("$MFT runlist is shorter than its data size");
    }

    let mut stats = ReadStats::default();
    let mut seg = 0usize;
    for &(range_start, range_end) in ranges {
        let mut pos = range_start;
        while pos < range_end {
            while table.get(seg).map_or(false, |&(s, len, _)| s + len <= pos) {
                seg += 1;
            }
            let &(seg_start, seg_len, disk) = table.get(seg).ok_or_else(|| anyhow::anyhow!("bad runlist"))?;
            let disk = disk.ok_or_else(|| anyhow::anyhow!("$MFT has a sparse extent (unsupported)"))?;
            let n = (chunk as u64).min(range_end - pos).min(seg_start + seg_len - pos) as usize;
            let mut buf = match pool.try_recv() {
                Ok(mut b) if b.capacity() >= n => {
                    b.set_len(n);
                    b
                }
                _ => AlignedBuf::new(n),
            };
            let began = Instant::now();
            read_exact_aligned(vol, disk + (pos - seg_start), buf.as_mut_slice())?;
            let took = began.elapsed();
            if took >= Duration::from_millis(500) {
                // A stall detector: one read that takes this long is the kind
                // of thing that makes a scan look frozen.
                crate::debug_log::log(&format!(
                    "MFT: slow read: {n} bytes at $MFT offset {pos} (disk offset {}) took {:.1}s",
                    disk + (pos - seg_start),
                    took.as_secs_f64()
                ));
            }
            stats.nanos += took.as_nanos() as u64;
            stats.bytes += n as u64;
            if tx.send(Chunk { buf, stream_off: pos }).is_err() {
                return Ok(stats); // consumer stopped (cancelled)
            }
            pos += n as u64;
        }
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Collecting records and rebuilding the tree
// ---------------------------------------------------------------------------

const IN_USE: u8 = 1;
const IS_DIR: u8 = 2;
const NONE: u32 = u32::MAX;

struct Entry {
    /// Record the name belongs to (the base record, even for names that
    /// physically live in an extension record).
    rec: u32,
    parent: u32,
    name_off: u32,
    name_len: u16,
    /// Next entry waiting for the same parent directory (`NONE` = end).
    next: u32,
}

/// Everything learned from the `$MFT` so far, including the tree built from it.
///
/// The tree is built **incrementally and append-only**, as records stream in:
/// the GUI keeps arena indices (what's expanded, where the user is looking)
/// valid across live snapshots precisely because scanners only ever append.
/// A name whose parent directory is already in the tree is attached at once;
/// otherwise it waits on a per-parent list (threaded through `Entry::next`)
/// and is attached the moment that directory shows up. A parent therefore
/// always has a lower arena index than its children, which lets sizes be
/// summed in a single reverse pass over any snapshot.
struct Tables {
    next_rec: usize,
    flags: Vec<u8>,
    /// Unnamed `$DATA` size per record.
    size: Vec<u64>,
    entries: Vec<Entry>,
    pool: String,
    errors: u64,
    bytes_seen: u64,

    arena: Vec<Node>,
    /// Record number -> arena index of the directory's node, once attached.
    dir_node: Vec<u32>,
    /// Record number -> head of the list of entries waiting for that directory.
    waiting: Vec<u32>,
    /// Scratch worklist for `attach`, kept to avoid an allocation per entry.
    work: Vec<(u32, u32)>,
    /// Entries that depend on extension records (names or sizes that may not
    /// be complete yet); attached at the very end, once everything is known.
    deferred: Vec<u32>,
    file_count: u64,
    dir_count: u64,
}

impl Tables {
    fn new(records: usize, label: &str) -> Self {
        let mut dir_node = vec![NONE; records];
        dir_node[ROOT_RECORD] = 0;
        Self {
            next_rec: 0,
            flags: vec![0; records],
            size: vec![0; records],
            entries: Vec::new(),
            pool: String::new(),
            errors: 0,
            bytes_seen: 0,
            arena: vec![Node {
                name: Arc::from(label),
                is_dir: true,
                size: 0,
                parent: None,
                children: Vec::new(),
                abs_path: None,
            }],
            dir_node,
            waiting: vec![NONE; records],
            work: Vec::new(),
            deferred: Vec::new(),
            file_count: 0,
            dir_count: 1,
        }
    }

    /// Count (and, under `--debug`, explain) a record that couldn't be used.
    fn bad(&mut self, num: usize, why: &str) {
        self.errors += 1;
        if crate::debug_log::is_enabled() {
            crate::debug_log::log_item(&format!("MFT record {num}: {why}"));
        }
    }

    /// Parse every record in `buf` (a whole number of records, in order).
    fn ingest(&mut self, buf: &mut [u8], record_size: usize, parsed: &mut Parsed) {
        let n = self.flags.len();
        for rec in buf.chunks_exact_mut(record_size) {
            let num = self.next_rec;
            self.next_rec += 1;
            if num >= n {
                return;
            }
            // Unused slots are usually all zero: not an error, just skip.
            if rec[..4] != *b"FILE" || u16_at(rec, 0x16).map_or(true, |f| f & 1 == 0) {
                continue;
            }
            if !apply_fixups(rec) {
                self.bad(num, "failed the update-sequence check (torn or corrupt record)");
                continue;
            }
            if parse_record(rec, parsed).is_none() {
                self.bad(num, "corrupt attribute data");
                continue;
            }
            let is_ext = parsed.base != 0;
            let owner = if is_ext { (parsed.base & REF_MASK) as usize } else { num };
            if owner >= n {
                self.bad(num, "extension record points outside the $MFT");
                continue;
            }
            if !is_ext {
                self.flags[num] = IN_USE | if parsed.is_dir { IS_DIR } else { 0 };
            }
            if let Some(sz) = parsed.size {
                self.size[owner] = sz;
                if !is_ext && !parsed.is_dir {
                    self.bytes_seen += sz;
                }
            }
            for nm in &parsed.names {
                // The 8.3 "DOS" alias is a second name for the same file;
                // skip it so nothing is counted twice. The root's own "."
                // name points at itself.
                if nm.namespace == 2 || owner == ROOT_RECORD || nm.parent >= n as u64 {
                    continue;
                }
                let units = rec[nm.off..nm.off + nm.chars * 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]));
                let name_off = self.pool.len();
                for ch in char::decode_utf16(units) {
                    self.pool.push(ch.unwrap_or('\u{FFFD}'));
                }
                let name_len = self.pool.len() - name_off;
                let (Ok(name_off), Ok(name_len), Ok(idx)) = (
                    u32::try_from(name_off),
                    u16::try_from(name_len),
                    u32::try_from(self.entries.len()),
                ) else {
                    self.bad(num, "name table overflow");
                    continue;
                };
                self.entries.push(Entry { rec: owner as u32, parent: nm.parent as u32, name_off, name_len, next: NONE });
                if is_ext || parsed.has_attr_list {
                    self.deferred.push(idx);
                } else {
                    self.attach(idx);
                }
            }
        }
    }

    /// Put an entry into the tree if its parent directory is already there,
    /// otherwise park it until that directory arrives. Attaching a directory
    /// releases everything that was waiting for it, iteratively.
    fn attach(&mut self, entry: u32) {
        let parent = self.entries[entry as usize].parent as usize;
        let parent_node = self.dir_node[parent];
        if parent_node == NONE {
            self.entries[entry as usize].next = self.waiting[parent];
            self.waiting[parent] = entry;
            return;
        }
        let mut work = std::mem::take(&mut self.work);
        work.push((entry, parent_node));
        while let Some((ei, parent_idx)) = work.pop() {
            let e = &self.entries[ei as usize];
            let (rec, name_off, name_len) = (e.rec as usize, e.name_off as usize, e.name_len as usize);
            let is_dir = self.flags[rec] & IS_DIR != 0;
            let idx = self.arena.len();
            self.arena.push(Node {
                name: Arc::from(&self.pool[name_off..name_off + name_len]),
                is_dir,
                size: if is_dir { 0 } else { self.size[rec] },
                parent: Some(parent_idx as usize),
                children: Vec::new(),
                abs_path: None,
            });
            self.arena[parent_idx as usize].children.push(idx);
            if !is_dir {
                self.file_count += 1;
                continue;
            }
            self.dir_count += 1;
            // A directory listed under two names is only expanded once.
            if self.dir_node[rec] == NONE {
                self.dir_node[rec] = idx as u32;
                let mut w = std::mem::replace(&mut self.waiting[rec], NONE);
                while w != NONE {
                    work.push((w, idx as u32));
                    w = self.entries[w as usize].next;
                }
            }
        }
        self.work = work;
    }

    /// Directory sizes are the sum of everything below them. Valid in one
    /// reverse pass because a parent always precedes its children.
    fn aggregate(arena: &mut [Node]) {
        for idx in (1..arena.len()).rev() {
            if let Some(parent) = arena[idx].parent {
                let size = arena[idx].size;
                arena[parent].size += size;
            }
        }
    }

    /// A point-in-time copy of the tree for the live view.
    fn snapshot(&self) -> (Vec<Node>, u64, u64) {
        let mut arena = self.arena.clone();
        Self::aggregate(&mut arena);
        (arena, self.file_count, self.dir_count)
    }

    /// Attach the deferred entries (their extension records have all been
    /// read by now) and return the finished tree.
    fn finish(mut self) -> (Vec<Node>, u64, u64) {
        for ei in std::mem::take(&mut self.deferred) {
            if self.flags[self.entries[ei as usize].rec as usize] & IN_USE != 0 {
                self.attach(ei);
            }
        }
        Self::aggregate(&mut self.arena);
        (self.arena, self.file_count, self.dir_count)
    }
}

/// Feed one buffer of the `$MFT` byte stream to the parser. Buffers are cut
/// wherever the reader's extents and chunk size happen to fall, so a record
/// can straddle two of them: the unfinished tail is kept in `carry` and
/// completed from the head of the next buffer.
fn feed(tables: &mut Tables, carry: &mut Vec<u8>, mut data: &mut [u8], record_size: usize, parsed: &mut Parsed) {
    if !carry.is_empty() {
        let need = record_size - carry.len();
        if data.len() < need {
            carry.extend_from_slice(data);
            return;
        }
        carry.extend_from_slice(&data[..need]);
        tables.ingest(carry, record_size, parsed);
        carry.clear();
        data = &mut data[need..];
    }
    let whole = data.len() / record_size * record_size;
    let (body, tail) = data.split_at_mut(whole);
    tables.ingest(body, record_size, parsed);
    carry.extend_from_slice(tail);
}

/// The knobs of a scan that production never changes but tests do.
struct Tuning {
    /// Bytes per read.
    chunk: usize,
    /// `Some(..)` fixes the live-snapshot interval and also skips the "GUI has
    /// drained the channel" guard, so snapshots are deterministic in tests.
    partial_every: Option<Duration>,
    /// Free gaps shorter than this are read through rather than skipped.
    min_gap_bytes: u64,
    /// How long the scan may wait on the disk before telling the user that the
    /// drive is slow (so a stretch with no new names doesn't look like a hang).
    slow_after: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { chunk: CHUNK, partial_every: None, min_gap_bytes: MIN_GAP_BYTES, slow_after: Duration::from_millis(1500) }
    }
}

fn send_progress(tx: &crossbeam_channel::Sender<ScanMessage>, tables: &Tables) {
    let current = tables
        .entries
        .last()
        .map(|e| tables.pool[e.name_off as usize..e.name_off as usize + e.name_len as usize].to_string())
        .unwrap_or_default();
    let _ = tx.send(ScanMessage::Progress {
        files_seen: tables.entries.len() as u64,
        current,
        bytes_seen: tables.bytes_seen,
    });
}

/// Fast-scan an NTFS volume read through `vol`. Sends `Info`/`Progress`
/// messages on `tx` and returns the finished result (the caller sends `Done`).
fn scan_reader<R: Read + Seek + Send>(
    vol: &mut R,
    label: &str,
    root_path: PathBuf,
    tx: &crossbeam_channel::Sender<ScanMessage>,
    cancel: &AtomicBool,
) -> anyhow::Result<ScanResult> {
    scan_reader_tuned(vol, label, root_path, tx, cancel, &Tuning::default())
}

fn scan_reader_tuned<R: Read + Seek + Send>(
    vol: &mut R,
    label: &str,
    root_path: PathBuf,
    tx: &crossbeam_channel::Sender<ScanMessage>,
    cancel: &AtomicBool,
    tuning: &Tuning,
) -> anyhow::Result<ScanResult> {
    use crossbeam_channel::RecvTimeoutError;
    let start = Instant::now();
    crate::debug_log::reset_item_budget();

    let mut sector = vec![0u8; ALIGN as usize];
    read_exact_aligned(vol, 0, &mut sector)?;
    let boot = parse_boot(&sector).ok_or_else(|| anyhow::anyhow!("not an NTFS volume (or an unsupported layout)"))?;
    let _ = tx.send(ScanMessage::Info("Using fast scan.".to_string()));

    let mut rec0 = vec![0u8; boot.record_size];
    read_exact_aligned(vol, boot.mft_offset, &mut rec0)?;
    if !apply_fixups(&mut rec0) {
        anyhow::bail!("$MFT record 0 failed validation");
    }
    let (data_size, runs, bitmap) = locate_mft(vol, &boot, &rec0)?;
    let record_size = boot.record_size;
    let records = usize::try_from(data_size / record_size as u64)?;
    if records <= ROOT_RECORD || records >= u32::MAX as usize {
        anyhow::bail!("implausible $MFT size ({data_size} bytes)");
    }

    // Which parts of the table actually need reading.
    let min_gap = (tuning.min_gap_bytes / record_size as u64).max(1);
    let ranges = needed_ranges(bitmap.as_deref(), records, record_size as u64, min_gap);
    let total_bytes = records as u64 * record_size as u64;
    let read_bytes: u64 = ranges.iter().map(|&(s, e)| e - s).sum();
    let mib = |b: u64| b >> 20;
    crate::debug_log::log(&format!(
        "MFT: {records} records of {record_size} bytes in {} extent(s), cluster size {}",
        runs.len(),
        boot.cluster
    ));
    match &bitmap {
        Some(bits) if bits.len() * 8 >= records => {
            let in_use: u64 = (0..records).filter(|&i| bits[i / 8] & (1 << (i % 8)) != 0).count() as u64;
            crate::debug_log::log(&format!(
                "MFT: {in_use} of {records} records in use ({:.0}% of the table is free); reading {} MiB of {} MiB in {} range(s), skipping {} MiB",
                (records as u64 - in_use) as f64 * 100.0 / records as f64,
                mib(read_bytes),
                mib(total_bytes),
                ranges.len(),
                mib(total_bytes - read_bytes),
            ));
        }
        _ => crate::debug_log::log("MFT: no usable $BITMAP; reading the whole table"),
    }

    let mut tables = Tables::new(records, label);
    let mut parsed = Parsed::default();
    let mut last_sent = Instant::now();
    let mut last_partial = Instant::now();
    let mut cancelled = false;

    // A reader thread keeps the disk streaming while this thread parses.
    let (chunk_tx, chunk_rx) = crossbeam_channel::bounded::<Chunk>(2);
    let (pool_tx, pool_rx) = crossbeam_channel::unbounded::<AlignedBuf>();
    let (runs_ref, ranges_ref, cluster, chunk) = (&runs, &ranges, boot.cluster, tuning.chunk);
    let (mut waited, mut parsing, mut snapshotting) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let mut snapshots = 0u32;
    let mut carry: Vec<u8> = Vec::new();
    let mut expected_off = 0u64;
    let mut last_data = Instant::now();
    let mut told_slow = false;
    let reader_result = std::thread::scope(|scope| {
        // `chunk_tx` moves into the thread, so the channel closes when it ends.
        let reader = scope
            .spawn(move || stream_mft(vol, runs_ref, cluster, ranges_ref, chunk, &pool_rx, &chunk_tx));
        loop {
            // Wait for the next piece, but keep the UI alive (progress, timer,
            // cancel) while the disk is slow: a stretch with no new names must
            // never look like a freeze.
            let began = Instant::now();
            let received = chunk_rx.recv_timeout(Duration::from_millis(60));
            waited += began.elapsed();
            let mut piece = match received {
                Ok(piece) => piece,
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::Relaxed) {
                        cancelled = true;
                        break;
                    }
                    if !told_slow && last_data.elapsed() >= tuning.slow_after {
                        told_slow = true;
                        let _ = tx.send(ScanMessage::Info(
                            "Waiting on the drive: it is slow to read part of the disk (not a WyvernScan problem).".to_string(),
                        ));
                    }
                    if last_sent.elapsed() >= Duration::from_millis(60) {
                        send_progress(tx, &tables);
                        last_sent = Instant::now();
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            last_data = Instant::now();
            if told_slow {
                told_slow = false;
                let _ = tx.send(ScanMessage::Info("Using fast scan.".to_string()));
            }
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            // A gap (skipped free records) means this isn't the next byte of
            // the stream: restart record numbering at the new position.
            if piece.stream_off != expected_off {
                carry.clear();
                tables.next_rec = (piece.stream_off / record_size as u64) as usize;
            }
            let len = piece.buf.as_mut_slice().len() as u64;
            expected_off = piece.stream_off + len;

            let began = Instant::now();
            feed(&mut tables, &mut carry, piece.buf.as_mut_slice(), record_size, &mut parsed);
            parsing += began.elapsed();
            let _ = pool_tx.send(piece.buf);

            // Live view, like the normal scan: a copy of the tree built so
            // far (records whose parent directory hasn't been read yet simply
            // aren't in it yet). Same pacing, and same rule of only taking
            // one when the GUI has drained everything sent so far, so a
            // window that isn't polling can't pile up full-tree copies.
            let due = last_partial.elapsed()
                >= tuning.partial_every.unwrap_or_else(|| partial_interval_for(tables.arena.len()));
            if due && (tuning.partial_every.is_some() || tx.is_empty()) {
                let began = Instant::now();
                let (arena, file_count, dir_count) = tables.snapshot();
                let _ = tx.send(ScanMessage::Partial(Box::new(ScanResult {
                    arena,
                    root: 0,
                    root_path: root_path.clone(),
                    file_count,
                    dir_count,
                    error_count: tables.errors,
                    elapsed_secs: start.elapsed().as_secs_f64(),
                    used_fast_path: true,
                })));
                snapshotting += began.elapsed();
                snapshots += 1;
                last_partial = Instant::now();
            }

            if last_sent.elapsed() >= Duration::from_millis(60) {
                send_progress(tx, &tables);
                last_sent = Instant::now();
            }
        }
        drop(chunk_rx); // lets a reader blocked on a full channel exit
        reader.join()
    });
    let read_stats = match reader_result {
        Ok(Ok(stats)) => stats,
        Ok(Err(e)) if !cancelled => return Err(e),
        Ok(Err(_)) => ReadStats::default(),
        Err(_) => anyhow::bail!("$MFT reader thread panicked"),
    };

    let error_count = tables.errors;
    let root_ok = tables.flags[ROOT_RECORD] & (IN_USE | IS_DIR) == (IN_USE | IS_DIR);
    if !root_ok && !cancelled {
        anyhow::bail!("root directory record not found in $MFT");
    }
    let finishing = Instant::now();
    let (arena, file_count, dir_count) = tables.finish();
    let finishing = finishing.elapsed();
    // Where the time went. `reading` is time inside the OS read calls on the
    // reader thread; `waiting` is how long the parser sat idle for the disk.
    // Big `waiting` => disk-bound; big `parsing`/`snapshots` => CPU-bound.
    let secs = |d: Duration| d.as_secs_f64();
    let read_secs = read_stats.nanos as f64 / 1e9;
    crate::debug_log::log(&format!(
        "MFT timing: total {:.1}s | read {:.1}s for {} MiB ({:.0} MiB/s inside read calls) | parser waited on disk {:.1}s | \
         parsing {:.1}s | live snapshots {:.1}s ({snapshots} taken) | finishing {:.1}s",
        start.elapsed().as_secs_f64(),
        read_secs,
        read_stats.bytes >> 20,
        if read_secs > 0.0 { read_stats.bytes as f64 / 1_048_576.0 / read_secs } else { 0.0 },
        secs(waited),
        secs(parsing),
        secs(snapshotting),
        secs(finishing),
    ));
    crate::debug_log::log(&format!(
        "MFT: finished with {file_count} files, {dir_count} folders, {error_count} unusable record(s)"
    ));

    Ok(ScanResult {
        arena,
        root: 0,
        root_path,
        file_count,
        dir_count,
        error_count,
        elapsed_secs: start.elapsed().as_secs_f64(),
        used_fast_path: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ---- builders for a tiny, fully synthetic NTFS volume -----------------

    const RS: usize = 1024;
    const CLUSTER: usize = 4096;
    const MFT_LCN: usize = 4;

    fn p16(b: &mut Vec<u8>, o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn p32(b: &mut Vec<u8>, o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn p64(b: &mut Vec<u8>, o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn pad8(v: &mut Vec<u8>) {
        while v.len() % 8 != 0 {
            v.push(0);
        }
    }

    /// Resident attribute with `value` as its content.
    fn resident(ty: u32, value: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 24];
        p32(&mut a, 0, ty);
        p32(&mut a, 16, value.len() as u32);
        p16(&mut a, 20, 24);
        a.extend_from_slice(value);
        pad8(&mut a);
        let len = a.len() as u32;
        p32(&mut a, 4, len);
        a
    }

    fn file_name(parent: u64, ns: u8, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66];
        p64(&mut v, 0, parent | (1 << 48)); // sequence number must be ignored
        v[64] = units.len() as u8;
        v[65] = ns;
        for u in units {
            v.extend_from_slice(&u.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, &v)
    }

    fn data_resident(len: usize) -> Vec<u8> {
        resident(ATTR_DATA, &vec![7u8; len])
    }

    fn data_nonresident(vcn: u64, real_size: u64, runlist: &[u8]) -> Vec<u8> {
        nonresident(ATTR_DATA, vcn, real_size, runlist)
    }

    fn nonresident(ty: u32, vcn: u64, real_size: u64, runlist: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 64];
        p32(&mut a, 0, ty);
        a[8] = 1;
        p64(&mut a, 16, vcn);
        p16(&mut a, 32, 64);
        p64(&mut a, 48, real_size);
        a.extend_from_slice(runlist);
        pad8(&mut a);
        let len = a.len() as u32;
        p32(&mut a, 4, len);
        a
    }

    /// Encode extents `(start cluster, length)` as a runlist, each start
    /// relative to the previous (the first relative to 0), like NTFS does.
    fn encode_runs(extents: &[(usize, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev = 0i64;
        for &(lcn, len) in extents {
            let delta = lcn as i64 - prev;
            out.extend_from_slice(&[0x21, len as u8, delta as u8, (delta >> 8) as u8]);
            prev = lcn as i64;
        }
        out.push(0);
        out
    }

    /// One `$ATTRIBUTE_LIST` entry: (type, starting VCN, file reference).
    fn list_entry(ty: u32, vcn: u64, rec: u64) -> Vec<u8> {
        let mut e = vec![0u8; 32];
        p32(&mut e, 0, ty);
        p16(&mut e, 4, 32);
        p64(&mut e, 8, vcn);
        p64(&mut e, 16, rec | (1 << 48));
        e
    }

    /// A complete, fixed-up 1024-byte file record.
    fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; RS];
        r[0..4].copy_from_slice(b"FILE");
        p16(&mut r, 4, 0x30); // update sequence array offset
        p16(&mut r, 6, (RS / 512 + 1) as u16);
        p16(&mut r, 0x14, 0x38); // first attribute
        p16(&mut r, 0x16, flags);
        p64(&mut r, 0x20, base);
        let mut off = 0x38;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        p32(&mut r, off, ATTR_END);
        p32(&mut r, 0x18, (off + 8) as u32); // bytes in use
        p32(&mut r, 0x1C, RS as u32);
        // Apply the update sequence protection.
        let usn = [0x34u8, 0x12];
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 0..RS / 512 {
            let end = (i + 1) * 512;
            let orig = [r[end - 2], r[end - 1]];
            r[0x32 + i * 2..0x34 + i * 2].copy_from_slice(&orig);
            r[end - 2..end].copy_from_slice(&usn);
        }
        r
    }

    const FILE: u16 = 1;
    const DIR: u16 = 1 | 2;

    /// Builds the volume described in the comments and returns its bytes.
    fn synthetic_volume(mutate: impl FnOnce(&mut Vec<Vec<u8>>)) -> Vec<u8> {
        synthetic_volume_at(&[(MFT_LCN, 4)], &[], mutate)
    }

    const N: usize = 16;
    const DATA_SIZE: u64 = (N * RS) as u64;

    /// Like `synthetic_volume`, but with the `$MFT` stored as the given
    /// extents `(start cluster, clusters)` in virtual order (to build a
    /// fragmented `$MFT`), plus `extra` raw bytes to place at byte offsets
    /// (e.g. a non-resident attribute list). `mutate` may replace any record.
    fn synthetic_volume_at(
        extents: &[(usize, usize)],
        extra: &[(usize, Vec<u8>)],
        mutate: impl FnOnce(&mut Vec<Vec<u8>>),
    ) -> Vec<u8> {
        synthetic_volume_n(N, extents, extra, mutate)
    }

    /// As `synthetic_volume_at`, with `n` records in the `$MFT` (`n >= 16`).
    fn synthetic_volume_n(
        n: usize,
        extents: &[(usize, usize)],
        extra: &[(usize, Vec<u8>)],
        mutate: impl FnOnce(&mut Vec<Vec<u8>>),
    ) -> Vec<u8> {
        let mut recs: Vec<Vec<u8>> = vec![vec![0u8; RS]; n];
        // 0: $MFT itself (a file in the root, like on a real volume)
        recs[0] = record(
            FILE,
            0,
            &[file_name(5, 3, "$MFT"), data_nonresident(0, (n * RS) as u64, &encode_runs(extents))],
        );
        // 1: \late_dir\child_of_late.txt — its record comes BEFORE its parent's
        // (15), so it must wait for the directory to appear.
        recs[1] = record(FILE, 0, &[file_name(15, 3, "child_of_late.txt"), data_resident(7)]);
        // 5: root directory (its "." name points at itself)
        recs[5] = record(DIR, 0, &[file_name(5, 3, ".")]);
        // 6: \docs
        recs[6] = record(DIR, 0, &[file_name(5, 3, "docs")]);
        // 7: \docs\a.txt, 10 bytes resident
        recs[7] = record(FILE, 0, &[file_name(6, 3, "a.txt"), data_resident(10)]);
        // 8: long name + separate 8.3 alias, 5000 bytes non-resident (+ unicode)
        recs[8] = record(
            FILE,
            0,
            &[
                file_name(6, 1, "Long ünï File.txt"),
                file_name(6, 2, "LONGFI~1.TXT"),
                data_nonresident(0, 5000, &[0x11, 2, 0x50, 0]),
            ],
        );
        // 9: hard link: \h1 and \docs\h2, 100 bytes
        recs[9] = record(FILE, 0, &[file_name(5, 1, "h1"), file_name(6, 1, "h2"), data_resident(100)]);
        // 10: deleted file (not in use) — must not appear
        recs[10] = record(0, 0, &[file_name(5, 1, "deleted.txt"), data_resident(300)]);
        // 11 + 12: base record with its $DATA in extension record 12
        recs[11] = record(FILE, 0, &[file_name(5, 1, "big.bin"), resident(ATTR_ATTRIBUTE_LIST, &[0; 16])]);
        recs[12] = record(FILE, 11, &[data_nonresident(0, 123_456, &[0x11, 1, 0x60, 0])]);
        // 13: orphan — parent 14 is not an in-use directory
        recs[13] = record(FILE, 0, &[file_name(14, 1, "orphan.txt"), data_resident(50)]);
        // 14: a plain file, so "parent is a file, not a directory" is exercised
        recs[14] = record(FILE, 0, &[file_name(5, 1, "plainfile"), data_resident(1)]);
        // 15: \late_dir (see record 1)
        recs[15] = record(DIR, 0, &[file_name(5, 3, "late_dir")]);
        mutate(&mut recs);

        let end_cluster = extents.iter().map(|&(lcn, len)| lcn + len).max().unwrap_or(0).max(48);
        let mut img = vec![0u8; (end_cluster + 8) * CLUSTER];
        img[3..11].copy_from_slice(b"NTFS    ");
        p16(&mut img, 0x0B, 512);
        img[0x0D] = (CLUSTER / 512) as u8;
        p64(&mut img, 0x30, extents[0].0 as u64);
        img[0x40] = (-10i8) as u8; // 2^10 = 1024-byte records
        for (i, r) in recs.iter().enumerate() {
            // Map the record's position in the $MFT stream to the extents.
            let vcn = i * RS / CLUSTER;
            let (mut base, mut o) = (0, None);
            for &(lcn, len) in extents {
                if vcn < base + len {
                    o = Some(lcn * CLUSTER + (i * RS - base * CLUSTER));
                    break;
                }
                base += len;
            }
            let o = o.expect("record outside the extents");
            img[o..o + RS].copy_from_slice(r);
        }
        for (off, bytes) in extra {
            img[*off..*off + bytes.len()].copy_from_slice(bytes);
        }
        img
    }

    /// A device that rejects any read not aligned to 4096 in offset, length
    /// *and memory address*, like a raw Windows volume handle opened with
    /// `FILE_FLAG_NO_BUFFERING`.
    struct StrictDevice(Cursor<Vec<u8>>);
    impl Read for StrictDevice {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.position() % 4096 != 0 || buf.len() % 4096 != 0 || buf.as_ptr() as usize % 4096 != 0 {
                return Err(std::io::Error::other("unaligned read (ERROR_INVALID_PARAMETER)"));
            }
            self.0.read(buf)
        }
    }
    impl Seek for StrictDevice {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    fn run_scan(img: Vec<u8>) -> anyhow::Result<ScanResult> {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let cancel = AtomicBool::new(false);
        scan_reader(&mut StrictDevice(Cursor::new(img)), "T:", PathBuf::from("T:\\"), &tx, &cancel)
    }

    fn child<'a>(r: &'a ScanResult, parent: usize, name: &str) -> Option<&'a Node> {
        r.arena[parent].children.iter().map(|&c| &r.arena[c]).find(|n| &*n.name == name)
    }
    fn child_idx(r: &ScanResult, parent: usize, name: &str) -> usize {
        *r.arena[parent].children.iter().find(|&&c| &*r.arena[c].name == name).unwrap()
    }

    // ---- tests ------------------------------------------------------------

    #[test]
    fn synthetic_volume_builds_the_right_tree_using_only_aligned_reads() {
        let r = run_scan(synthetic_volume(|_| {})).unwrap();
        assert!(r.used_fast_path);
        let mft_size = (16 * RS) as u64;

        // Root contents — nothing deleted/orphaned. `late_dir`'s child record
        // precedes the directory's own record in the $MFT.
        let mut names: Vec<_> = r.arena[0].children.iter().map(|&c| r.arena[c].name.to_string()).collect();
        names.sort();
        assert_eq!(names, ["$MFT", "big.bin", "docs", "h1", "late_dir", "plainfile"]);
        let late = child_idx(&r, 0, "late_dir");
        assert_eq!(child(&r, late, "child_of_late.txt").unwrap().size, 7, "attached once its parent arrived");
        assert_eq!(child(&r, 0, "big.bin").unwrap().size, 123_456, "size from the extension record");

        let docs = child_idx(&r, 0, "docs");
        let mut dn: Vec<_> = r.arena[docs].children.iter().map(|&c| r.arena[c].name.to_string()).collect();
        dn.sort();
        assert_eq!(dn, ["Long ünï File.txt", "a.txt", "h2"], "8.3 alias skipped, unicode kept");
        assert_eq!(r.arena[docs].size, 10 + 5000 + 100);

        // Hard link is counted under both parents, like a directory walk would.
        assert_eq!(child(&r, 0, "h1").unwrap().size, 100);
        assert_eq!(r.arena[0].size, mft_size + (10 + 5000 + 100) + 100 + 123_456 + 1 + 7);
        assert_eq!((r.file_count, r.dir_count, r.error_count), (8, 3, 0));
        // A parent always precedes its children (what lets sizes be summed in
        // one reverse pass, and keeps indices stable for the GUI).
        for (i, n) in r.arena.iter().enumerate().skip(1) {
            assert!(n.parent.unwrap() < i);
        }
    }

    /// Reads of 1500 or 700 bytes (not a multiple of the 1024-byte record, nor
    /// of a sector) make nearly every record straddle two reads, exercising
    /// the carry-over logic; the tree must come out identical.
    #[test]
    fn records_straddling_read_boundaries_give_the_same_tree() {
        let dump = |r: &ScanResult| {
            let mut v: Vec<_> = r.arena.iter().map(|n| (n.name.to_string(), n.is_dir, n.size, n.children.len())).collect();
            v.sort();
            v
        };
        let reference = dump(&run_scan(synthetic_volume(|_| {})).unwrap());
        for chunk in [1500, 700, 1024, 4096 + 8] {
            let (tx, _rx) = crossbeam_channel::unbounded();
            let cancel = AtomicBool::new(false);
            let img = synthetic_volume(|_| {});
            let tuning = Tuning { chunk, ..Tuning::default() };
            let r = scan_reader_tuned(&mut StrictDevice(Cursor::new(img)), "T:", PathBuf::from("T:\\"), &tx, &cancel, &tuning)
                .unwrap();
            assert_eq!(dump(&r), reference, "chunk size {chunk}");
        }
    }

    #[test]
    fn corrupt_and_torn_records_are_counted_not_fatal() {
        let r = run_scan(synthetic_volume(|recs| {
            recs[7][510] ^= 0xFF; // break the update-sequence check on a.txt
        }))
        .unwrap();
        assert_eq!(r.error_count, 1);
        assert!(child(&r, child_idx(&r, 0, "docs"), "a.txt").is_none());
        assert!(child(&r, 0, "big.bin").is_some());
    }

    fn dump(r: &ScanResult) -> Vec<(String, bool, u64, usize)> {
        let mut v: Vec<_> = r.arena.iter().map(|n| (n.name.to_string(), n.is_dir, n.size, n.children.len())).collect();
        v.sort();
        v
    }

    /// The `$MFT` split into two non-adjacent extents (what a heavily used
    /// drive looks like), with record 0 pointing at the rest through an
    /// `$ATTRIBUTE_LIST`. Must give exactly the same tree as the contiguous
    /// volume — this is the layout that made the first version give up on a
    /// real C: drive.
    fn fragmented_volume(list_nonresident: bool, second_extent_vcn: u64, list_points_at: u64) -> Vec<u8> {
        let extents = [(4, 2), (20, 2)];
        let list = [
            list_entry(ATTR_FILE_NAME, 0, 0), // other types in the list must be ignored
            list_entry(ATTR_DATA, 0, 0),
            list_entry(ATTR_DATA, second_extent_vcn, list_points_at),
        ]
        .concat();
        let extra = if list_nonresident { vec![(40 * CLUSTER, list.clone())] } else { vec![] };
        synthetic_volume_at(&extents, &extra, move |recs| {
            let list_attr = if list_nonresident {
                nonresident(ATTR_ATTRIBUTE_LIST, 0, list.len() as u64, &encode_runs(&[(40, 1)]))
            } else {
                resident(ATTR_ATTRIBUTE_LIST, &list)
            };
            recs[0] = record(
                FILE,
                0,
                &[file_name(5, 3, "$MFT"), data_nonresident(0, DATA_SIZE, &encode_runs(&extents[..1])), list_attr],
            );
            // An extension record of $MFT: its base reference is record 0 *with
            // a sequence number*, so it only differs from "no base" by that.
            recs[3] = record(FILE, 1 << 48, &[data_nonresident(second_extent_vcn, 0, &encode_runs(&extents[1..]))]);
        })
    }

    #[test]
    fn fragmented_mft_is_followed_through_its_attribute_list() {
        let reference = dump(&run_scan(synthetic_volume(|_| {})).unwrap());
        for nonresident in [false, true] {
            let r = run_scan(fragmented_volume(nonresident, 2, 3)).unwrap();
            assert_eq!(dump(&r), reference, "attribute list non-resident = {nonresident}");
            assert_eq!(r.error_count, 0);
        }
    }

    #[test]
    fn inconsistent_fragmented_mfts_are_rejected_not_guessed() {
        // The list names record 12 for the second extent, but record 12 lies in
        // the part of the $MFT that hasn't been located yet.
        let e = run_scan(fragmented_volume(false, 2, 12)).err().expect("must fail").to_string();
        assert!(e.contains("outside"), "{e}");
        // A gap in virtual cluster numbers (extent 2 claims to start at 3).
        let e = run_scan(fragmented_volume(false, 3, 3)).err().expect("must fail").to_string();
        assert!(e.contains("not contiguous"), "{e}");
    }

    /// The GUI keeps arena indices (what's expanded, where the user is looking)
    /// valid across live snapshots because scanners only ever append. Every
    /// snapshot, and the final result, must extend the previous one unchanged.
    #[test]
    fn live_snapshots_only_append_so_gui_indices_stay_valid() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let collector = std::thread::spawn(move || rx.iter().collect::<Vec<_>>());
        let cancel = AtomicBool::new(false);
        let img = synthetic_volume(|_| {});
        let tuning = Tuning {
            chunk: 1024, // one record per read, so a snapshot after (almost) every record
            partial_every: Some(Duration::ZERO),
            ..Tuning::default()
        };
        let fin = scan_reader_tuned(
            &mut StrictDevice(Cursor::new(img)),
            "T:",
            PathBuf::from("T:\\"),
            &tx,
            &cancel,
            &tuning,
        )
        .unwrap();
        drop(tx);
        let msgs = collector.join().unwrap();
        let partials: Vec<_> = msgs
            .iter()
            .filter_map(|m| if let ScanMessage::Partial(p) = m { Some(p) } else { None })
            .collect();
        assert!(partials.len() >= 8, "expected a live snapshot per read, got {}", partials.len());

        let extends = |a: &ScanResult, b: &ScanResult| {
            a.arena.len() <= b.arena.len()
                && a.arena.iter().zip(&b.arena).all(|(x, y)| x.name == y.name && x.is_dir == y.is_dir && x.parent == y.parent)
        };
        for w in partials.windows(2) {
            assert!(extends(&w[0], &w[1]), "a snapshot rewrote earlier nodes");
        }
        assert!(extends(partials.last().unwrap(), &fin), "final result rewrote earlier nodes");
        // Snapshots carry finished directory sizes, not just file sizes.
        for p in &partials {
            let sum: u64 = p.arena[0].children.iter().map(|&c| p.arena[c].size).sum();
            assert_eq!(p.arena[0].size, sum);
        }
    }

    /// Wraps the strict device and counts the bytes actually read from it.
    struct CountingDevice {
        inner: StrictDevice,
        bytes: u64,
    }
    impl Read for CountingDevice {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes += n as u64;
            Ok(n)
        }
    }
    impl Seek for CountingDevice {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// A device that stalls once, on its first read at or beyond `after`.
    struct StallingDevice {
        inner: StrictDevice,
        after: u64,
        stalled: bool,
    }
    impl Read for StallingDevice {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.stalled && self.inner.0.position() >= self.after {
                self.stalled = true;
                std::thread::sleep(Duration::from_millis(400));
            }
            self.inner.read(buf)
        }
    }
    impl Seek for StallingDevice {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// A drive that goes quiet for a while (a slow region of an ageing SSD) must
    /// be reported, and the normal status restored once data flows again —
    /// otherwise the progress display just looks frozen.
    #[test]
    fn a_slow_drive_is_reported_and_the_status_restored() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = AtomicBool::new(false);
        let img = synthetic_volume(|_| {});
        let mut dev = StallingDevice { inner: StrictDevice(Cursor::new(img)), after: (MFT_LCN * CLUSTER + 4096) as u64, stalled: false };
        // One record per read, and "slow" after 100ms of silence.
        let tuning = Tuning { chunk: 1024, slow_after: Duration::from_millis(100), ..Tuning::default() };
        let r = scan_reader_tuned(&mut dev, "T:", PathBuf::from("T:\\"), &tx, &cancel, &tuning).unwrap();
        assert_eq!(r.error_count, 0);
        let infos: Vec<String> =
            rx.try_iter().filter_map(|m| if let ScanMessage::Info(s) = m { Some(s) } else { None }).collect();
        let slow = infos.iter().position(|s| s.contains("slow to read")).expect("slow-drive notice missing");
        assert!(
            infos[slow + 1..].iter().any(|s| s == "Using fast scan."),
            "status must be restored afterwards: {infos:?}"
        );
    }

    #[test]
    fn needed_ranges_skip_big_free_gaps_and_read_through_small_ones() {
        let rs = 1024u64;
        let mut bits = vec![0u8; 8]; // 64 records
        for r in [0usize, 1, 40, 41] {
            bits[r / 8] |= 1 << (r % 8);
        }
        // Records 2..40 are free (38 of them).
        let r = needed_ranges(Some(&bits), 64, rs, 4);
        assert_eq!(r, [(0, 4096), (40 * 1024, 44 * 1024)], "ranges are rounded out to 4096");
        // A threshold bigger than the gap reads straight through it.
        assert_eq!(needed_ranges(Some(&bits), 64, rs, 100), [(0, 44 * 1024)]);
        // Without a usable bitmap the whole table is read.
        let whole = vec![(0, 64 * rs)];
        assert_eq!(needed_ranges(None, 64, rs, 4), whole);
        assert_eq!(needed_ranges(Some(&bits[..4]), 64, rs, 4), whole, "bitmap too short");
        assert_eq!(needed_ranges(Some(&[0u8; 8]), 64, rs, 4), whole, "all-free can't be right");
        // Bits past the record count are ignored.
        let mut stray = bits.clone();
        stray[7] |= 0x80; // record 63, but only 60 records exist
        assert_eq!(needed_ranges(Some(&stray), 60, rs, 4), [(0, 4096), (40 * 1024, 44 * 1024)]);
    }

    /// A drive that once held a huge tree which was then deleted keeps all those
    /// free records in its `$MFT`. The scan must skip them (using the `$MFT`'s own
    /// `$BITMAP`), read far fewer bytes, and still produce exactly the same tree.
    #[test]
    fn free_stretches_of_the_mft_are_skipped_without_changing_the_result() {
        const N_BIG: usize = 96;
        let extents = [(4, N_BIG * RS / CLUSTER)];
        let volume = |with_bitmap: bool| {
            synthetic_volume_n(N_BIG, &extents, &[], |recs| {
                // A directory and its child sit far beyond a long free stretch
                // (16..92). The child refers to the directory *by record number*,
                // so mis-numbering records after a skipped gap would orphan it.
                recs[94] = record(DIR, 0, &[file_name(5, 1, "tail_dir")]);
                recs[95] = record(FILE, 0, &[file_name(94, 1, "tail.bin"), data_resident(33)]);
                if with_bitmap {
                    let mut bits = vec![0u8; N_BIG.div_ceil(8)];
                    for (i, r) in recs.iter().enumerate() {
                        if u16::from_le_bytes([r[0x16], r[0x17]]) & 1 != 0 {
                            bits[i / 8] |= 1 << (i % 8);
                        }
                    }
                    recs[0] = record(
                        FILE,
                        0,
                        &[
                            file_name(5, 3, "$MFT"),
                            data_nonresident(0, (N_BIG * RS) as u64, &encode_runs(&extents)),
                            resident(ATTR_BITMAP, &bits),
                        ],
                    );
                }
            })
        };
        let scan = |img: Vec<u8>| {
            let (tx, _rx) = crossbeam_channel::unbounded();
            let cancel = AtomicBool::new(false);
            let mut dev = CountingDevice { inner: StrictDevice(Cursor::new(img)), bytes: 0 };
            let tuning = Tuning { min_gap_bytes: 1, ..Tuning::default() };
            let r = scan_reader_tuned(&mut dev, "T:", PathBuf::from("T:\\"), &tx, &cancel, &tuning).unwrap();
            (r, dev.bytes)
        };
        let (full, full_bytes) = scan(volume(false));
        let (skipped, skipped_bytes) = scan(volume(true));
        assert_eq!(dump(&skipped), dump(&full));
        let tail_dir = child_idx(&skipped, 0, "tail_dir");
        assert_eq!(
            child(&skipped, tail_dir, "tail.bin").map(|n| n.size),
            Some(33),
            "records after the gap must keep their real record numbers"
        );
        assert_eq!(skipped.error_count, 0);
        assert!(
            skipped_bytes * 2 < full_bytes,
            "expected to read well under half: {skipped_bytes} vs {full_bytes} bytes"
        );
    }

    #[test]
    fn a_bitmap_that_lies_about_nothing_in_use_is_ignored() {
        // A bitmap with no set bits (corrupt) must not make the scan read nothing.
        let img = synthetic_volume(|recs| {
            recs[0] = record(
                FILE,
                0,
                &[
                    file_name(5, 3, "$MFT"),
                    data_nonresident(0, DATA_SIZE, &encode_runs(&[(MFT_LCN, 4)])),
                    resident(ATTR_BITMAP, &[0u8; 2]),
                ],
            );
        });
        let r = run_scan(img).unwrap();
        assert_eq!(dump(&r), dump(&run_scan(synthetic_volume(|_| {})).unwrap()));
    }

    #[test]
    fn directory_cycles_cannot_hang_the_scan() {
        // 6 (docs) claims to live inside 15, and 15 inside 6: unreachable
        // from the root, so neither may appear, and nothing may loop.
        let r = run_scan(synthetic_volume(|recs| {
            recs[6] = record(DIR, 0, &[file_name(15, 3, "docs")]);
            recs[15] = record(DIR, 0, &[file_name(6, 3, "loop")]);
        }))
        .unwrap();
        assert!(child(&r, 0, "docs").is_none());
    }

    #[test]
    fn unsupported_or_foreign_volumes_are_errors_so_the_caller_falls_back() {
        // Not NTFS at all.
        let mut img = synthetic_volume(|_| {});
        img[3] = b'X';
        assert!(run_scan(img).is_err());
        // $MFT's own record needing an attribute list (heavily fragmented MFT).
        let img = synthetic_volume(|recs| {
            let mut a = vec![
                file_name(5, 3, "$MFT"),
                data_nonresident(0, (16 * RS) as u64, &[0x11, 4, MFT_LCN as u8, 0]),
            ];
            a.push(resident(ATTR_ATTRIBUTE_LIST, &[0; 8]));
            recs[0] = record(FILE, 0, &a);
        });
        let e = run_scan(img).err().expect("must be rejected").to_string();
        assert!(e.contains("attribute list"), "{e}");
    }

    #[test]
    fn cancel_before_start_returns_an_empty_root_not_an_error() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let cancel = AtomicBool::new(true);
        let img = synthetic_volume(|_| {});
        let r = scan_reader(&mut Cursor::new(img), "T:", PathBuf::from("T:\\"), &tx, &cancel).unwrap();
        // The first chunk is dropped unread, so only the (empty) root exists.
        assert_eq!(r.arena.len(), 1);
        assert_eq!(r.error_count, 0);
    }

    #[test]
    fn runlists_decode_including_negative_deltas_and_sparse_runs() {
        // len 3 @ 0x10; len 2 sparse; len 4 @ 0x10 + (-8) = 8 (1-byte signed delta 0xF8)
        let runs = decode_runs(&[0x11, 3, 0x10, 0x01, 2, 0x11, 4, 0xF8, 0]).unwrap();
        assert_eq!(
            runs,
            [Run { lcn: Some(0x10), len: 3 }, Run { lcn: None, len: 2 }, Run { lcn: Some(8), len: 4 }]
        );
        // Multi-byte length and offset.
        let runs = decode_runs(&[0x22, 0x34, 0x12, 0x00, 0x01, 0]).unwrap();
        assert_eq!(runs, [Run { lcn: Some(0x100), len: 0x1234 }]);
        // Truncated / absurd headers are rejected, never panic.
        assert!(decode_runs(&[0x31, 1]).is_none());
        assert!(decode_runs(&[0x91, 1, 0]).is_none());
        // Delta that would go below cluster 0 is corrupt.
        assert!(decode_runs(&[0x11, 1, 0xFF, 0]).is_none());
    }

    #[test]
    fn parsers_never_panic_on_garbage() {
        // Deterministic pseudo-random junk, with a valid "FILE" prefix some of the time.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut parsed = Parsed::default();
        for round in 0..3000 {
            let mut rec = vec![0u8; RS];
            for b in rec.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            if round % 2 == 0 {
                rec[0..4].copy_from_slice(b"FILE");
                p16(&mut rec, 4, 0x30);
                p16(&mut rec, 6, 3);
                p16(&mut rec, 0x14, 0x38);
                p32(&mut rec, 0x18, RS as u32);
            }
            let _ = apply_fixups(&mut rec);
            let _ = parse_record(&rec, &mut parsed);
            let _ = decode_runs(&rec);
            let _ = parse_boot(&rec);
            let boot = Boot { cluster: 4096, record_size: RS, mft_offset: 0 };
            let _ = locate_mft(&mut Cursor::new(vec![0u8; 8192]), &boot, &rec);
            let _ = attr_positions(&rec);
        }
    }

    #[test]
    fn aligned_reader_handles_unaligned_requests_and_the_device_end() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut dev = StrictDevice(Cursor::new(data.clone()));
        let mut out = vec![0u8; 700];
        read_exact_aligned(&mut dev, 5000, &mut out).unwrap();
        assert_eq!(out, data[5000..5700]);
        // A request ending at the very end of a device that isn't a multiple
        // of 4096 long must still work: rounding up overshoots EOF.
        let mut tail = vec![0u8; 10_000 - 9_000];
        read_exact_aligned(&mut dev, 9_000, &mut tail).unwrap();
        assert_eq!(tail, data[9_000..]);
        // But genuinely reading past the end is an error.
        let mut past = vec![0u8; 100];
        assert!(read_exact_aligned(&mut dev, 9_950, &mut past).is_err());
    }

    #[test]
    fn aligned_buffers_are_aligned_zeroed_and_reusable() {
        let mut b = AlignedBuf::new(10_000);
        assert_eq!(b.as_mut_slice().as_ptr() as usize % 4096, 0);
        assert!(b.capacity() >= 10_000 && b.capacity() % 4096 == 0);
        assert!(b.as_mut_slice().iter().all(|&x| x == 0));
        b.as_mut_slice().fill(0xAB);
        b.set_len(100); // a shorter final read reuses the same allocation
        assert_eq!(b.as_mut_slice().len(), 100);
        assert_eq!(AlignedBuf::new(0).as_mut_slice().len(), 0);
    }

    #[test]
    fn drive_letter_parsing() {
        #[cfg(windows)]
        {
            assert_eq!(drive_letter_of_root(Path::new(r"C:\")), Some('C'));
            assert_eq!(drive_letter_of_root(Path::new(r"C:\Users")), None);
        }
        assert_eq!(drive_letter_of_root(Path::new("/")), None);
    }
}
