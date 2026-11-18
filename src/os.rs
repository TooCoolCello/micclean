//! What differs between Windows and macOS outside the audio code: folders, the one-copy lock,
//! starting at sign-in, opening things in the system's own apps, and the words for each.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A test build, made with MICCLEAN_TEST_FEED=<folder> set while compiling, updates from that
/// folder (a `vpk pack` output) instead of GitHub. It is a separate app beside the real one:
/// its own NAME (so its own settings, one-copy lock, Velopack install and Start menu entry),
/// it never opens an audio device and never starts at sign-in. Release builds have no feed.
pub const TEST_FEED: Option<&str> = option_env!("MICCLEAN_TEST_FEED");
pub const NAME: &str = if TEST_FEED.is_some() { "MicCleanTest" } else { "MicClean" };

/// %APPDATA%\MicClean on Windows, ~/Library/Application Support/MicClean on a Mac: the log,
/// backups and update notes. The settings live in its data folder.
pub fn app_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    #[cfg(not(windows))]
    let base = std::env::home_dir().unwrap_or_default().join("Library").join("Application Support");
    base.join(NAME)
}

/// Where eframe keeps app.ron. eframe's own choice is this on Windows but the bare app folder
/// on a Mac; MicClean uses the data folder on both, so the backups folder sits beside it.
pub fn settings_dir() -> PathBuf {
    app_dir().join("data")
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::process::CommandExt;

    pub const SOUND_SETTINGS: &str = "Sound settings";
    pub const SET_48K: &str = "In Sound settings, set it to 48000 Hz under Properties → Advanced.";
    pub const AUTOSTART: &str = "Start with Windows";
    pub const TRAY: &str = "tray";
    pub const THIS_PC: &str = "this PC";

    /// One copy per Windows session: a named mutex in the session's own (Local\) namespace, so
    /// another Windows user (fast user switching, Remote Desktop) runs their own copy. The first
    /// copy keeps its handle until it exits; a later one closes its own at once, or it would
    /// keep the mutex alive after the first one has gone.
    pub fn first_copy() -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = format!("Local\\{NAME}\0").encode_utf16().collect();
        // SAFETY: a NUL-terminated name that outlives the call; the handle is only closed here.
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if !h.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return false;
            }
        }
        true
    }

    /// The Windows session number, so each signed-in user gets their own window-ping port.
    pub fn session() -> u32 {
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        let mut session = 0u32;
        // SAFETY: writes one u32 into `session`.
        unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
        session
    }

    /// A web page or folder, in the browser or Explorer.
    pub fn open(target: impl AsRef<OsStr>) {
        let _ = Command::new(crate::install::windows_exe("explorer.exe")).arg(target).spawn();
    }

    /// A file, selected in its Explorer folder.
    pub fn reveal(path: &Path) {
        // raw_arg: explorer wants /select,"path" exactly, which normal argument quoting breaks.
        let _ = Command::new(crate::install::windows_exe("explorer.exe")).raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }

    /// Windows' classic Sound dialog, where Properties → Advanced sets a device's rate.
    pub fn sound_settings() {
        let _ = Command::new(crate::install::windows_exe(crate::install::CONTROL)).arg("mmsys.cpl").spawn();
    }

    /// "Start with Windows" = a value under the per-user Run key, written with reg.exe.
    pub mod autostart {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        fn reg(args: &[&str]) -> bool {
            Command::new(crate::install::windows_exe(crate::install::REG))
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .is_ok_and(|s| s.success())
        }

        pub fn enabled() -> bool {
            reg(&["query", KEY, "/v", super::NAME])
        }

        pub fn set(on: bool) -> bool {
            if !on {
                return reg(&["delete", KEY, "/v", super::NAME, "/f"]);
            }
            if super::TEST_FEED.is_some() {
                return false;
            }
            let Ok(exe) = std::env::current_exe() else { return false };
            let cmd = format!("\"{}\" --minimized", exe.display());
            reg(&["add", KEY, "/v", super::NAME, "/t", "REG_SZ", "/d", &cmd, "/f"])
        }

        #[cfg(test)]
        mod tests {
            /// Touches the real Run key, so opt-in: `cargo test -- --ignored`. Restores what was there.
            #[test]
            #[ignore]
            fn run_entry_is_quoted() {
                let before = super::enabled();
                assert!(super::set(true));
                let out = super::Command::new(crate::install::windows_exe(crate::install::REG)).args(["query", super::KEY, "/v", super::super::NAME]).output().unwrap();
                let text = String::from_utf8_lossy(&out.stdout).to_string();
                super::set(before);
                let exe = std::env::current_exe().unwrap();
                assert!(text.contains(&format!("\"{}\" --minimized", exe.display())), "{text}");
                assert_eq!(super::enabled(), before);
            }
        }
    }

    /// The free virtual cable Discord listens to.
    pub mod cable {
        pub const NAME: &str = "VB-Cable";
        /// The end MicClean sends to: a device name containing this.
        pub const SEND: &str = "CABLE Input";
        /// The end Discord uses as its mic.
        pub const MIC: &str = "CABLE Output";
        pub const FOUND: &str = "VB-Cable is installed";
        pub const MISSING: &str = "VB-Cable isn't installed (free)";
        pub const GET: &str = "Get VB-Cable";
        const URL: &str = "https://vb-audio.com/Cable/";
        /// "Then in Discord" in Setup.
        pub const STEPS: &[&str] = &[
            "1. Voice & Video → Input Device: CABLE Output.",
            "2. Turn off Noise Suppression, Echo Cancellation and Automatic Gain Control. MicClean does these.",
            "3. Turn off Automatic Input Sensitivity and drag its slider all the way left. MicClean mutes between words.",
            "4. In Windows Sound settings, set CABLE Input and CABLE Output to 48000 Hz (Properties → Advanced).",
            "5. Windows Settings → System → Sound → your mic: Audio enhancements off.",
        ];

        /// Either end, also when renamed in Windows: as a mic, MicClean would hear itself.
        pub fn is(name: &str) -> bool {
            name.contains("CABLE") || name.contains("VB-Audio")
        }

        /// VB-Cable's download page.
        pub fn get() {
            super::open(URL);
        }

        /// How getting it went (nothing to say on Windows: it's a web page).
        pub fn note() -> Option<String> {
            None
        }

        /// Whether it was just installed, so the device list should be read again (once).
        pub fn just_installed() -> bool {
            false
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub const SOUND_SETTINGS: &str = "Audio MIDI Setup";
    pub const SET_48K: &str = "In Audio MIDI Setup, set its Format to 48,000 Hz.";
    pub const AUTOSTART: &str = "Open at login";
    pub const TRAY: &str = "menu bar";
    pub const THIS_PC: &str = "this Mac";

    /// One copy per Mac user: a lock on a file in the app folder, held until this copy exits
    /// (the system lets go of it then, crash or not).
    pub fn first_copy() -> bool {
        static LOCK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
        if LOCK.get().is_some() {
            return true;
        }
        let dir = app_dir();
        let _ = std::fs::create_dir_all(&dir);
        let Ok(f) = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("running.lock")) else {
            return true; // can't make the file: better two copies than none
        };
        match f.try_lock() {
            Ok(()) => {
                let _ = LOCK.set(f);
                true
            }
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(_) => true,
        }
    }

    /// Dragged into Applications (not run from the disk image or a build folder).
    pub fn installed_copy() -> bool {
        let Ok(exe) = std::env::current_exe() else { return false };
        let mine = std::env::home_dir().map(|h| h.join("Applications"));
        exe.starts_with("/Applications") || mine.is_some_and(|d| exe.starts_with(d))
    }

    /// The user id, so each signed-in user gets their own window-ping port.
    pub fn session() -> u32 {
        // SAFETY: getuid has no failure case.
        unsafe { libc::getuid() }
    }

    /// `open` returns at once, but its exit still has to be collected, so off the GUI thread.
    fn run(mut cmd: Command) {
        std::thread::spawn(move || cmd.status());
    }

    /// A web page or folder, in the browser or Finder.
    pub fn open(target: impl AsRef<OsStr>) {
        let mut cmd = Command::new("/usr/bin/open");
        cmd.arg(target);
        run(cmd);
    }

    /// A file, selected in its Finder folder.
    pub fn reveal(path: &Path) {
        let mut cmd = Command::new("/usr/bin/open");
        cmd.arg("-R").arg(path);
        run(cmd);
    }

    /// Audio MIDI Setup, where a device's rate (Format) is set.
    pub fn sound_settings() {
        let mut cmd = Command::new("/usr/bin/open");
        cmd.args(["-b", "com.apple.audio.AudioMIDISetup"]);
        run(cmd);
    }

    /// "Open at login" = a LaunchAgent that starts this copy hidden when you log in.
    pub mod autostart {
        use std::path::PathBuf;

        const LABEL: &str = "io.github.toocoolcello.micclean";

        fn plist() -> PathBuf {
            std::env::home_dir().unwrap_or_default().join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))
        }

        pub fn enabled() -> bool {
            plist().exists()
        }

        pub fn set(on: bool) -> bool {
            let path = plist();
            if !on {
                return std::fs::remove_file(&path).is_ok() || !path.exists();
            }
            // A copy on the disk image (or one macOS runs from a hidden temporary place) is gone
            // by the next login.
            if !super::installed_copy() || crate::os::TEST_FEED.is_some() {
                return false;
            }
            let Ok(exe) = std::env::current_exe() else { return false };
            let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>--minimized</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
</dict>
</plist>
"#
            );
            path.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&path, xml).is_ok()
        }
    }

    /// MicClean's own device (mac-driver/, inside the app at Contents/PlugIns): one device,
    /// "MicClean", whose output comes back as its input. BlackHole still counts as a cable.
    pub mod cable {
        use std::path::Path;
        use std::process::Command;
        use std::sync::Mutex;

        pub const NAME: &str = "the MicClean device";
        /// The device MicClean sends to: a name containing this.
        pub const SEND: &str = "MicClean";
        /// What Discord uses as its mic: the same device.
        pub const MIC: &str = "MicClean";
        pub const FOUND: &str = "The MicClean device is installed";
        pub const MISSING: &str = "The MicClean device isn't installed (it asks for your password once)";
        pub const GET: &str = "Install it";
        /// "Then in Discord" in Setup. MicClean sets the device to 48000 Hz itself.
        pub const STEPS: &[&str] = &[
            "1. Voice & Video → Input Device: MicClean. Not in the list? Quit Discord and open it again.",
            "2. Turn off Noise Suppression, Echo Cancellation and Automatic Gain Control. MicClean does these.",
            "3. Turn off Automatic Input Sensitivity and drag its slider all the way left. MicClean mutes between words.",
        ];

        /// As a mic, MicClean would hear itself.
        pub fn is(name: &str) -> bool {
            name == "MicClean" || name.contains("BlackHole")
        }

        /// What the last install said, and whether the device list should be read again.
        static STATE: Mutex<(Option<String>, bool)> = Mutex::new((None, false));

        /// Installs the device in the background (macOS shows its own password prompt).
        pub fn get() {
            *STATE.lock().unwrap() = (Some("Installing…".into()), false);
            std::thread::spawn(|| {
                let r = install();
                crate::logfile::line(&format!("Installing the MicClean device: {}", r.as_ref().map_or_else(|e| e.as_str(), |_| "done")));
                if r.is_ok() {
                    *STATE.lock().unwrap() = (Some("Installed. Core Audio is restarting…".into()), false);
                    // Core Audio takes a moment to come back with the new device.
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                *STATE.lock().unwrap() = match r {
                    Ok(()) => (Some("Installed.".into()), true),
                    Err(e) => (Some(format!("Couldn't install it: {e}")), false),
                };
            });
        }

        pub fn note() -> Option<String> {
            STATE.lock().unwrap().0.clone()
        }

        pub fn just_installed() -> bool {
            std::mem::take(&mut STATE.lock().unwrap().1)
        }

        /// Copies the device out of this app into /Library/Audio/Plug-Ins/HAL as root (one
        /// password prompt) and restarts Core Audio, which loads it. Every app's sound stops
        /// for a moment.
        fn install() -> Result<(), String> {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let src = exe
                .parent()
                .and_then(Path::parent)
                .map(|contents| contents.join("PlugIns").join("MicClean.driver"))
                .filter(|p| p.is_dir())
                .ok_or("this copy of MicClean has no device in it (it's built by build-mac.sh)")?;
            let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
            let dir = "/Library/Audio/Plug-Ins/HAL";
            let dst = format!("{dir}/MicClean.driver");
            // Quarantine and other attributes from the download are cleared, or Core Audio may
            // refuse the copy. kickstart is refused under SIP on newer macOS, then killall
            // (launchd starts Core Audio again by itself).
            let script = format!(
                "/bin/mkdir -p {dir} && /bin/rm -rf {dst} && /usr/bin/ditto {} {dst} && /usr/sbin/chown -R root:wheel {dst} && /usr/bin/xattr -cr {dst} && \
                 (/bin/launchctl kickstart -k system/com.apple.audio.coreaudiod || /usr/bin/killall coreaudiod)",
                quote(&src.to_string_lossy())
            );
            // The script goes in as an argument, so no AppleScript quoting is needed.
            let prompt = r#"do shell script (item 1 of argv) with prompt "MicClean wants to add its audio device." with administrator privileges"#;
            let out = Command::new("/usr/bin/osascript")
                .args(["-e", "on run argv", "-e", prompt, "-e", "end run", &script])
                .output()
                .map_err(|e| format!("couldn't run osascript: {e}"))?;
            if out.status.success() {
                return Ok(());
            }
            let err = String::from_utf8_lossy(&out.stderr);
            Err(if err.contains("-128") { "the password prompt was cancelled".into() } else { err.trim().lines().last().unwrap_or("osascript failed").to_string() })
        }
    }
}

pub use imp::*;
