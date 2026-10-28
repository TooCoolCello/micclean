#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MicClean-Setup.exe. build-installer.ps1 appends micclean.exe, its length and a marker to this
//! program; running it copies that exe in and adds the Start menu entry. `--silent` skips every
//! prompt and doesn't start the app.

mod install;

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
    let silent = std::env::args().any(|a| a == "--silent");
    let launch = std::env::args().any(|a| a == "--launch"); // the updater: start the new copy afterwards
    if !silent && !install::ask(TITLE, "Install MicClean for your Windows account?\n\nIt adds a Start menu entry and needs no admin rights.") {
        return;
    }
    match payload().and_then(|exe| install::install(&exe)) {
        Ok(exe) if !silent => {
            if install::ask(TITLE, "MicClean is installed.\n\nStart it now?") {
                let _ = std::process::Command::new(exe).spawn();
            }
        }
        Ok(exe) => {
            if launch {
                let _ = std::process::Command::new(exe).spawn();
            }
        }
        Err(e) => {
            if !silent {
                install::info(TITLE, &format!("Setup failed: {e}"));
            }
            std::process::exit(1);
        }
    }
}
