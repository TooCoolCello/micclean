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

/// Where the updater downloads setup to, and where setup leaves word of a failed update.
pub fn update_dir() -> PathBuf {
    std::env::temp_dir().join("MicClean-update")
}

/// Why an update didn't install, for the copy setup starts again.
pub fn failure_note() -> PathBuf {
    update_dir().join("failed.txt")
}

/// Setup writes this first thing, so the updating copy only closes once setup really runs.
pub fn setup_started() -> PathBuf {
    update_dir().join("started")
}

/// Closes MicClean before it's replaced. `wait_pid` is an updating copy, which closes itself;
/// without it (setup run by hand, or by an updater from before 0.7.0) any copy running from
/// the install folder is closed. Done before anything else, so even a setup that fails at once
/// starts MicClean again only after the old copy is gone.
pub fn close_running(wait_pid: Option<u32>) {
    let Ok(exe) = install_dir().map(|d| d.join(EXE)) else { return };
    match wait_pid {
        Some(pid) => wait_for_exit(pid, &exe),
        None => stop_running(&exe, 0),
    }
}

/// Same file, whatever way the paths are spelled (case, 8.3 names, junctions).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

/// The copy an update replaced. It can't be deleted while it's still closing, so the next
/// start (or the next update) clears it.
pub fn old_exe() -> Result<PathBuf, String> {
    Ok(install_dir()?.join("micclean.old.exe"))
}

/// Waits up to 10 s for the copy that started this update to close itself (it saves your
/// settings on the way out), then closes it the hard way. Only ever a process running our exe:
/// if `pid` already exited, Windows may have given the number to something else.
fn wait_for_exit(pid: u32, exe: &Path) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };
    // SAFETY: the handle is checked for null and closed before returning; the name buffer
    // outlives the call, which writes at most `len` UTF-16 units into it.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return; // already gone
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ours = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0
            && same_file(Path::new(&String::from_utf16_lossy(&buf[..len as usize])), exe);
        if ours && WaitForSingleObject(h, 10_000) == WAIT_TIMEOUT {
            TerminateProcess(h, 1);
            WaitForSingleObject(h, 5_000);
        }
        CloseHandle(h);
    }
}

/// Copies the exe in, adds the Start menu shortcut and the Settings -> Apps entry. Safe to run
/// over an existing install (it's the upgrade path too); `close_running` goes first.
///
/// The new exe goes in beside the old one and is swapped in by renaming, and is undone if it
/// fails: a full disk or a locked file leaves the working copy in place. The shortcut and the
/// Settings -> Apps entry are extras: if those fail, the returned notes say so, and MicClean is
/// still installed.
pub fn install(exe_bytes: &[u8]) -> Result<(PathBuf, Vec<String>), String> {
    let dir = install_dir()?;
    let exe = swap(&dir, exe_bytes)?;
    let mut notes = Vec::new();
    let made = start_link().is_ok_and(|link| {
        powershell(&format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({}); $s.TargetPath = {}; $s.WorkingDirectory = {}; \
             $s.IconLocation = {}; $s.Description = 'MicClean mic cleaner'; $s.Save()",
            q(&link),
            q(&exe),
            q(&dir),
            q(format!("{},0", exe.display()))
        ))
    });
    if !made {
        notes.push("couldn't create the Start menu shortcut".to_string());
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
    if !values.iter().all(|(name, kind, data)| reg(&["add", UNINSTALL_KEY, "/v", name, "/t", kind, "/d", data, "/f"])) {
        notes.push("couldn't add MicClean to Settings -> Apps".to_string());
    }
    // "Start with Windows" was on for an older copy: point it at this one.
    if reg(&["query", RUN_KEY, "/v", "MicClean"]) {
        reg(&["add", RUN_KEY, "/v", "MicClean", "/t", "REG_SZ", "/d", &format!("\"{exe_s}\" --minimized"), "/f"]);
    }
    Ok((exe, notes))
}

/// Puts `exe_bytes` in place as `dir`\micclean.exe, undoing it on failure (see `install`).
/// Renaming works even if the old copy still runs.
fn swap(dir: &Path, exe_bytes: &[u8]) -> Result<PathBuf, String> {
    let exe = dir.join(EXE);
    let (new, old) = (dir.join("micclean.new.exe"), dir.join("micclean.old.exe"));
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    std::fs::write(&new, exe_bytes).map_err(|e| format!("couldn't write {}: {e}", new.display()))?;
    let _ = std::fs::remove_file(&old);
    if exe.exists()
        && let Err(e) = std::fs::rename(&exe, &old)
    {
        let _ = std::fs::remove_file(&new);
        return Err(format!("couldn't move the old version aside: {e}"));
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("couldn't put the new version in place: {e}"));
    }
    let _ = std::fs::remove_file(&old);
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
    // Only our own exes go (and an update's leftovers); the folder only if nothing else is in it.
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Remove-Item -LiteralPath {}, {}, {} -Force -ErrorAction SilentlyContinue; \
         Remove-Item -LiteralPath {} -Recurse -Force -ErrorAction SilentlyContinue; \
         if (-not (Get-ChildItem -LiteralPath {} -Force)) {{ Remove-Item -LiteralPath {} -ErrorAction SilentlyContinue }}; {settings}",
        std::process::id(),
        q(&exe),
        q(dir.join("micclean.old.exe")),
        q(dir.join("micclean.new.exe")),
        q(update_dir()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("micclean-swap-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The updating copy hangs instead of closing: setup waits, closes it, and swaps. The path
    /// is spelled differently from how Windows reports it, as an 8.3 name or junction would be.
    /// Takes the 10 s wait: `cargo test -- --ignored swap_over_a_copy_that_wont_close`.
    #[test]
    #[ignore]
    fn swap_over_a_copy_that_wont_close() {
        let dir = temp("running");
        let exe = dir.join(EXE);
        // A stand-in that runs for a minute: Windows' own ping.
        fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
        let mut child = hidden(Command::new(&exe).args(["-n", "60", "127.0.0.1"])).spawn().unwrap();
        let other_spelling = dir.join(".").join(EXE.to_uppercase());
        wait_for_exit(child.id(), &other_spelling);
        assert!(child.try_wait().unwrap().is_some(), "the old copy still runs");
        let got = swap(&dir, b"new version").unwrap();
        assert_eq!(fs::read(&got).unwrap(), b"new version");
        assert!(!dir.join("micclean.new.exe").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_that_cant_move_the_old_copy_leaves_it_working() {
        let dir = temp("locked");
        let exe = dir.join(EXE);
        fs::write(&exe, b"working").unwrap();
        // Opened without sharing, so it can't be renamed (what antivirus sometimes does).
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&exe).unwrap();
        let err = swap(&dir, b"new version").unwrap_err();
        drop(lock);
        assert!(err.contains("aside"), "{err}");
        assert_eq!(fs::read(&exe).unwrap(), b"working");
        assert!(!dir.join("micclean.new.exe").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
