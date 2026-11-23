//! Update check and one-click install, through Velopack (velopack.io). Only a copy Velopack
//! installed takes part (a build-folder copy, or a Mac copy still on the disk image, has nothing
//! to replace). Updates come from the GitHub releases of TooCoolCello/tacet-releases, a public
//! repo that holds only the releases (the source repo is private): releases.win.json (or
//! releases.osx.json) and the .nupkg packages beside them, whose checksums Velopack checks. A
//! test build reads a local folder instead (os::TEST_FEED).
//!
//! On a Mac, one-click updates wait for the `mac-updates` feature (off until they've been tried
//! on a Mac; build-mac.sh turns it on with MICCLEAN_MAC_UPDATES=1). Without it a Mac copy in
//! Applications does what 0.11 did: it asks GitHub for the newest release, and Download opens
//! that release's MicClean.dmg in the browser (`dmg`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp, sources};

pub const REPO_URL: &str = "https://github.com/TooCoolCello/tacet-releases";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Velopack installs updates. Off on a Mac without the `mac-updates` feature: then an update is
/// a download you drag in, so it's never started for you.
pub const ONE_CLICK: bool = cfg!(any(windows, feature = "mac-updates"));
/// What updating does, after "You have 0.x.y."
pub const HOW: &str = if ONE_CLICK {
    "Updating closes MicClean for a few seconds, so your voice drops out briefly."
} else {
    "Download it, then drag it into Applications over this one."
};

/// Velopack restarted this copy after applying an update (or after failing to).
static RESTARTED: AtomicBool = AtomicBool::new(false);

/// Velopack's start-up work; the first thing in main. Its installer and updater run this exe
/// with --veloapp-* arguments at install, update and uninstall time, and this answers them and
/// exits. A downloaded update is only applied when MicClean asks (`install`), never on its own
/// at start-up: plugin hosts are copies of this exe too, and one must never restart the app.
pub fn startup() {
    let mut app = VelopackApp::build().set_auto_apply_on_startup(false).on_restarted(|_| RESTARTED.store(true, Ordering::Relaxed));
    // Uninstalling: a running copy closes and "Start with Windows" goes too (Velopack removes
    // the rest). No UI here, so your settings stay in %APPDATA%\MicClean.
    #[cfg(windows)]
    {
        app = app.on_before_uninstall_fast_callback(|_| crate::install::before_uninstall());
    }
    app.run();
}

pub fn restarted() -> bool {
    RESTARTED.load(Ordering::Relaxed)
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    get: Get,
}

#[derive(Clone, Debug)]
enum Get {
    Velopack(Arc<UpdateInfo>),
    /// The release's MicClean.dmg, for the browser (a Mac without one-click updates).
    Browser(String),
}

impl PartialEq for Release {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
    }
}

#[derive(Clone, PartialEq, Debug, Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    /// Downloading the update.
    Installing,
    /// Velopack's updater is waiting for this copy to close, so it can be replaced.
    Restarting,
    Failed(String),
}

/// Shared with the worker threads; the GUI polls `state()` every frame.
#[derive(Clone, Default)]
pub struct Updater {
    state: Arc<Mutex<State>>,
    /// None when this copy isn't one Velopack installed, or doesn't use Velopack (`ONE_CLICK`).
    um: Option<UpdateManager>,
    installed: bool,
}

impl Updater {
    pub fn new() -> Self {
        let source: Box<dyn sources::UpdateSource> = match crate::os::TEST_FEED {
            Some(dir) => Box::new(sources::FileSource::new(dir)),
            None => Box::new(sources::GithubSource::new(REPO_URL, None, false)),
        };
        let um = if ONE_CLICK { UpdateManager::new_boxed(source, None, None).ok() } else { None };
        #[cfg(windows)]
        let installed = um.is_some();
        // A Mac copy run from the disk image can't replace itself.
        #[cfg(not(windows))]
        let installed = crate::os::installed_copy() && (um.is_some() || !ONE_CLICK);
        Self { state: Default::default(), um: um.filter(|_| installed), installed }
    }

    /// Only the installed copy can replace itself, so only it checks.
    pub fn installed(&self) -> bool {
        self.installed
    }

