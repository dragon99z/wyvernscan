//! WyvernScan setup: one self-contained file that carries the app and installs it
//! into the per-user app folder (no admin rights needed):
//!   Windows  %LOCALAPPDATA%\WyvernScan\
//!   Linux    $XDG_DATA_HOME/WyvernScan/ (default ~/.local/share/WyvernScan/)
//!   macOS    ~/Applications/WyvernScan.app
//! The window is egui (gui.rs); this file is the install logic, which has no UI
//! in it. The few things std can't do (a Windows .lnk or registry value, a macOS
//! .icns) shell out to the OS tools (PowerShell, reg, sips), as updater.rs does
//! with curl.
// A GUI program: no console window on Windows. Not for tests, which print to cargo.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod gui;
// The app's own theme, so the installer looks like it; the treemap palette goes unused here.
#[allow(dead_code)]
#[path = "../../src/theme.rs"]
mod theme;

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

// Copied here by CI (or by hand) from target/release/ before building this crate.
static APP: &[u8] = include_bytes!("../payload/wyvernscan");
#[cfg(unix)]
static ICON: &[u8] = include_bytes!("../../assets/icon-512.png");

fn main() -> eframe::Result<()> {
    gui::run()
}

/// What the person ticked in the window.
struct Options {
    menu: bool,
    desktop: bool,
    /// Windows only: always start WyvernScan elevated.
    #[cfg_attr(not(windows), allow(dead_code))]
    admin: bool,
}

/// Installs the app, then the optional extras. Returns one line per extra that
/// failed: a missing shortcut shouldn't undo a good install.
fn install(dir: &Path, o: &Options) -> io::Result<Vec<String>> {
    let exe = install_files(dir)?;
    let mut warnings = Vec::new();
    let mut check = |what: &str, r: io::Result<()>| {
        if let Err(e) = r {
            warnings.push(format!("{what} could not be set up: {e}"));
        }
    };
    #[cfg(target_os = "macos")]
    check("App icon", macos_icon(dir));
    if o.menu {
        check("Menu shortcut", menu_shortcut(dir, &exe));
    }
    if o.desktop {
        check("Desktop shortcut", desktop_shortcut(dir, &exe));
    }
    #[cfg(windows)]
    check("Run as administrator", run_as_admin(&exe, o.admin));
    Ok(warnings)
}

// macOS has no menu option: an app in ~/Applications already shows up in Launchpad.
#[cfg(windows)]
const MENU_LABEL: Option<&str> = Some("Create a Start menu shortcut");
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_LABEL: Option<&str> = Some("Add WyvernScan to the application menu");
#[cfg(target_os = "macos")]
const MENU_LABEL: Option<&str> = None;

