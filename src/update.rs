//! Update check and one-click install, through Velopack (velopack.io), on Windows and on a Mac.
//! Only a copy Velopack installed takes part (a build-folder copy, or a Mac copy still on the
//! disk image, has nothing to replace). Updates come from this repo's GitHub releases:
//! releases.win.json / releases.osx.json and the .nupkg packages beside them, whose checksums
//! Velopack checks. A test build reads a local folder instead (os::TEST_FEED).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp, sources};

pub const REPO_URL: &str = "https://github.com/TooCoolCello/micclean";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// What updating does, after "You have 0.x.y."
pub const HOW: &str = "Updating closes MicClean for a few seconds, so your voice drops out briefly.";

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
    info: Arc<UpdateInfo>,
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
    /// None when this copy isn't one Velopack installed.
    um: Option<UpdateManager>,
}

impl Updater {
    pub fn new() -> Self {
        let source: Box<dyn sources::UpdateSource> = match crate::os::TEST_FEED {
            Some(dir) => Box::new(sources::FileSource::new(dir)),
            None => Box::new(sources::GithubSource::new(REPO_URL, None, false)),
        };
        let um = UpdateManager::new_boxed(source, None, None).ok();
        // A Mac copy run from the disk image can't replace itself.
        #[cfg(not(windows))]
        let um = um.filter(|_| crate::os::installed_copy());
        Self { state: Default::default(), um }
    }

    /// Only the installed copy can replace itself, so only it checks.
    pub fn installed(&self) -> bool {
        self.um.is_some()
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
        let Some(um) = self.um.clone() else { return };
        if matches!(self.state(), State::Checking | State::Installing | State::Restarting) {
            return;
        }
        self.set(State::Checking);
        let me = self.clone();
        std::thread::spawn(move || {
            me.set(match um.check_for_updates() {
                Ok(UpdateCheck::UpdateAvailable(info)) => {
                    State::Available(Release { version: info.TargetFullRelease.Version.clone(), info: Arc::from(info) })
                }
                Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => State::UpToDate,
                Err(e) => State::Failed(e.to_string()),
            });
        });
    }

    /// Downloads the update, then hands it to Velopack's updater. The state turns `Restarting`:
    /// this copy then closes itself (saving your settings), and the updater swaps the new
    /// version in and starts it, back in the tray if `minimized`. If the swap fails, the
    /// updater starts this version again.
    pub fn install(&self, rel: Release, minimized: bool) {
        let Some(um) = self.um.clone() else { return };
        if matches!(self.state(), State::Installing | State::Restarting) {
            return;
        }
        self.set(State::Installing);
        let me = self.clone();
        std::thread::spawn(move || {
            me.set(match download_and_apply(&um, &rel, minimized) {
                Ok(()) => State::Restarting,
                Err(e) => State::Failed(e),
            });
        });
    }
}

fn download_and_apply(um: &UpdateManager, rel: &Release, minimized: bool) -> Result<(), String> {
    um.download_updates(&rel.info, None).map_err(|e| format!("couldn't download it: {e}"))?;
    // Noted before the updater runs, so not even one that dies silently gets retried forever.
    std::fs::write(attempt_file(), &rel.version).map_err(|e| format!("couldn't note the update: {e}"))?;
    let args: &[&str] = if minimized { &["--minimized"] } else { &[] };
    um.wait_exit_then_apply_updates(&rel.info.TargetFullRelease, true, true, args)
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