    pub fn state(&self) -> State {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set(&self, s: State) {
        if let Ok(mut st) = self.state.lock() {
            *st = s;
        }
    }

    /// Asks the update feed for the newest release, in the background. No-op while one is in
    /// flight.
    pub fn check(&self) {
        if !self.installed || matches!(self.state(), State::Checking | State::Installing | State::Restarting) {
            return;
        }
        self.set(State::Checking);
        let me = self.clone();
        std::thread::spawn(move || {
            let found = match &me.um {
                Some(um) => match um.check_for_updates() {
                    Ok(UpdateCheck::UpdateAvailable(info)) => {
                        Ok(Some(Release { version: info.TargetFullRelease.Version.clone(), get: Get::Velopack(Arc::from(info)) }))
                    }
                    Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => Ok(None),
                    Err(e) => Err(e.to_string()),
                },
                None => dmg::latest(),
            };
            me.set(match found {
                Ok(Some(r)) => State::Available(r),
                Ok(None) => State::UpToDate,
                Err(e) => State::Failed(e),
            });
        });
    }

    /// Downloads the update, then hands it to Velopack's updater. The state turns `Restarting`:
    /// this copy then closes itself (saving your settings), and the updater swaps the new
    /// version in and starts it, back in the tray if `minimized`. If the swap fails, the
    /// updater starts this version again.
    pub fn install(&self, rel: Release, minimized: bool) {
        let info = match &rel.get {
            Get::Velopack(info) => info.clone(),
            Get::Browser(url) => return crate::os::open(url),
        };
        let Some(um) = self.um.clone() else { return };
        if matches!(self.state(), State::Installing | State::Restarting) {
            return;
        }
        self.set(State::Installing);
        let me = self.clone();
        std::thread::spawn(move || {
            me.set(match download_and_apply(&um, &info, minimized) {
                Ok(()) => State::Restarting,
                Err(e) => State::Failed(e),
            });
        });
    }
}

fn download_and_apply(um: &UpdateManager, info: &UpdateInfo, minimized: bool) -> Result<(), String> {
    um.download_updates(info, None).map_err(|e| format!("couldn't download it: {e}"))?;
    // Noted before the updater runs, so not even one that dies silently gets retried forever.
    let target = &info.TargetFullRelease;
    std::fs::write(attempt_file(), &target.Version).map_err(|e| format!("couldn't note the update: {e}"))?;
    let args: &[&str] = if minimized { &["--minimized"] } else { &[] };
    um.wait_exit_then_apply_updates(target, true, true, args)
        .map_err(|e| format!("couldn't start the updater: {e}"))
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

/// The 0.11 way, for a Mac without one-click updates: GitHub's newest release, if it's newer, and
/// its MicClean.dmg. curl ships with macOS, so no HTTP library is linked in for this.
// ponytail: compiled on Windows too, unused there, so its tests run on every build.
#[cfg_attr(windows, allow(dead_code))]
mod dmg {
    use super::{Get, Release, VERSION};
    use serde::Deserialize;

    /// The releases repo (`REPO_URL`), not the private source repo.
    const REPO: &str = "TooCoolCello/tacet-releases";
    const DMG: &str = "MicClean.dmg";

    pub fn latest() -> Result<Option<Release>, String> {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let out = std::process::Command::new("/usr/bin/curl")
            .args(["--silent", "--show-error", "--fail", "--location", "--connect-timeout", "30", "--max-time", "120"])
            .args(["-H", "Accept: application/vnd.github+json", "-H", "User-Agent: MicClean", &url])
            .output()
            .map_err(|e| format!("couldn't run curl: {e}"))?;
        if out.status.success() {
            return parse(&out.stdout, VERSION);
        }
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.lines().last().unwrap_or("download failed").trim();
        if err.contains("404") { Ok(None) } else { Err(err.to_string()) } // 404: nothing released yet
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

    /// The newest release if it is newer than `current`. The download link is only accepted when
    /// it points into the releases repo's own releases, whatever the reply says.
    pub fn parse(json: &[u8], current: &str) -> Result<Option<Release>, String> {
        let gh: Gh = serde_json::from_slice(json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
        if version(&gh.tag_name)? <= version(current)? {
            return Ok(None);
        }
        let prefix = format!("https://github.com/{REPO}/releases/download/");
        let url = gh
            .assets
            .iter()
            .find(|a| a.name == DMG && a.browser_download_url.starts_with(&prefix))
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| format!("release {} has no {DMG}", gh.tag_name))?;
        Ok(Some(Release { version: gh.tag_name.trim_start_matches('v').to_string(), get: Get::Browser(url) }))
    }

    pub fn version(s: &str) -> Result<[u32; 3], String> {
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn release(tag: &str, host: &str) -> String {
            let url = format!("https://{host}/{REPO}/releases/download/{tag}/{DMG}");
            format!(r#"{{"tag_name":"{tag}","html_url":"x","assets":[{{"name":"{DMG}","browser_download_url":"{url}"}}]}}"#)
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
            assert!(matches!(&r.get, Get::Browser(u) if u.ends_with(&format!("/v0.2.0/{DMG}"))));
            assert_eq!(parse(release("v0.1.0", "github.com").as_bytes(), "0.1.0").unwrap(), None, "same version");
            assert_eq!(parse(release("v0.0.9", "github.com").as_bytes(), "0.1.0").unwrap(), None, "older version");
        }

        #[test]
        fn links_outside_this_repo_are_refused() {
            let err = parse(release("v0.2.0", "evil.example").as_bytes(), "0.1.0").unwrap_err();
            assert!(err.contains("has no"), "{err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy run from the build folder isn't installed: it never checks or updates.
    #[test]
    fn a_build_folder_copy_doesnt_update() {
        let u = Updater::new();
        assert!(!u.installed());
        u.check();
        assert_eq!(u.state(), State::Idle);
    }
}
