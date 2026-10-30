//! Update check and one-click install from GitHub Releases. Only the installed copy takes part
//! (a build-folder copy has nothing to replace). curl.exe ships with Windows 10 and 11, so no
//! HTTP library is linked in.

use crate::install;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

pub const REPO: &str = "TooCoolCello/micclean";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const SETUP: &str = "MicClean-Setup.exe";

#[derive(Clone, PartialEq, Debug)]
pub struct Release {
    pub version: String,
    setup_url: String,
    sha_url: String,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing,
    /// Setup is running and waiting for this copy to close, so it can be replaced.
    Restarting,
    Failed(String),
}

/// Shared with the worker threads; the GUI polls `state()` every frame.
#[derive(Clone, Default)]
pub struct Updater(Arc<Mutex<State>>);

impl Updater {
    pub fn state(&self) -> State {
        self.0.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set(&self, s: State) {
        if let Ok(mut st) = self.0.lock() {
            *st = s;
        }
    }

    /// Asks GitHub for the newest release, in the background. No-op while one is in flight.
    pub fn check(&self) {
        if matches!(self.state(), State::Checking | State::Installing | State::Restarting) {
            return;
        }
        self.set(State::Checking);
        let me = self.clone();
        std::thread::spawn(move || {
            // Leftover from the last update; it may still be locked, then the next check gets it.
            let _ = std::fs::remove_dir_all(install::update_dir());
            me.set(match latest() {
                Ok(Some(r)) => State::Available(r),
                Ok(None) => State::UpToDate,
                Err(e) => State::Failed(e),
            });
        });
    }

    /// Downloads and checks the setup file, then runs it. The state turns `Restarting` once it
    /// runs: this copy then closes itself (saving your settings), and setup installs and starts
    /// the new one, back in the tray if `minimized`.
    pub fn install(&self, rel: Release, minimized: bool) {
        if matches!(self.state(), State::Installing | State::Restarting) {
            return;
        }
        self.set(State::Installing);
        let me = self.clone();
        std::thread::spawn(move || {
            me.set(match download_and_run(&rel, minimized) {
                Ok(()) => State::Restarting,
                Err(e) => State::Failed(e),
            });
        });
    }
}

/// Why setup couldn't install an update, once: setup started this copy again after it failed.
pub fn take_failure() -> Option<String> {
    let note = install::failure_note();
    let why = std::fs::read_to_string(&note).ok()?;
    let _ = std::fs::remove_file(note);
    Some(why)
}

/// The version the last update tried to install. It stays until this copy is that version,
/// so an update that didn't take is never retried on its own, launch after launch.
fn attempt_file() -> std::path::PathBuf {
    crate::logfile::path().with_file_name("update-attempt.txt")
}

/// A version an update tried and failed to install (Settings can still try it by hand).
pub fn failed_version() -> Option<String> {
    let v = std::fs::read_to_string(attempt_file()).ok()?.trim().to_string();
    if v == VERSION {
        let _ = std::fs::remove_file(attempt_file()); // it worked
        return None;
    }
    Some(v)
}

/// Gives up on a connection that stalls (under 1 kB/s for a minute), not on a slow one: a
/// fixed time limit made a 12 MB download fail every time under about 0.8 Mbit/s.
// ponytail: no --ssl-revoke-best-effort (for networks that block revocation checks): the curl
// in older Windows 10 builds (7.55) doesn't know it and would fail every check.
fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let opts = ["--silent", "--show-error", "--fail", "--location", "--connect-timeout", "30", "--speed-limit", "1024", "--speed-time", "60"];
    let out = install::hidden(Command::new(install::windows_exe(install::CURL)).args(opts).args(args))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err.lines().last().unwrap_or("download failed").trim().to_string())
}

fn latest() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    match curl(&["-H", "Accept: application/vnd.github+json", "-H", "User-Agent: MicClean", &url]) {
        Ok(json) => parse(&json, VERSION),
        Err(e) if e.contains("404") => Ok(None), // nothing released yet
        Err(e) => Err(e),
    }
}

