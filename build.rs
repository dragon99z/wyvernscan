//! Embeds the WyvernScan icon and version info into `wyvernscan.exe`, so the
//! file shows the wyvern eye in Explorer, the taskbar and Alt-Tab. (The
//! in-app window icon is set separately in `main.rs` and needs none of this.)
//!
//! This is the one place the project takes a build-dependency, and only on
//! Windows hosts: attaching an icon means compiling a Windows resource, which
//! has no std-only equivalent. It is deliberately fail-soft -- if the
//! resource compiler (rc.exe / windres) can't be found, the build still
//! succeeds and just prints a warning; the exe then has the default icon.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "WyvernScan");
        res.set("FileDescription", "WyvernScan - fast disk space explorer");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the exe icon ({e}); building without it");
        }
    }
}
