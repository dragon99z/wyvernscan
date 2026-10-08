//! Self-update from the newest GitHub release.
//!
//! No HTTP/TLS crate on purpose (see AGENTS.md "No new dependency"): it shells
//! out to `curl` and `tar`, which ship with Windows 10+, macOS and practically
//! every Linux. The asset names below must match what `.github/workflows/build.yml`
//! publishes. Nothing here may touch `eframe`/`egui` (the CLI uses it too).
//!
//! Trust model: HTTPS to github.com is the only integrity check; there is no
//! signature verification.

use anyhow::{bail, Context, Result};
use std::{fs, process::Command};

const LATEST: &str = "https://api.github.com/repos/dragon99z/wyvernscan/releases/latest";

pub enum Outcome {
    UpToDate,
    Updated(String),
}

/// The release asset for this platform, or `None` when CI doesn't build one.
fn asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("wyvernscan.exe"),
        ("linux", "x86_64") => Some("wyvernscan-linux-x86_64.tar.gz"),
        ("macos", _) => Some("wyvernscan-macos-universal.tar.gz"),
        _ => None,
    }
}

fn parse(v: &str) -> Option<Vec<u64>> {
    v.trim_start_matches('v').split('.').map(|p| p.parse().ok()).collect()
}

/// Tags that don't parse (`v1.0-beta`) are never "newer", so they can't
/// downgrade or loop anyone.
fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(a), Some(b)) if a > b)
}

fn run(cmd: &mut Command) -> Result<Vec<u8>> {
    // A GUI-subsystem exe would otherwise flash a console window per child.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().context("could not run curl/tar")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    run(Command::new("curl")
        .args(["-fsSL", "--max-time", "300", "-H", "User-Agent: wyvernscan"])
        .args(args))
}

/// Checks GitHub and, if a newer release exists, replaces the running
/// executable with it. The new version takes effect on the next start.
pub fn update() -> Result<Outcome> {
    let exe = std::env::current_exe()?;
    let old = exe.with_extension("old");
    let new = exe.with_extension("new");
    let _ = fs::remove_file(&old); // leftover from a previous Windows update

    let asset = asset().context("no prebuilt release for this platform")?;
    let rel: serde_json::Value = serde_json::from_slice(&curl(&[LATEST])?)?;
    let tag = rel["tag_name"].as_str().context("no release found")?;
    if !is_newer(tag, env!("CARGO_PKG_VERSION")) {
        return Ok(Outcome::UpToDate);
    }
    let url = rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"] == asset)
        .and_then(|a| a["browser_download_url"].as_str())
        .with_context(|| format!("release {tag} has no {asset}"))?;

    let bin = if asset.ends_with(".tar.gz") {
        let tgz = std::env::temp_dir().join("wyvernscan-update.tar.gz");
        curl(&["-o", &tgz.to_string_lossy(), url])?;
        let bin = run(Command::new("tar").arg("-xzOf").arg(&tgz).arg("wyvernscan"));
        let _ = fs::remove_file(&tgz);
        bin?
    } else {
        curl(&[url])?
    };
    if bin.is_empty() {
        bail!("downloaded file is empty");
    }

    fs::write(&new, &bin)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        // A running .exe can be renamed but not overwritten.
        fs::rename(&exe, &old)?;
        if let Err(e) = fs::rename(&new, &exe) {
            let _ = fs::rename(&old, &exe);
            return Err(e.into());
        }
    } else {
        fs::rename(&new, &exe)?; // atomic, fine while running
    }
    Ok(Outcome::Updated(tag.to_string()))
}

/// `wyvernscan --update`: returns the process exit code.
pub fn run_cli() -> i32 {
    match update() {
        Ok(Outcome::UpToDate) => {
            println!("WyvernScan v{} is up to date.", env!("CARGO_PKG_VERSION"));
            0
        }
        Ok(Outcome::Updated(tag)) => {
            println!("Updated to {tag}. It takes effect the next time you start WyvernScan.");
            0
        }
        Err(e) => {
            eprintln!("Update failed: {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_compare() {
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.10.0", "0.9.0")); // numeric, not string, compare
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(!is_newer("nightly", "0.1.1"));
        assert!(!is_newer("v0.2.0-beta", "0.1.1"));
    }
}
