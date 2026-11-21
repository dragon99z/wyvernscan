//! Embeds the WyvernScan icon into wyvernscan-setup.exe, so the installer shows the
//! wyvernscan eye in Explorer and the taskbar (the window icon is set in gui.rs).
//! Same approach as the app's own build.rs: Windows hosts only, and fail-soft -- if
//! the resource compiler can't be found the build still succeeds, with a warning.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../assets/icon.ico");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/icon.ico");
        res.set("ProductName", "WyvernScan");
        res.set("FileDescription", "WyvernScan Setup");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the exe icon ({e}); building without it");
        }
    }
}
