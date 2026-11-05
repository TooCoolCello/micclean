//! Copies of the settings file in the app folder's backups, made at each start when it changed
//! (so also the first start after an update, before the new version touches it), newest 10
//! kept. A settings file that won't load (cut short by a crash mid-save) is swapped for the
//! newest good copy before eframe reads it, instead of everything going back to defaults.
//! eframe writes app.ron in place, so a power cut mid-save leaves it empty or zero-filled;
//! each save also goes to settings.ron the safe way (a new file swapped in), and that copy is
//! the one a broken app.ron comes back from, so nothing since the last start is lost.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const KEEP: usize = 10;

fn backup_dir(settings_dir: &Path) -> PathBuf {
    settings_dir.parent().unwrap_or(settings_dir).join("backups")
}

/// eframe's file is a RON map of strings; a cut-off or empty one doesn't parse.
fn loads(text: &str) -> bool {
    ron::from_str::<HashMap<String, String>>(text).is_ok()
}

/// Backups, oldest first (the names sort by date).
fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ron")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Runs before eframe loads the settings.
pub fn run(settings_dir: &Path) {
    let file = settings_dir.join("app.ron");
    let dir = backup_dir(settings_dir);
    let Ok(bytes) = fs::read(&file) else { return }; // first start
    // Garbage that isn't even text (a power cut) is as broken as a cut-off file.
    let text = String::from_utf8(bytes).unwrap_or_default();
    let saved = backups(&dir);
    if !loads(&text) {
        let _ = fs::rename(&file, file.with_extension("broken.ron"));
        let read = |p: &Path| fs::read_to_string(p).ok().filter(|t| loads(t));
        let last = settings_dir.join("settings.ron");
        let good = read(&last).map(|t| (last.clone(), t)).or_else(|| saved.iter().rev().find_map(|p| read(p).map(|t| (p.clone(), t))));
        match good {
            Some((p, t)) if write_safely(&file, &t).is_ok() => {
                crate::logfile::line(&format!("The settings file was damaged; loaded {}", p.display()));
            }
            _ => crate::logfile::line("The settings file was damaged and there's no backup to load; starting fresh"),
        }
        return;
    }
    if saved.last().and_then(|p| fs::read_to_string(p).ok()).as_deref() == Some(text.as_str()) {
        return;
    }
    let _ = write(&dir, &text);
}

/// A new backup of `text` (a whole settings file), keeping the newest KEEP.
fn write(dir: &Path, text: &str) -> Result<(), String> {
    let t = crate::logfile::local_time();
    let name = format!("settings-{:04}-{:02}-{:02}-{:02}{:02}{:02}.ron", t.year, t.month, t.day, t.hour, t.minute, t.second);
    fs::create_dir_all(dir).and_then(|()| fs::write(dir.join(name), text)).map_err(|e| e.to_string())?;
    for old in backups(dir).iter().rev().skip(KEEP) {
        let _ = fs::remove_file(old);
    }
    Ok(())
}

/// %APPDATA%\MicClean\backups (Application Support/MicClean/backups on a Mac).
pub fn dir() -> PathBuf {
    backup_dir(&crate::os::settings_dir())
}

/// Backups, newest first.
pub fn list() -> Vec<PathBuf> {
    backups(&dir()).into_iter().rev().collect()
}

/// These settings (eframe's "app" value) as a whole settings file.
fn as_file(settings: &str) -> Result<String, String> {
    ron::to_string(&HashMap::from([(eframe::APP_KEY, settings)])).map_err(|e| e.to_string())
}

/// Backs up these settings now.
pub fn save(settings: &str) -> Result<(), String> {
    write(&dir(), &as_file(settings)?)
}

/// The safe copy of each save, for when app.ron is broken.
pub fn keep(settings: &str) -> Result<(), String> {
    keep_in(&crate::os::settings_dir(), settings)
}

fn keep_in(settings_dir: &Path, settings: &str) -> Result<(), String> {
    fs::create_dir_all(settings_dir).map_err(|e| e.to_string())?;
    write_safely(&settings_dir.join("settings.ron"), &as_file(settings)?).map_err(|e| e.to_string())
}

/// A crash or power cut leaves the old file or the new one, never part of one: the text goes
/// to a new file, onto the disk, and only then takes the old one's place.
fn write_safely(file: &Path, text: &str) -> std::io::Result<()> {
    let tmp = file.with_extension("ron.tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, file)
}

/// The MicClean settings in a settings file, a backup or one a friend sent, if it is one.
pub fn settings_in(text: &str) -> Option<String> {
    ron::from_str::<HashMap<String, String>>(text).ok()?.remove(eframe::APP_KEY)
}

/// "2026-10-29 19:28" for settings-2026-10-29-192843.ron.
pub fn label(p: &Path) -> String {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match stem.strip_prefix("settings-") {
        Some(s) if s.len() >= 15 => format!("{} {}:{}", &s[..10], &s[11..13], &s[13..15]),
        _ => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_file_is_swapped_for_the_newest_good_backup_and_good_ones_are_kept() {
        let root = std::env::temp_dir().join(format!("micclean-backup-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let good = |n: u32| format!("{{\"app\":\"settings {n}\"}}");

        for n in 0..12 {
            fs::write(data.join("app.ron"), good(n)).unwrap();
            run(&data);
            run(&data); // unchanged: no second copy
            // Distinct names without waiting a second each time.
            let dir = backup_dir(&data);
            if let Some(last) = backups(&dir).last() {
                fs::rename(last, dir.join(format!("settings-0000-{n:02}.ron"))).unwrap();
            }
        }
        let kept = backups(&backup_dir(&data));
        assert_eq!(kept.len(), KEEP);
        assert_eq!(fs::read_to_string(kept.last().unwrap()).unwrap(), good(11));

        fs::write(data.join("app.ron"), "{\"app\":\"settings 12").unwrap(); // cut off mid-save
        run(&data);
        assert_eq!(fs::read_to_string(data.join("app.ron")).unwrap(), good(11));
        assert!(data.join("app.broken.ron").exists());
        assert_eq!(settings_in(&good(11)).as_deref(), Some("settings 11"));

        // With a safe copy of the last save, a zero-filled file (power cut) comes back as that
        // save, not the older backup; so does one that isn't even text.
        keep_in(&data, "settings 13").unwrap();
        for junk in [vec![0u8; 64], vec![0xff, 0xfe, 0x00, 0x9c]] {
            fs::write(data.join("app.ron"), junk).unwrap();
            run(&data);
            assert_eq!(settings_in(&fs::read_to_string(data.join("app.ron")).unwrap()).as_deref(), Some("settings 13"));
        }
        assert!(!data.join("settings.ron.tmp").exists());
        assert_eq!(settings_in("not a settings file"), None);
        assert_eq!(label(Path::new("settings-2026-10-29-192843.ron")), "2026-10-29 19:28");
        let _ = fs::remove_dir_all(&root);
    }
}
