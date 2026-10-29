//! %APPDATA%\MicClean\micclean.log: device errors, glitches, restarts and updates, for
//! troubleshooting. Written from the GUI thread only, never from the audio callbacks.

use std::io::Write;
use std::path::PathBuf;

/// Past this the log moves to micclean.old.log and starts over, so it never grows past 2 MB.
const MAX_BYTES: u64 = 1 << 20;

pub fn path() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("MicClean").join("micclean.log")
}

pub fn local_time() -> windows_sys::Win32::Foundation::SYSTEMTIME {
    // SAFETY: SYSTEMTIME is plain integers, and GetLocalTime fills all of it.
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    t
}

/// Appends one line with the local time. Failures are ignored: the log must never stop the app.
pub fn line(msg: &str) {
    if cfg!(test) {
        return; // tests mustn't write into your real log
    }
    let path = path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let t = local_time();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {msg}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    }
}
