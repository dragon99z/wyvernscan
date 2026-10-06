//! Querying a volume's total used space — a single fast OS call, not a
//! directory walk — so a whole-drive scan can show a real "X of Y scanned"
//! indicator instead of just a running item count with no known target.
//!
//! Only meaningful for a whole-drive-root scan (`app.rs` only calls this
//! when the chosen path is one of the drives/volumes it already detected):
//! querying a *subfolder's* volume gives you the whole drive's usage, not
//! that subfolder's eventual total, which would be a misleading progress
//! bar for anything narrower than the whole drive.

use std::path::Path;

/// Returns `(used_bytes, total_bytes)` for the volume containing `path`,
/// or `None` if that can't be determined. `used_bytes` (total minus free)
/// is what's compared against bytes scanned so far — free space isn't
/// something a content scan will ever "reach".
pub fn disk_used_bytes(path: &Path) -> Option<(u64, u64)> {
    imp::disk_used_bytes(path)
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[allow(non_snake_case)]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lp_directory_name: *const u16,
            lp_free_bytes_available: *mut c_void,
            lp_total_number_of_bytes: *mut u64,
            lp_total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    pub fn disk_used_bytes(path: &Path) -> Option<(u64, u64)> {
        let wide: Vec<u16> = std::ffi::OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut free: u64 = 0;
        let mut total: u64 = 0;
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), std::ptr::null_mut(), &mut total, &mut free) };
        if ok == 0 || total == 0 {
            return None;
        }
        Some((total.saturating_sub(free), total))
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::path::Path;

    pub fn disk_used_bytes(path: &Path) -> Option<(u64, u64)> {
        let c_path = CString::new(path.to_str()?).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if ok != 0 {
            return None;
        }
        let frsize = stat.f_frsize as u64;
        let total = stat.f_blocks as u64 * frsize;
        let free = stat.f_bfree as u64 * frsize;
        if total == 0 {
            return None;
        }
        Some((total.saturating_sub(free), total))
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use std::path::Path;
    pub fn disk_used_bytes(_path: &Path) -> Option<(u64, u64)> {
        None
    }
}
