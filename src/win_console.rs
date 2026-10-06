//! Opens a real console window and redirects stdout/stderr to it, for
//! `--debug` on Windows. Hand-declares the two Win32 functions needed
//! instead of pulling in `windows`/`winapi` for just this.
//!
//! Rust's stdio implementation on Windows fetches the current std handle
//! via `GetStdHandle` on every read/write rather than caching it once at
//! process startup, so calling `SetStdHandle` here is enough to make
//! `println!`/`eprintln!` (and default panic output) start appearing in
//! the freshly allocated console from this point on — no need to touch
//! Rust's own stdout/stderr globals directly.
//!
//! Note: this is the process's own console (not a separate child process),
//! so closing its window closes the whole app, the same way closing a
//! terminal running any other program ends that program.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

#[allow(non_snake_case)]
extern "system" {
    fn AllocConsole() -> i32;
    fn SetStdHandle(nStdHandle: u32, hHandle: *mut c_void) -> i32;
}

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12

/// Opens a console window for this process and points stdout/stderr at it.
/// Safe to call even if a console somehow already exists; failures here
/// are non-fatal (the app just runs without a visible console, and file
/// logging from `debug_log` still works either way).
pub fn open() {
    unsafe {
        AllocConsole();
    }

    if let Ok(conout) = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$") {
        let handle = conout.as_raw_handle() as *mut c_void;
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, handle);
            SetStdHandle(STD_ERROR_HANDLE, handle);
        }
        // Keep the handle alive for the process's lifetime instead of
        // closing it when this local `File` would otherwise drop.
        std::mem::forget(conout);
    }
}
