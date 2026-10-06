//! Small Windows-specific OS integration helpers, hand-declared against the
//! raw Win32 API instead of pulling in `windows`/`winapi` for just a
//! handful of functions:
//!
//! - `open_console` — for `--debug`, opens a real console window and
//!   redirects stdout/stderr to it.
//! - `attach_console` — for `--cli`, makes console output work from the
//!   GUI-subsystem release exe (see its doc comment for why it is needed).
//! - `pause_if_new_console` — keeps a console window that this process had
//!   to create itself open long enough to be read.
//! - `is_elevated` — whether this process is running as Administrator
//!   (shown as "(Admin)" in the window title).
//! - `restart_as_admin` — relaunches the current executable with a UAC
//!   elevation prompt (the "Restart as Admin" button).
//!
//! Rust's stdio implementation on Windows fetches the current std handle
//! via `GetStdHandle` on every read/write rather than caching it once at
//! process startup, so `open_console` calling `SetStdHandle` is enough to
//! make `println!`/`eprintln!` (and default panic output) start appearing
//! in the freshly allocated console from that point on.
//!
//! Note on `open_console`: this is the process's own console (not a
//! separate child process), so closing its window closes the whole app,
//! the same way closing a terminal running any other program ends it.

use std::ffi::c_void;
use std::io::BufRead;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};

#[allow(non_snake_case)]
extern "system" {
    fn AllocConsole() -> i32;
    fn AttachConsole(dwProcessId: u32) -> i32;
    fn GetStdHandle(nStdHandle: u32) -> *mut c_void;
    fn SetStdHandle(nStdHandle: u32, hHandle: *mut c_void) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn OpenProcessToken(process_handle: *mut c_void, desired_access: u32, token_handle: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token_handle: *mut c_void,
        token_information_class: u32,
        token_information: *mut c_void,
        token_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

#[allow(non_snake_case)]
#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut c_void,
        lp_operation: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show_cmd: i32,
    ) -> *mut c_void;
}

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF; // (DWORD)-1
const TOKEN_QUERY: u32 = 0x0008;
/// `TokenElevation` in the `TOKEN_INFORMATION_CLASS` enum (winnt.h).
const TOKEN_ELEVATION_CLASS: u32 = 20;
const SW_SHOWNORMAL: i32 = 1;

/// Set when `attach_console` had no terminal to attach to and had to open a
/// console window of its own; that window disappears the moment the process
/// exits, so `pause_if_new_console` holds it open.
static OWNS_CONSOLE: AtomicBool = AtomicBool::new(false);

/// A std handle is "missing" when the process was started without one:
/// `NULL` or `INVALID_HANDLE_VALUE`.
fn handle_missing(handle: *mut c_void) -> bool {
    handle.is_null() || handle as isize == -1
}

/// Makes `println!`/`eprintln!` reach a terminal from the GUI-subsystem
/// release exe, for `--cli`.
///
/// A release build is linked with `windows_subsystem = "windows"` so the GUI
/// opens with no console window behind it. The price is that such a process
/// is started with no stdout/stderr at all, so console mode printed nothing
/// (not even `--help`). (Debug builds keep a console, which is why `cargo run`
/// never showed the problem.)
///
/// This attaches to the console of the terminal that launched us, or, when
/// launched some other way (Run dialog, a shortcut), opens a new console
/// window. Only a handle that is actually missing is pointed at the console,
/// so `wyvernscan.exe --cli ... > report.json` still writes the report to the
/// file while the banner and progress still appear on screen.
///
/// Limitation of a single exe: cmd and PowerShell do not wait for a
/// GUI-subsystem program, so the prompt comes back immediately and the exit
/// code is not reported. `start /wait` (cmd) restores both.
pub fn attach_console() {
    let out_missing = handle_missing(unsafe { GetStdHandle(STD_OUTPUT_HANDLE) });
    let err_missing = handle_missing(unsafe { GetStdHandle(STD_ERROR_HANDLE) });
    if !out_missing && !err_missing {
        // Both already go somewhere: redirected, or a console already exists.
        return;
    }

    let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
    if !attached {
        if unsafe { AllocConsole() } == 0 {
            return; // no console to be had; stay silent rather than fail
        }
        OWNS_CONSOLE.store(true, Ordering::Relaxed);
    }

    if let Ok(conout) = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$") {
        let handle = conout.as_raw_handle() as *mut c_void;
        unsafe {
            if out_missing {
                SetStdHandle(STD_OUTPUT_HANDLE, handle);
            }
            if err_missing {
                SetStdHandle(STD_ERROR_HANDLE, handle);
            }
        }
        // Keep the handle alive for the life of the process.
        std::mem::forget(conout);
    }
}

/// If `attach_console` had to open its own console window, waits for Enter so
/// the output can be read before the window closes. A no-op otherwise.
pub fn pause_if_new_console() {
    if !OWNS_CONSOLE.load(Ordering::Relaxed) {
        return;
    }
    eprintln!("\nPress Enter to close this window...");
    if let Ok(conin) = std::fs::File::open("CONIN$") {
        let mut line = String::new();
        let _ = std::io::BufReader::new(conin).read_line(&mut line);
    }
}

/// Opens a console window for this process and points stdout/stderr at it.
/// Safe to call even if a console somehow already exists; failures here
/// are non-fatal (the app just runs without a visible console, and file
/// logging from `debug_log` still works either way).
pub fn open_console() {
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

/// Whether this process is running elevated (as Administrator). Defaults
/// to `false` on any failure querying the process token — an inability to
/// tell is treated the same as "not elevated" rather than risking a
/// confidently wrong "(Admin)" label.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: *mut c_void = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }

        #[repr(C)]
        struct TokenElevation {
            token_is_elevated: u32,
        }
        let mut elevation = TokenElevation { token_is_elevated: 0 };
        let mut returned_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION_CLASS,
            &mut elevation as *mut TokenElevation as *mut c_void,
            std::mem::size_of::<TokenElevation>() as u32,
            &mut returned_len,
        );
        CloseHandle(token);

        ok != 0 && elevation.token_is_elevated != 0
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Relaunches the current executable elevated (triggers the standard UAC
/// prompt) and, if the user is currently running with `--debug`, passes
/// that along too. Does not exit the current process — the caller should
/// do that itself once this returns `Ok`, so the two-instances-briefly
/// overlapping window stays as short as possible.
pub fn restart_as_admin(carry_debug_flag: bool) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let exe_wide = to_wide_null(&exe.to_string_lossy());
    let verb_wide = to_wide_null("runas");
    let params_wide = to_wide_null(if carry_debug_flag { "--debug" } else { "" });

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb_wide.as_ptr(),
            exe_wide.as_ptr(),
            params_wide.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW returns a value > 32 on success; anything else is an
    // error code stuffed into the same pointer-sized return (a historical
    // Win32 API wart), most commonly ERROR_CANCELLED (1223) if the user
    // dismisses the UAC prompt.
    if (result as isize) > 32 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(result as i32))
    }
}