fn install_dir() -> io::Result<PathBuf> {
    let var = |k: &str| env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    #[cfg(windows)]
    let dir = var("LOCALAPPDATA").map(|p| p.join("WyvernScan"));
    #[cfg(target_os = "macos")]
    let dir = var("HOME").map(|p| p.join("Applications/WyvernScan.app"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let dir = var("XDG_DATA_HOME")
        .filter(|p| p.is_absolute()) // the XDG spec says to ignore relative values
        .or_else(|| var("HOME").map(|h| h.join(".local/share")))
        .map(|p| p.join("WyvernScan"));
    dir.ok_or_else(|| io::Error::other("can't find your user folder (LOCALAPPDATA / HOME is not set)"))
}

/// Writes the app (plus icon / bundle files on Unix) and returns the exe's path.
fn install_files(dir: &Path) -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let bin_dir = dir.join("Contents/MacOS");
    #[cfg(not(target_os = "macos"))]
    let bin_dir = dir.to_path_buf();
    fs::create_dir_all(&bin_dir)?;

    let exe = bin_dir.join(exe_name());
    // A running exe can be renamed but not overwritten (Windows: access denied,
    // Linux: "text file busy"), so move it aside first, like updater.rs does.
    let old = exe.with_extension("old");
    let _ = fs::remove_file(&old);
    if exe.exists() {
        fs::rename(&exe, &old)?;
    }
    fs::write(&exe, APP).inspect_err(|_| {
        let _ = fs::rename(&old, &exe); // don't leave the user with no app at all
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755))?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fs::write(dir.join("icon.png"), ICON)?;
    #[cfg(target_os = "macos")]
    {
        fs::create_dir_all(dir.join("Contents/Resources"))?;
        fs::write(dir.join("Contents/Info.plist"), PLIST)?;
    }
    Ok(exe)
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "wyvernscan.exe" } else { "wyvernscan" }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
fn windows_shortcut(special_folder: &str, exe: &Path) -> io::Result<()> {
    // Paths travel in env vars, not in the script text, so no quoting can go wrong.
    let script = "$ErrorActionPreference='Stop'; \
        $f=[Environment]::GetFolderPath($env:WS_FOLDER); \
        $s=(New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $f 'WyvernScan.lnk')); \
        $s.TargetPath=$env:WS_EXE; $s.WorkingDirectory=Split-Path $env:WS_EXE; $s.Save()";
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("WS_FOLDER", special_folder)
        .env("WS_EXE", exe)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

#[cfg(windows)]
fn menu_shortcut(_dir: &Path, exe: &Path) -> io::Result<()> {
    windows_shortcut("Programs", exe) // the per-user Start menu's Programs folder
}

#[cfg(windows)]
fn desktop_shortcut(_dir: &Path, exe: &Path) -> io::Result<()> {
    windows_shortcut("Desktop", exe) // asked of Windows, so OneDrive-redirected desktops work
}

/// "Run this program as an administrator" is stored per user as a compatibility flag
/// keyed by the exe's path, so it needs no admin rights to set and it survives the
/// updater swapping the exe. It covers every way of starting the app (shortcuts,
/// Explorer, Run), not just our own shortcuts.
#[cfg(windows)]
const LAYERS_KEY: &str = r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";

#[cfg(windows)]
fn run_as_admin(exe: &Path, on: bool) -> io::Result<()> {
    // shortcut: this replaces (or, when unticked, removes) any other compatibility flags
    // already set on this exe; fine for a fresh per-user install, revisit if that matters.
    let mut reg = Command::new("reg");
    if on {
        reg.args(["add", LAYERS_KEY, "/v"]).arg(exe).args(["/t", "REG_SZ", "/d", "~ RUNASADMIN", "/f"]);
    } else {
        reg.args(["delete", LAYERS_KEY, "/v"]).arg(exe).arg("/f");
    }
    let out = reg.output()?;
    // Deleting a flag that was never set fails, and that is exactly the wanted end state.
    if out.status.success() || !on {
        Ok(())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

/// Whether an earlier install already set the flag, so reinstalling doesn't silently undo it.
#[cfg(windows)]
fn admin_enabled(dir: &Path) -> bool {
    Command::new("reg")
        .args(["query", LAYERS_KEY, "/v"])
        .arg(dir.join(exe_name()))
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("RUNASADMIN"))
}

#[cfg(not(windows))]
fn admin_enabled(_dir: &Path) -> bool {
    false
}

// ------------------------------------------------------------------ Linux

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_entry(dir: &Path, exe: &Path) -> String {
    // shortcut: no escaping of quotes, backslashes, `$` or `%` in these paths;
    // add it if anyone ever has such a home directory.
    format!(
        "[Desktop Entry]\nType=Application\nName=WyvernScan\nComment=Disk space explorer\n\
         Exec=\"{}\"\nIcon={}\nCategories=Utility;System;\nTerminal=false\n",
        exe.display(),
        dir.join("icon.png").display()
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn menu_shortcut(dir: &Path, exe: &Path) -> io::Result<()> {
    // dir is <data home>/WyvernScan, and menus read <data home>/applications.
    let apps = dir.parent().unwrap_or(dir).join("applications");
    fs::create_dir_all(&apps)?;
    fs::write(apps.join("wyvernscan.desktop"), desktop_entry(dir, exe))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_shortcut(dir: &Path, exe: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let desk = Command::new("xdg-user-dir")
        .arg("DESKTOP")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join("Desktop")))
        .filter(|d| d.is_dir())
        .ok_or_else(|| io::Error::other("no Desktop folder found"))?;
    let file = desk.join("wyvernscan.desktop");
    fs::write(&file, desktop_entry(dir, exe))?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
    // GNOME also wants the launcher marked trusted; harmless elsewhere, so best effort.
    let _ = Command::new("gio")
        .arg("set")
        .arg(&file)
        .args(["metadata::trusted", "true"])
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

// ------------------------------------------------------------------ macOS

#[cfg(target_os = "macos")]
const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>WyvernScan</string>
<key>CFBundleIdentifier</key><string>de.dragon99z.wyvernscan</string>
<key>CFBundleExecutable</key><string>wyvernscan</string>
<key>CFBundleIconFile</key><string>icon</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
"#;

/// Only the Dock/Finder icon is at stake here, so the caller reports a failure as a warning.
#[cfg(target_os = "macos")]
fn macos_icon(app: &Path) -> io::Result<()> {
    let res = app.join("Contents/Resources");
    fs::write(res.join("icon.png"), ICON)?;
    let ok = Command::new("sips")
        .args(["-s", "format", "icns"])
        .arg(res.join("icon.png"))
        .arg("--out")
        .arg(res.join("icon.icns"))
        .output()
        .map(|o| o.status.success());
    let _ = fs::remove_file(res.join("icon.png"));
    if ok? { Ok(()) } else { Err(io::Error::other("sips could not convert the icon")) }
}

#[cfg(target_os = "macos")]
fn menu_shortcut(_dir: &Path, _exe: &Path) -> io::Result<()> {
    Ok(()) // unreachable: MENU_LABEL is None on macOS
}

#[cfg(target_os = "macos")]
fn desktop_shortcut(dir: &Path, _exe: &Path) -> io::Result<()> {
    let desk = env::var_os("HOME").map(|h| PathBuf::from(h).join("Desktop"));
    let link = desk.filter(|d| d.is_dir()).ok_or_else(|| io::Error::other("no Desktop folder found"))?;
    let link = link.join("WyvernScan.app");
    let _ = fs::remove_file(&link); // replace an older link; never touches a real folder
    std::os::unix::fs::symlink(dir, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reinstall_replaces_the_app_and_keeps_the_old_copy() {
        let dir = env::temp_dir().join(format!("wyvernscan-setup-test-{}", std::process::id()));
        let exe = install_files(&dir).unwrap();
        fs::write(&exe, b"previous version").unwrap();
        let exe2 = install_files(&dir).unwrap(); // takes the rename-aside path
        assert_eq!(exe, exe2);
        assert_eq!(fs::read(&exe).unwrap(), APP);
        assert_eq!(fs::read(exe.with_extension("old")).unwrap(), b"previous version");
        fs::remove_dir_all(&dir).unwrap();
    }
}
