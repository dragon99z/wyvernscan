//! A `Read + Seek` wrapper that translates arbitrary-offset, arbitrary-
//! length access into sector-aligned reads underneath, with a small
//! multi-slot LRU cache rather than a single buffer.
//!
//! This exists specifically for Windows raw volume handles (used by
//! `mft.rs`): Windows requires reads to a volume device to be aligned to
//! the disk's sector size in both offset and length, even on a handle
//! opened without `FILE_FLAG_NO_BUFFERING`. The `ntfs` crate routinely
//! seeks to arbitrary mid-sector byte offsets to read individual struct
//! fields, and a plain `BufReader` just forwards that unaligned seek
//! straight through to the OS — which then fails with
//! `ERROR_INVALID_PARAMETER`. This type is the fix: it presents an
//! ordinary seekable byte stream to its caller while only ever actually
//! reading `inner` at sector-aligned offsets.
//!
//! The cache is multiple small slots, not one big buffer, because of a
//! real-world performance bug: walking the directory tree means constantly
//! bouncing between a file's MFT record and its directory's index data,
//! which are very often nowhere near each other on disk. A single-buffer
//! cache gets evicted on nearly every access in that pattern — on one
//! user's 128GB drive, that meant a scan that should take seconds took
//! roughly 30 minutes, almost certainly from the resulting flood of
//! redundant whole-buffer re-reads. A handful of independently-cached
//! slots means the small number of regions actually in active use (the
//! MFT, the directory currently being listed, maybe one or two more) can
//! all stay resident at once instead of evicting each other on every
//! single step back and forth.
//!
//! Kept in its own platform-independent module (rather than inside the
//! `#[cfg(windows)]`-gated `mft.rs`) specifically so its logic can be
//! tested against a plain in-memory buffer on any platform — this project
//! was built without a Windows machine available, so anything that can be
//! verified without one, is.
//!
//! That also means its public API is legitimately unused dead code when
//! compiling for anything other than Windows (its only real caller is
//! `mft.rs`, which doesn't exist on other platforms) — not a bug.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io::{Read, Seek, SeekFrom};

/// Read/seek granularity assumed for the underlying device. 512 bytes is
/// the logical sector size on essentially every real drive, including 4Kn
/// drives running in 512e compatibility mode, so aligning to it is safe
/// without needing to query the drive's actual physical sector size.
const SECTOR: u64 = 512;

/// Size of each cached region. Large enough to amortize the cost of a
/// syscall over a useful amount of data (MFT records and index buffers are
/// typically 1-4KB, so this covers many of them per read), small enough
/// that a handful of slots covering different hot regions doesn't mean
/// reading megabytes of data that's never actually used.
const SLOT_SIZE: usize = 64 * 1024;

/// How many independent regions to keep cached at once. Doesn't need to be
/// large — just more than one, which is the entire point — but a bit of
/// headroom costs little (64 slots x 64KiB is 4MiB) and absorbs walks that
/// bounce between more than two hot regions at once.
const SLOT_COUNT: usize = 64;

struct Slot {
    /// Volume offset this slot's data starts at; `u64::MAX` marks an empty,
    /// never-yet-used slot (never a valid aligned start in practice).
    start: u64,
    /// Valid bytes in `data` (may be less than `data.len()` near the end of
    /// the device).
    len: usize,
    data: Vec<u8>,
    /// Logical clock value from the access that last used this slot, for
    /// LRU eviction — the slot with the smallest value is least recently
    /// used.
    last_used: u64,
}

pub struct AlignedVolumeReader<R> {
    inner: R,
    slot_size: u64,
    slots: Vec<Slot>,
    pos: u64,
    clock: u64,
}

