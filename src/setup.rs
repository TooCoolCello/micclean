#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MicClean-Setup.exe. build-installer.ps1 appends micclean.exe, its length and a marker to this
//! program; running it copies that exe in and adds the Start menu entry. `--silent` skips every
//! prompt and doesn't start the app.
//!
//! The updater runs it as `--silent --launch [--wait <pid>] [--minimized]`: wait for that copy
//! to close, install, then start whatever MicClean is in place, the new one or (if installing
//! failed) the old one, so an update never leaves you without your mic. Every step goes to
//! micclean.log, since nobody sees a silent setup.

mod install;
mod logfile;

use std::io::{Read, Seek, SeekFrom};

const MAGIC: &[u8; 8] = b"MCPAYLD1";
const TITLE: &str = "MicClean Setup";

/// The appended micclean.exe: [exe bytes][length: u64 LE][MAGIC].
fn payload() -> Result<Vec<u8>, String> {
    let es = |e: std::io::Error| e.to_string();
    let mut f = std::fs::File::open(std::env::current_exe().map_err(es)?).map_err(es)?;
    let total = f.metadata().map_err(es)?.len();
    let mut tail = [0u8; 16];
    f.seek(SeekFrom::End(-16)).map_err(es)?;
    f.read_exact(&mut tail).map_err(es)?;
    let len = u64::from_le_bytes(tail[..8].try_into().unwrap());
    if &tail[8..] != MAGIC || len > total - 16 {
        return Err("this setup file has no MicClean inside it. Build it with build-installer.ps1".into());
    }
    f.seek(SeekFrom::Start(total - 16 - len)).map_err(es)?;
    let mut exe = vec![0u8; len as usize];
    f.read_exact(&mut exe).map_err(es)?;
    Ok(exe)
}

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
    let result = std::panic::catch_unwind(|| payload().and_then(|exe| install::install(&exe)))
        .unwrap_or_else(|_| Err("setup crashed".into()));
    match &result {
        Ok((_, notes)) => {
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
        if let Ok(exe) = install::install_dir().map(|d| d.join(install::EXE)) {
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
            Ok((exe, notes)) => {
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
