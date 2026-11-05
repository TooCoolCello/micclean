//! VST3 and CLAP effect plugins in the rack.
//!
//! Searching runs each plugin file in a child copy of MicClean (`--scan-plugin`), so one that
//! crashes or hangs costs that file, not the app. Each plugin in the rack runs in a child copy
//! of its own too (`--plugin-host`, see the end of win.rs): a plugin that crashes or hangs
//! ends only that process, and MicClean carries on without it. The audio thread gets each
//! plugin's `Proc` through the mailbox; the GUI talks to its process through `Host`.
//!
//! The host is Windows only so far. On a Mac the rack has MicClean's own effects, and a plugin
//! in a settings file from Windows stays off.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;
#[cfg(not(windows))]
pub use none::*;

/// Whether this build can search for and run plugins.
pub const SUPPORTED: bool = cfg!(windows);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum Format {
    Vst3,
    Clap,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Vst3 => "VST3",
            Format::Clap => "CLAP",
        }
    }
}

/// One effect in a plugin file, as a search found it.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Info {
    pub format: Format,
    /// The DLL itself (inside a .vst3 bundle, the one for this machine).
    pub path: PathBuf,
    /// VST3: the class ID as 32 hex digits. CLAP: the plugin ID.
    pub uid: String,
    pub name: String,
    pub vendor: String,
}

/// A plugin in the rack, or in a preset's: which one, and its settings as it last saved them.
/// `Fx::Plugin { key }` points here.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub key: u32,
    pub info: Info,
    /// The plugin's own settings, hex.
    pub state: String,
    /// It crashed or hung while it ran: it stays unloaded until "Try again". (Before plugins
    /// had their own processes: it crashed MicClean.)
    pub crashed: bool,
}

/// Each loaded plugin's audio half, by key, for the audio thread.
pub type Procs = Vec<(u32, Arc<Mutex<Proc>>)>;

/// A search in progress: files done of all, then the effects found.
#[derive(Default)]
pub struct Search {
    pub progress: (usize, usize),
    pub found: Option<Vec<Info>>,
}

/// No plugin host: nothing is found and nothing loads.
#[cfg(not(windows))]
mod none {
    use super::*;

    pub struct Proc;

    impl Proc {
        pub fn process(&mut self, _: &mut [f32]) {}
    }

    pub struct Instance {
        pub editor_error: Option<String>,
    }

    impl Instance {
        pub fn editor_open(&self) -> bool {
            false
        }
        pub fn open_editor(&mut self, _: Option<isize>, _: bool) {}
        pub fn close_editor(&mut self) {}
    }

    #[derive(Default)]
    pub struct Host {
        pub resend: bool,
    }

    impl Host {
        pub fn sync(&mut self, _: &[u32], _: &mut [Entry]) -> Option<Procs> {
            None
        }
        pub fn get(&self, _: u32) -> Option<&Result<Instance, String>> {
            None
        }
        pub fn get_mut(&mut self, _: u32) -> Option<&mut Result<Instance, String>> {
            None
        }
        pub fn retry(&mut self, _: u32) {}
        pub fn idle(&mut self) -> Vec<(u32, String)> {
            Vec::new()
        }
        pub fn store(&mut self, _: &mut [Entry]) {}
        pub fn shutdown(&mut self) {}
    }

    pub fn default_dirs() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn search(_: Vec<PathBuf>) -> Arc<Mutex<Search>> {
        Arc::new(Mutex::new(Search { progress: (0, 0), found: Some(Vec::new()) }))
    }

    pub fn pick_folder(_: Option<isize>) -> Option<PathBuf> {
        None
    }

    pub fn scan_main(_: &[String]) {}

    pub fn host_main(_: &[String]) {}
}