impl<R: Read + Seek> AlignedVolumeReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_slot_config(inner, SLOT_SIZE, SLOT_COUNT)
    }

    /// Exposed separately from `new` so tests can use small slots and a
    /// small slot count — exercising eviction and multi-region caching
    /// doesn't require megabyte-sized test fixtures.
    fn with_slot_config(inner: R, slot_size: usize, slot_count: usize) -> Self {
        let slot_size = (slot_size.max(SECTOR as usize)) as u64;
        let slots = (0..slot_count.max(1))
            .map(|_| Slot { start: u64::MAX, len: 0, data: vec![0u8; slot_size as usize], last_used: 0 })
            .collect();
        Self { inner, slot_size, slots, pos: 0, clock: 0 }
    }

    fn slot_start_for(&self, pos: u64) -> u64 {
        (pos / self.slot_size) * self.slot_size
    }

    /// Finds the slot covering `pos`, loading it from the device first if
    /// no cached slot covers it yet. Returns that slot's index.
    fn slot_for(&mut self, pos: u64) -> std::io::Result<usize> {
        let start = self.slot_start_for(pos);
        self.clock += 1;

        if let Some(i) = self.slots.iter().position(|s| s.start == start) {
            self.slots[i].last_used = self.clock;
            return Ok(i);
        }

        self.inner.seek(SeekFrom::Start(start))?;
        let slot_size = self.slot_size as usize;

        // Reuse the least-recently-used slot's buffer in place rather than
        // allocating a new one on every miss.
        let victim = self
            .slots
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| s.last_used)
            .map(|(i, _)| i)
            .expect("slots is never empty");

        // A single read() can legitimately return fewer bytes than asked
        // for even mid-device, not just at EOF, so loop until the slot is
        // full or the device genuinely has no more to give right now.
        let mut total = 0usize;
        while total < slot_size {
            match self.inner.read(&mut self.slots[victim].data[total..])? {
                0 => break,
                n => total += n,
            }
        }
        self.slots[victim].start = start;
        self.slots[victim].len = total;
        self.slots[victim].last_used = self.clock;
        Ok(victim)
    }
}

