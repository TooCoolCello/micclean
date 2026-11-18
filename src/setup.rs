#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MicClean-Setup.exe from before Velopack. build-installer-old.ps1 appends micclean.exe, its
//! length and a marker to this program; running it copies that exe in and adds the Start menu
//! entry. `--silent` skips every prompt and doesn't start the app.
//!
//! The bridge to Velopack (MicClean-Setup.exe in release 0.12.0 only, made by
//! build-installer.ps1) carries Velopack's setup instead, under its own marker. The updater in
//! 0.11 and older can only run a setup like this one, so this one waits for that copy to close
//! and runs Velopack's setup, which installs to %LOCALAPPDATA%\MicClean, then starts that copy.
//! It then clears the old install away (install::remove_legacy). If Velopack's setup fails, the
//! old install is still there and starts again.
//!
//! The updater runs it as `--silent --launch [--wait <pid>] [--minimized]`: wait for that copy
//! to close, install, then start whatever MicClean is in place, the new one or (if installing
//! failed) the old one, so an update never leaves you without your mic. Every step goes to
//! micclean.log, since nobody sees a silent setup.
//!
//! Windows only: a Mac gets MicClean.app in a .dmg instead (build-mac.sh), so there this is
//! an empty program.

#[cfg(windows)]
mod install;
#[cfg(windows)]
mod logfile;
#[cfg(windows)]
#[allow(dead_code, unused_imports)] // setup only needs the app folder, for the log
mod os;

#[cfg(windows)]
use std::io::{Read, Seek, SeekFrom};

#[cfg(windows)]
const MAGIC: &[u8; 8] = b"MCPAYLD1";
/// The bridge: the payload is Velopack's MicClean-win-Setup.exe.
#[cfg(windows)]
const MAGIC_VELOPACK: &[u8; 8] = b"MCVELOP1";
#[cfg(windows)]
const TITLE: &str = "MicClean Setup";

#[cfg(not(windows))]
fn main() {}

/// The appended micclean.exe: [exe bytes][length: u64 LE][MAGIC], or Velopack's setup with
/// MAGIC_VELOPACK (then true).
#[cfg(windows)]
fn payload() -> Result<(Vec<u8>, bool), String> {
    let es = |e: std::io::Error| e.to_string();
    let mut f = std::fs::File::open(std::env::current_exe().map_err(es)?).map_err(es)?;
    let total = f.metadata().map_err(es)?.len();
    let mut tail = [0u8; 16];
    f.seek(SeekFrom::End(-16)).map_err(es)?;
    f.read_exact(&mut tail).map_err(es)?;
    let len = u64::from_le_bytes(tail[..8].try_into().unwrap());
    let velopack = &tail[8..] == MAGIC_VELOPACK;
    if !(velopack || &tail[8..] == MAGIC) || len > total - 16 {
        return Err("this setup file has no MicClean inside it. Build it with build-installer.ps1".into());
    }
    f.seek(SeekFrom::Start(total - 16 - len)).map_err(es)?;
    let mut exe = vec![0u8; len as usize];
    f.read_exact(&mut exe).map_err(es)?;
    Ok((exe, velopack))
}

/// Runs Velopack's setup and waits for it. It installs MicClean to %LOCALAPPDATA%\MicClean
/// (install::velopack_exe), and starts it unless `--silent`.
#[cfg(windows)]
fn run_velopack(setup: &[u8], silent: bool) -> Result<(), String> {
    let dir = install::update_dir();
    let path = dir.join("MicClean-win-Setup.exe");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, setup).map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
    let mut cmd = std::process::Command::new(&path);
    if silent {
        cmd.arg("--silent");
    }
    let status = cmd.status().map_err(|e| format!("couldn't start Velopack's setup: {e}"))?;
    let _ = std::fs::remove_file(&path);
    if status.success() { Ok(()) } else { Err(format!("Velopack's setup failed ({status})")) }
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let (silent, launch, minimized) = (flag("--silent"), flag("--launch"), flag("--minimized"));
    let wait_pid = args.iter().position(|a| a == "--wait").and_then(|i| args.get(i + 1)?.parse().ok());
    if !silent && !install::ask(TITLE, "Install MicClean for your Windows account?\n\nIt adds a Start menu entry and needs no admin rights.") {
        return;
    }
    let version = env!("CARGO_PKG_VERSION");
    if launch {
        // The updater closes itself only once this exists.
        let _ = std::fs::write(install::setup_started(), version);
    }
    install::close_running(wait_pid);
    // Some(..) for the old install, None once Velopack's setup has installed it.
    let result = std::panic::catch_unwind(|| {
        payload().and_then(|(bytes, velopack)| if velopack { run_velopack(&bytes, silent).map(|()| None) } else { install::install(&bytes).map(Some) })
    })
    .unwrap_or_else(|_| Err("setup crashed".into()));
    match &result {
        Ok(None) => logfile::line(&format!("Setup moved MicClean over to Velopack ({version})")),
        Ok(Some((_, notes))) => {
            logfile::line(&format!("Setup installed MicClean {version}"));
            notes.iter().for_each(|n| logfile::line(&format!("Setup: {n}")));
        }
        Err(e) => {
            logfile::line(&format!("Setup couldn't install MicClean {version}: {e}"));
            if launch {
                let _ = std::fs::write(install::failure_note(), e);
            }
        }
    }

    if launch {
        // Velopack's copy, or the old install if that didn't work. A silent Velopack setup
        // doesn't start it itself.
        let exe = match result {
            Ok(None) => install::velopack_exe(),
            _ => install::install_dir().map(|d| d.join(install::EXE)),
        };
        if let Ok(exe) = exe {
            let mut cmd = std::process::Command::new(exe);
            // Waits for the old copy's single-instance lock instead of taking it as "running".
            cmd.arg("--updated");
            if minimized {
                cmd.arg("--minimized");
            }
            if let Err(e) = cmd.spawn() {
                logfile::line(&format!("Setup couldn't start MicClean again: {e}"));
            }
        }
    } else if !silent {
        match &result {
            Ok(None) => {}
            Ok(Some((exe, notes))) => {
                let extra = if notes.is_empty() { String::new() } else { format!("\n\nBut it {}.", notes.join(", and ")) };
                if install::ask(TITLE, &format!("MicClean is installed.{extra}\n\nStart it now?")) {
                    let _ = std::process::Command::new(exe).spawn();
                }
            }
            Err(e) => install::info(TITLE, &format!("Setup failed: {e}")),
        }
    }
    if result.is_err() {
        std::process::exit(1);
    }
}
