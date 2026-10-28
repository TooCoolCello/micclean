#![allow(dead_code)] // micclean.exe uses uninstall, the setup exe uses install
//! Per-user install and uninstall, no admin needed: the exe in %LOCALAPPDATA%\Programs\MicClean,
//! a Start menu shortcut, and an entry under Settings -> Apps. Shared by the setup exe and by
//! `micclean.exe --uninstall`.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MessageBoxW};

pub const EXE: &str = "micclean.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\MicClean";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn env_path(var: &str) -> Result<PathBuf, String> {
    std::env::var_os(var).map(PathBuf::from).ok_or_else(|| format!("%{var}% is not set"))
}

pub fn install_dir() -> Result<PathBuf, String> {
    Ok(env_path("LOCALAPPDATA")?.join("Programs").join("MicClean"))
}

/// True when this is the copy the installer put in place (the only one that updates itself).
pub fn is_installed_copy() -> bool {
    let installed = install_dir().map(|d| d.join(EXE));
    std::env::current_exe().ok() == installed.ok()
}

fn start_link() -> Result<PathBuf, String> {
    Ok(env_path("APPDATA")?.join(r"Microsoft\Windows\Start Menu\Programs\MicClean.lnk"))
}

pub fn hidden(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
}

fn reg(args: &[&str]) -> bool {
    hidden(Command::new("reg").args(args)).status().is_ok_and(|s| s.success())
}

fn powershell_args(script: &str) -> [&str; 6] {
    ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]
}

fn powershell(script: &str) -> bool {
    hidden(Command::new("powershell").args(powershell_args(script))).status().is_ok_and(|s| s.success())
}

/// A PowerShell single-quoted string literal.
fn q(s: impl AsRef<std::ffi::OsStr>) -> String {
    format!("'{}'", s.as_ref().to_string_lossy().replace('\'', "''"))
}

/// Closes a running copy of this exe so the file can be replaced or deleted.
fn stop_running(exe: &Path, keep_pid: u32) {
    powershell(&format!(
        "Get-Process micclean -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq {} -and $_.Id -ne {keep_pid} }} \
         | Stop-Process -Force; Start-Sleep -Milliseconds 600",
        q(exe)
    ));
}

pub fn info(title: &str, text: &str) {
    message(title, text, false);
}

pub fn ask(title: &str, text: &str) -> bool {
    message(title, text, true)
}

fn message(title: &str, text: &str, question: bool) -> bool {
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let style = if question { MB_YESNO | MB_ICONQUESTION } else { MB_OK | MB_ICONINFORMATION };
    // SAFETY: both strings are NUL-terminated and outlive the call; no owner window.
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), style) == IDYES }
}

/// Copies the exe in, adds the Start menu shortcut and the Settings -> Apps entry. Safe to run
/// over an existing install (it's the upgrade path too).
pub fn install(exe_bytes: &[u8]) -> Result<PathBuf, String> {
    let dir = install_dir()?;
    let exe = dir.join(EXE);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    stop_running(&exe, 0);
    std::fs::write(&exe, exe_bytes).map_err(|e| format!("couldn't write {}: {e}", exe.display()))?;

    let link = start_link()?;
    let made = powershell(&format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath = {}; $s.WorkingDirectory = {}; \
         $s.IconLocation = {}; $s.Description = 'MicClean mic cleaner'; $s.Save()",
        q(&link),
        q(&exe),
        q(&dir),
        q(format!("{},0", exe.display()))
    ));
    if !made {
        return Err("couldn't create the Start menu shortcut".into());
    }

    let exe_s = exe.to_string_lossy().to_string();
    let values = [
        ("DisplayName", "REG_SZ", "MicClean".to_string()),
        ("DisplayVersion", "REG_SZ", env!("CARGO_PKG_VERSION").to_string()),
        ("InstallLocation", "REG_SZ", dir.to_string_lossy().to_string()),
        ("DisplayIcon", "REG_SZ", format!("{exe_s},0")),
        ("UninstallString", "REG_SZ", format!("\"{exe_s}\" --uninstall")),
        ("NoModify", "REG_DWORD", "1".to_string()),
        ("NoRepair", "REG_DWORD", "1".to_string()),
        ("EstimatedSize", "REG_DWORD", (exe_bytes.len() / 1024).to_string()),
    ];
    for (name, kind, data) in &values {
        if !reg(&["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", data, "/f"]) {
            return Err("couldn't register with Settings -> Apps".into());
        }
    }
    // "Start with Windows" was on for an older copy: point it at this one.
    if reg(&["query", RUN_KEY, "/v", "MicClean"]) {
        reg(&["add", RUN_KEY, "/v", "MicClean", "/t", "REG_SZ", "/d", &format!("\"{exe_s}\" --minimized"), "/f"]);
    }
    Ok(exe)
}

/// Runs from the installed exe (Settings -> Apps -> Uninstall). A running exe can't delete
/// itself, so a hidden PowerShell waits for this process to exit and then removes it.
pub fn uninstall(silent: bool) {
    const TITLE: &str = "Uninstall MicClean";
    let Ok(dir) = install_dir() else { return };
    let exe = dir.join(EXE);
    // Never delete anything unless this really is the installed copy (not a dev build folder).
    if std::env::current_exe().ok().as_deref() != Some(exe.as_path()) {
        if !silent {
            info(TITLE, "This copy of MicClean isn't the installed one, so there is nothing to uninstall here.");
        }
        return;
    }
    if !silent && !ask(TITLE, "Remove MicClean from this PC?") {
        return;
    }
    let wipe = !silent
        && ask(TITLE, "Also delete your settings, learned sounds and EQ?\n\nChoose No to keep them in case you reinstall.");
    stop_running(&exe, std::process::id());

    if let Ok(link) = start_link() {
        let _ = std::fs::remove_file(link);
    }
    reg(&["delete", UNINSTALL_KEY, "/f"]);
    reg(&["delete", RUN_KEY, "/v", "MicClean", "/f"]);

    let settings = match (wipe, env_path("APPDATA")) {
        (true, Ok(a)) => format!("Remove-Item -LiteralPath {} -Recurse -Force -ErrorAction SilentlyContinue;", q(a.join("MicClean"))),
        _ => String::new(),
    };
    // Only our own exe goes; the folder only if nothing else is in it.
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue; \
         if (-not (Get-ChildItem -LiteralPath {} -Force)) {{ Remove-Item -LiteralPath {} -ErrorAction SilentlyContinue }}; {settings}",
        std::process::id(),
        q(&exe),
        q(&dir),
        q(&dir)
    );
    let _ = Command::new("powershell")
        .args(powershell_args(&script))
        // Not DETACHED_PROCESS: PowerShell exits at once without a console. It outlives us anyway.
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if !silent {
        info(TITLE, "MicClean was removed.");
    }
}