#[derive(Deserialize)]
struct Gh {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The newest release if it is newer than `current`. Download links are only accepted when they
/// point into this repo's own releases, whatever the reply says.
fn parse(json: &[u8], current: &str) -> Result<Option<Release>, String> {
    let gh: Gh = serde_json::from_slice(json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    if version(&gh.tag_name)? <= version(current)? {
        return Ok(None);
    }
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let asset = |name: &str| {
        gh.assets
            .iter()
            .find(|a| a.name == name && a.browser_download_url.starts_with(&prefix))
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| format!("release {} has no {name}", gh.tag_name))
    };
    Ok(Some(Release {
        version: gh.tag_name.trim_start_matches('v').to_string(),
        setup_url: asset(SETUP)?,
        sha_url: asset(&format!("{SETUP}.sha256"))?,
    }))
}

fn version(s: &str) -> Result<[u32; 3], String> {
    let parts: Vec<&str> = s.trim_start_matches('v').split('.').collect();
    let odd = || format!("odd version number: {s}");
    if parts.len() > 3 {
        return Err(odd());
    }
    let mut v = [0; 3];
    for (slot, p) in v.iter_mut().zip(parts) {
        *slot = p.parse().map_err(|_| odd())?;
    }
    Ok(v)
}

/// The hash from a `sha256sum`-style file ("<hash>  <name>") or a bare hash.
fn first_hash(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

fn download_and_run(rel: &Release, minimized: bool) -> Result<(), String> {
    let dir = install::update_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let setup = dir.join(SETUP);
    curl(&["-o", &setup.to_string_lossy(), &rel.setup_url])?;
    let want = first_hash(&String::from_utf8_lossy(&curl(&[&rel.sha_url])?)).ok_or("the checksum file is malformed")?;
    let bytes = std::fs::read(&setup).map_err(|e| e.to_string())?;
    let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != want {
        let _ = std::fs::remove_file(&setup);
        return Err("the download didn't match its checksum, so nothing was installed".into());
    }
    // Noted before setup runs, so not even a setup that dies silently gets retried forever.
    std::fs::write(attempt_file(), &rel.version).map_err(|e| format!("couldn't note the update: {e}"))?;
    let started = install::setup_started();
    let _ = std::fs::remove_file(&started);
    let pid = std::process::id().to_string();
    let mut cmd = Command::new(&setup);
    cmd.args(["--silent", "--launch", "--wait", &pid]);
    if minimized {
        cmd.arg("--minimized");
    }
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start setup: {e}"))?;
    // This copy closes only once setup is really running: one that antivirus stops before it
    // starts would leave nobody to start MicClean again.
    for _ in 0..300 {
        if started.exists() {
            return Ok(());
        }
        if let Ok(Some(code)) = child.try_wait() {
            return Err(format!("setup stopped before it started ({code})"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    Err("setup didn't start within 30 seconds".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, host: &str) -> String {
        let url = |n: &str| format!("https://{host}/{REPO}/releases/download/{tag}/{n}");
        format!(
            r#"{{"tag_name":"{tag}","html_url":"x","assets":[{{"name":"{SETUP}","browser_download_url":"{}"}},{{"name":"{SETUP}.sha256","browser_download_url":"{}"}}]}}"#,
            url(SETUP),
            url(&format!("{SETUP}.sha256"))
        )
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version("v0.10.0").unwrap() > version("0.9.5").unwrap());
        assert!(version("v1.0").unwrap() > version("0.99.99").unwrap());
        assert!(version("v0.1.1").unwrap() > version("0.1.0").unwrap());
        assert!(version("v1.2.x").is_err());
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let r = parse(release("v0.2.0", "github.com").as_bytes(), "0.1.0").unwrap().expect("newer");
        assert_eq!(r.version, "0.2.0");
        assert!(r.setup_url.ends_with("/v0.2.0/MicClean-Setup.exe"));
        assert_eq!(parse(release("v0.1.0", "github.com").as_bytes(), "0.1.0").unwrap(), None, "same version");
        assert_eq!(parse(release("v0.0.9", "github.com").as_bytes(), "0.1.0").unwrap(), None, "older version");
    }

    #[test]
    fn links_outside_this_repo_are_refused() {
        let err = parse(release("v0.2.0", "evil.example").as_bytes(), "0.1.0").unwrap_err();
        assert!(err.contains("has no"), "{err}");
    }

    #[test]
    fn checksum_file_formats() {
        let h = "A".repeat(64);
        assert_eq!(first_hash(&format!("{h}  MicClean-Setup.exe\r\n")), Some("a".repeat(64)));
        assert_eq!(first_hash(&h), Some("a".repeat(64)));
        assert_eq!(first_hash("<html>not found</html>"), None);
        assert_eq!(first_hash(&"g".repeat(64)), None);
    }

    /// Needs the network and a published release: `cargo test -- --ignored live_check`.
    #[test]
    #[ignore]
    fn live_check() {
        println!("{:?}", latest());
    }
}