impl<R: Read + Seek> Read for AlignedVolumeReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let idx = self.slot_for(self.pos)?;
        let slot = &self.slots[idx];
        if slot.len == 0 {
            return Ok(0); // end of device
        }
        let offset_in_slot = (self.pos - slot.start) as usize;
        if offset_in_slot >= slot.len {
            return Ok(0);
        }
        let n = (slot.len - offset_in_slot).min(out.len());
        out[..n].copy_from_slice(&slot.data[offset_in_slot..offset_in_slot + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for AlignedVolumeReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(delta) => {
                if delta >= 0 {
                    self.pos + delta as u64
                } else {
                    self.pos.checked_sub((-delta) as u64).ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before byte 0")
                    })?
                }
            }
            // Not exercised by the ntfs crate for a read-only directory
            // walk, and this type doesn't track the volume's total size —
            // fail clearly rather than guess at one.
            SeekFrom::End(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "seeking from the end of a raw volume isn't supported here",
                ));
            }
        };
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Cursor;

    /// Wraps a `Cursor` but panics on any read that isn't sector-aligned in
    /// both offset and length — standing in for what a real Windows raw
    /// volume handle does. If `AlignedVolumeReader` is doing its job, this
    /// inner type should never actually see an unaligned request, no
    /// matter how the outer reader is used. Also counts how many reads
    /// actually reached the device, to directly verify the cache is doing
    /// its job of avoiding redundant ones.
    struct RejectsUnalignedReads {
        cursor: Cursor<Vec<u8>>,
        pos: u64,
        read_count: Cell<u32>,
    }

    impl Read for RejectsUnalignedReads {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            assert_eq!(self.pos % SECTOR, 0, "unaligned OFFSET reached the device: {}", self.pos);
            assert_eq!(out.len() as u64 % SECTOR, 0, "unaligned LENGTH reached the device: {}", out.len());
            self.read_count.set(self.read_count.get() + 1);
            let n = self.cursor.read(out)?;
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for RejectsUnalignedReads {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let new_pos = self.cursor.seek(pos)?;
            assert_eq!(new_pos % SECTOR, 0, "unaligned SEEK reached the device: {new_pos}");
            self.pos = new_pos;
            Ok(new_pos)
        }
    }

    fn make_device(size: usize) -> RejectsUnalignedReads {
        // Distinct byte pattern so any misplaced read is obviously wrong,
        // not accidentally right due to repeated bytes.
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        RejectsUnalignedReads { cursor: Cursor::new(data), pos: 0, read_count: Cell::new(0) }
    }

    #[test]
    fn unaligned_reads_return_correct_bytes_without_hitting_device_unaligned() {
        let device_size = 8192;
        let expected: Vec<u8> = (0..device_size).map(|i| (i % 256) as u8).collect();

        // Small slots (2 sectors) so loading a fresh one is exercised
        // repeatedly across this test, not just satisfied by one big read.
        let mut reader =
            AlignedVolumeReader::with_slot_config(make_device(device_size), (SECTOR * 2) as usize, 4);

        // Mimic exactly the pattern that triggered the real bug: seek to
        // an arbitrary mid-sector offset (not a multiple of 512) and read
        // a small, oddly-sized field.
        for &(offset, len) in &[(11u64, 25usize), (600, 8), (3, 1), (4090, 16), (0, 512), (8000, 100)] {
            reader.seek(SeekFrom::Start(offset)).unwrap();
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).unwrap();
            assert_eq!(
                buf,
                expected[offset as usize..offset as usize + len],
                "wrong bytes for offset {offset}, len {len}"
            );
        }
    }

    #[test]
    fn sequential_read_across_slot_boundary_is_contiguous() {
        let device_size = 4096;
        let expected: Vec<u8> = (0..device_size).map(|i| (i % 256) as u8).collect();
        let mut reader =
            AlignedVolumeReader::with_slot_config(make_device(device_size), (SECTOR * 2) as usize, 4);

        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        assert_eq!(all, expected);
    }

    /// The actual performance bug, reproduced directly: repeatedly bounce
    /// between two far-apart regions (standing in for a file's MFT record
    /// and its directory's index data). With only one cached slot, *every*
    /// access evicts the other region, so N round trips cost 2N device
    /// reads. With enough slots to hold both regions at once, both load
    /// exactly once no matter how many times the walk bounces between
    /// them. This is what actually caused the real ~30-minute scan.
    #[test]
    fn multiple_slots_avoid_thrashing_when_bouncing_between_two_regions() {
        let slot_size = (SECTOR * 4) as usize;
        let device_size = slot_size * 20;
        let region_a = 0u64;
        let region_b = (slot_size * 15) as u64; // far enough to never share a slot with A

        let device = make_device(device_size);
        let mut reader = AlignedVolumeReader::with_slot_config(device, slot_size, 4);
        for _ in 0..50 {
            reader.seek(SeekFrom::Start(region_a + 10)).unwrap();
            let mut buf = [0u8; 4];
            reader.read_exact(&mut buf).unwrap();
            reader.seek(SeekFrom::Start(region_b + 10)).unwrap();
            reader.read_exact(&mut buf).unwrap();
        }
        // Only 2 distinct regions ever touched: exactly 2 device reads
        // total, however many times the walk bounced between them.
        assert_eq!(reader.inner.read_count.get(), 2);

        // The old single-slot design, for direct comparison: every access
        // evicts the other region, so the same 100 accesses cost 100 reads.
        let device = make_device(device_size);
        let mut single_slot_reader = AlignedVolumeReader::with_slot_config(device, slot_size, 1);
        for _ in 0..50 {
            single_slot_reader.seek(SeekFrom::Start(region_a + 10)).unwrap();
            let mut buf = [0u8; 4];
            single_slot_reader.read_exact(&mut buf).unwrap();
            single_slot_reader.seek(SeekFrom::Start(region_b + 10)).unwrap();
            single_slot_reader.read_exact(&mut buf).unwrap();
        }
        assert_eq!(single_slot_reader.inner.read_count.get(), 100);
    }
}
