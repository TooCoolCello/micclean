//! micclean.log in the app folder (%APPDATA%\MicClean, or Application Support on a Mac):
//! device errors, glitches, restarts and updates, for troubleshooting. Written from the GUI
//! thread only, never from the audio callbacks.

use std::io::Write;
use std::path::PathBuf;

/// Past this the log moves to micclean.old.log and starts over, so it never grows past 2 MB.
const MAX_BYTES: u64 = 1 << 20;

pub fn path() -> PathBuf {
    crate::os::app_dir().join("micclean.log")
}

pub struct LocalTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

#[cfg(windows)]
pub fn local_time() -> LocalTime {
    // SAFETY: SYSTEMTIME is plain integers, and GetLocalTime fills all of it.
    let mut t = unsafe { std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    LocalTime { year: t.wYear, month: t.wMonth, day: t.wDay, hour: t.wHour, minute: t.wMinute, second: t.wSecond }
}

#[cfg(not(windows))]
pub fn local_time() -> LocalTime {
    // SAFETY: `tm` is plain integers that localtime_r fills; `now` outlives the call.
    let t = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut t = std::mem::zeroed::<libc::tm>();
        libc::localtime_r(&now, &mut t);
        t
    };
    let v = |x: i32| x as u16;
    LocalTime { year: v(t.tm_year + 1900), month: v(t.tm_mon + 1), day: v(t.tm_mday), hour: v(t.tm_hour), minute: v(t.tm_min), second: v(t.tm_sec) }
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
        let _ = writeln!(f, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {msg}", t.year, t.month, t.day, t.hour, t.minute, t.second);
    }
}
