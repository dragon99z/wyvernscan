//! WyvernScan setup: one self-contained file that carries the app and installs it
//! into the per-user app folder (no admin rights needed):
//!   Windows  %LOCALAPPDATA%\WyvernScan\
//!   Linux    $XDG_DATA_HOME/WyvernScan/ (default ~/.local/share/WyvernScan/)
//!   macOS    ~/Applications/WyvernScan.app
//! Std only, like the app itself. The few things std can't do (a Windows .lnk,
//! a macOS .icns) shell out to the OS tools (PowerShell, sips), as updater.rs
//! does with curl.
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

// Copied here by CI (or by hand) from target/release/ before building this crate.
static APP: &[u8] = include_bytes!("../payload/wyvernscan");
#[cfg(unix)]
static ICON: &[u8] = include_bytes!("../../assets/icon-512.png");

fn main() {
    let r = run();
    if let Err(e) = &r {
        eprintln!("\nInstall failed: {e}");
    }
    // Double-clicked on Windows, the console would vanish before it can be read.
    #[cfg(windows)]
    {
        print!("\nPress Enter to close...");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
    std::process::exit(r.is_err() as i32);
}

fn run() -> io::Result<()> {
    let dir = install_dir()?;
    println!("WyvernScan setup\n\nInstalling to {}\n", dir.display());

    // Asked before anything is written, so Ctrl+C here leaves nothing behind.
    let menu = MENU_QUESTION.is_some_and(|q| ask(q, true));
    let desktop = ask("Create a desktop shortcut?", false);

    let exe = install_files(&dir)?;
    println!("Installed {}", exe.display());

    // A failed shortcut shouldn't undo a good install: warn and carry on.
    if menu {
        report("Start menu / application menu shortcut", menu_shortcut(&dir, &exe));
    }
    if desktop {
        report("Desktop shortcut", desktop_shortcut(&dir, &exe));
    }
    println!("\nDone.");
    Ok(())
}

fn report(what: &str, r: io::Result<()>) {
    match r {
        Ok(()) => println!("Created: {what}"),
        Err(e) => eprintln!("Warning: {what} not created: {e}"),
    }
}

/// Anything but yes/no (or an empty/closed stdin, as in a piped install) keeps the default.
fn parse_yes(answer: &str, default: bool) -> bool {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

fn ask(question: &str, default: bool) -> bool {
    print!("{question} [{}] ", if default { "Y/n" } else { "y/N" });
    let _ = io::stdout().flush();
    let mut s = String::new();
    let _ = io::stdin().read_line(&mut s);
    parse_yes(&s, default)
}

// macOS has no menu question: an app in ~/Applications already shows up in Launchpad.
#[cfg(windows)]
const MENU_QUESTION: Option<&str> = Some("Create a Start menu shortcut?");
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_QUESTION: Option<&str> = Some("Add WyvernScan to your application menu?");
#[cfg(target_os = "macos")]
const MENU_QUESTION: Option<&str> = None;

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

    let exe = bin_dir.join(if cfg!(windows) { "wyvernscan.exe" } else { "wyvernscan" });
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
    macos_bundle_files(dir);
    Ok(exe)
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
fn macos_bundle_files(app: &Path) {
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
    let res = app.join("Contents/Resources");
    let r = fs::create_dir_all(&res)
        .and_then(|_| fs::write(app.join("Contents/Info.plist"), PLIST))
        .and_then(|_| fs::write(res.join("icon.png"), ICON));
    // Only the Dock/Finder icon is at stake from here on, so failures just warn.
    let icns = Command::new("sips")
        .args(["-s", "format", "icns"])
        .arg(res.join("icon.png"))
        .arg("--out")
        .arg(res.join("icon.icns"))
        .output();
    let _ = fs::remove_file(res.join("icon.png"));
    if r.is_err() || !icns.is_ok_and(|o| o.status.success()) {
        eprintln!("Warning: app icon not created (the app will use a generic icon)");
    }
}

#[cfg(target_os = "macos")]
fn menu_shortcut(_dir: &Path, _exe: &Path) -> io::Result<()> {
    Ok(()) // unreachable: MENU_QUESTION is None on macOS
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
    fn answers_fall_back_to_the_default() {
        assert!(parse_yes("Y\n", false) && parse_yes(" yes ", false));
        assert!(!parse_yes("n", true) && !parse_yes("NO\r\n", true));
        assert!(parse_yes("", true) && !parse_yes("", false) && parse_yes("maybe", true));
    }

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
