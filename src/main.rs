#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appicon;
mod audio;
mod dsp;
mod graphs;
mod install;
mod logfile;
mod smart;
mod ui;
mod update;

use audio::{Devices, Engine, Shared};
use dsp::{Band, BandKind, Kind, Meters, NoiseProfile, Params, Profiles, SPEC_BINS, SPEC_LO, SPEC_SPAN};
use eframe::egui::{self, Align, Color32, Layout, Pos2, Rect, RichText, Stroke, ViewportCommand};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use ui::icon;

// ponytail: a fixed localhost port is the single-instance lock; a named mutex if it ever collides.
const INSTANCE_PORT: u16 = 47391;
const HELLO: &[u8] = b"MICCLEAN\n";
const RECORD_SECS: usize = 10;
/// Set when a second launch or the tray asks for the window.
static SHOW: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

fn main() -> eframe::Result {
    // Settings -> Apps -> Uninstall runs this, before the single-instance check below.
    if std::env::args().any(|a| a == "--uninstall") {
        install::uninstall(std::env::args().any(|a| a == "--silent"));
        return Ok(());
    }
    match TcpListener::bind((Ipv4Addr::LOCALHOST, INSTANCE_PORT)) {
        Ok(listener) => {
            std::thread::spawn(move || {
                for mut c in listener.incoming().flatten() {
                    let _ = c.write_all(HELLO);
                    SHOW.store(true, Ordering::Relaxed);
                    if let Some(ctx) = CTX.get() {
                        ctx.request_repaint();
                    }
                }
            });
        }
        Err(_) => {
            // Already running: that copy shows its window, this one leaves. Two copies would
            // both grab the mic and double the audio going to Discord.
            let mut hello = [0u8; HELLO.len()];
            let is_us = TcpStream::connect((Ipv4Addr::LOCALHOST, INSTANCE_PORT))
                .and_then(|mut s| s.read_exact(&mut hello))
                .is_ok_and(|_| hello == HELLO);
            if is_us {
                return Ok(());
            }
        }
    }

    let hidden = std::env::args().any(|a| a == "--minimized");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([420.0, 420.0])
            .with_title("MicClean")
            .with_icon(Arc::new(egui::IconData { rgba: icon_rgba(true), width: ICON, height: ICON }))
            .with_visible(!hidden),
        ..Default::default()
    };
    eframe::run_native(
        "MicClean",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            // eframe shows the window after its first paint whatever with_visible says,
            // so a hidden start hides it again on frame 2. ponytail: costs a one-frame flash.
            app.hide_in = if hidden { 2 } else { 0 };
            Ok(Box::new(app))
        }),
    )
}

/// Everything that survives a restart (eframe stores it in %APPDATA%\MicClean).
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    devices: Devices,
    params: Params,
    profiles: Profiles,
    /// Stored inverted so a save from before this existed (and Default) means "check".
    update_check_off: bool,
    /// Skip the "update available" popup (Settings still shows it).
    update_popup_off: bool,
    /// Install updates without asking, once the gate is closed.
    auto_update: bool,
    presets: Vec<Preset>,
    /// Starter presets were made once (from the settings at the time). Deleting them sticks.
    presets_seeded: bool,
    /// The nav rail folded down to icons.
    nav_compact: bool,
    /// What the last Smart setup chose, for each box's Suggest button.
    smart: Option<Params>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Preset {
    name: String,
    params: Params,
}

/// Same settings, ignoring the Bypass switch (that's a listening aid, not part of a preset).
fn same_preset(a: &Params, b: &Params) -> bool {
    Params { bypass: false, ..a.clone() } == Params { bypass: false, ..b.clone() }
}

/// Talking = your settings as they are; Singing and Gaming are small changes from there,
/// so your EQ and gains carry into all three.
fn starter_presets(current: &Params) -> Vec<Preset> {
    let talking = Params { bypass: false, ..current.clone() };
    let singing = Params {
        sing_protect: 0.7,
        gate_hold_ms: 900.0,
        gate_hysteresis_db: 10.0,
        comp_ratio: 2.0,
        deess_threshold_db: current.deess_threshold_db + 4.0,
        ..talking.clone()
    };
    let gaming = Params { denoise_strength: 1.0, sing_protect: 0.1, gate_hold_ms: 250.0, vad_threshold: 0.6, ..talking.clone() };
    [("Talking", talking), ("Singing", singing), ("Gaming", gaming)]
        .into_iter()
        .map(|(name, params)| Preset { name: name.into(), params })
        .collect()
}

/// One effect that the "hold to hear it off" button can bypass.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Denoise,
    Gate,
    Agc,
    Eq,
    Deess,
    Comp,
}

impl Stage {
    fn turn_off(self, p: &mut Params) {
        match self {
            Stage::Denoise => p.denoise = false,
            Stage::Gate => p.gate = false,
            Stage::Agc => p.agc = false,
            Stage::Eq => p.eq = false,
            Stage::Deess => p.deess = false,
            Stage::Comp => p.comp = false,
        }
    }
}

/// "Hold to hear it off": bypasses one stage only while the button is held down.
fn ab_row(ui: &mut egui::Ui, stage: Stage, shown: Option<Stage>, hold: &mut Option<Stage>) {
    ui::row(ui, "Compare", |ui| {
        let held = shown == Some(stage);
        let label = if held { "Off while held" } else { "Hold to hear it off" };
        if ui.add(egui::Button::new(label).selected(held)).is_pointer_button_down_on() {
            *hold = Some(stage);
        }
    });
}

/// The fixed 4-filter EQ that saves had before the parametric one.
#[derive(Deserialize)]
struct OldEq {
    low_cut_hz: f32,
    low_gain_db: f32,
    mid_freq_hz: f32,
    mid_gain_db: f32,
    high_gain_db: f32,
}

#[derive(Deserialize)]
struct OldSave {
    params: OldEq,
}

/// Loads the settings, turning an old fixed EQ into the same filters as bands. The next save
/// writes bands only, so this converts once.
// ponytail: drop OldEq once nobody has a pre-parametric save left.
fn load(storage: &dyn eframe::Storage) -> Saved {
    let mut saved: Saved = eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
    if let Some(OldSave { params: o }) = eframe::get_value(storage, eframe::APP_KEY) {
        let band = |kind, freq, q, gain_db| Some(Band { kind, freq, gain_db, q, on: true });
        let boost = |kind, freq, q, gain_db: f32| if gain_db != 0.0 { band(kind, freq, q, gain_db) } else { None };
        saved.params.bands = [
            band(BandKind::LowCut, o.low_cut_hz, 0.707, 0.0),
            boost(BandKind::LowShelf, 200.0, 0.707, o.low_gain_db),
            boost(BandKind::Bell, o.mid_freq_hz, 1.0, o.mid_gain_db),
            boost(BandKind::HighShelf, 5000.0, 0.707, o.high_gain_db),
            None,
        ];
    }
    if !saved.presets_seeded {
        saved.presets = starter_presets(&saved.params);
        saved.presets_seeded = true;
    }
    saved
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Teach,
    Test,
    Settings,
}

const NAV: [(Page, &str, &str); 3] = [
    (Page::Home, icon::HOME, "Home"),
    (Page::Teach, icon::LEARN, "Learn sounds"),
    (Page::Test, icon::RECORD, "Test recording"),
];

/// A box in the signal chain; clicking one opens its settings under the chain.
#[derive(Clone, Copy, PartialEq)]
enum Node {
    Input,
    Echo,
    Noise,
    Gate,
    Level,
    Eq,
    Deess,
    Comp,
    Limiter,
    Output,
}

impl Node {
    fn title(self) -> &'static str {
        match self {
            Node::Input => "Input",
            Node::Echo => "Echo cancellation",
            Node::Noise => "Noise removal",
            Node::Gate => "Voice gate",
            Node::Level => "Auto level",
            Node::Eq => "Equalizer",
            Node::Deess => "De-esser",
            Node::Comp => "Compressor",
            Node::Limiter => "Limiter",
            Node::Output => "Output",
        }
    }

    /// Puts this box's settings back to the defaults. Its on/off switch stays as it is.
    fn reset(self, p: &mut Params) {
        let d = Params::default();
        match self {
            Node::Input => p.input_gain_db = d.input_gain_db,
            Node::Echo => {}
            Node::Noise => (p.denoise_strength, p.sing_protect) = (d.denoise_strength, d.sing_protect),
            Node::Gate => {
                (p.gate_threshold_db, p.gate_hysteresis_db, p.vad_threshold) = (d.gate_threshold_db, d.gate_hysteresis_db, d.vad_threshold);
                (p.gate_hold_ms, p.gate_attack_ms, p.gate_release_ms) = (d.gate_hold_ms, d.gate_attack_ms, d.gate_release_ms);
                p.gate_floor_db = d.gate_floor_db;
            }
            Node::Level => (p.agc_target_db, p.agc_max_gain_db) = (d.agc_target_db, d.agc_max_gain_db),
            Node::Eq => p.bands = d.bands,
            Node::Deess => p.deess_threshold_db = d.deess_threshold_db,
            Node::Comp => {
                (p.comp_threshold_db, p.comp_ratio, p.comp_makeup_db) = (d.comp_threshold_db, d.comp_ratio, d.comp_makeup_db);
                (p.comp_attack_ms, p.comp_release_ms) = (d.comp_attack_ms, d.comp_release_ms);
            }
            Node::Limiter | Node::Output => p.output_gain_db = d.output_gain_db,
        }
    }

    /// The boxes Smart setup chooses settings for.
    const SMART: [Node; 6] = [Node::Input, Node::Noise, Node::Gate, Node::Eq, Node::Deess, Node::Comp];

    /// Copies what Smart setup chose (`s`) for this box. Other boxes are left alone.
    fn suggest(self, p: &mut Params, s: &Params) {
        match self {
            Node::Input => p.input_gain_db = s.input_gain_db,
            Node::Noise => p.denoise_strength = s.denoise_strength,
            // Chosen for the suggested input gain, so it follows the gain you actually have.
            Node::Gate => p.gate_threshold_db = (s.gate_threshold_db + p.input_gain_db - s.input_gain_db).clamp(-80.0, -10.0),
            Node::Eq => p.bands = s.bands,
            Node::Deess => p.deess_threshold_db = s.deess_threshold_db,
            Node::Comp => (p.comp_threshold_db, p.comp_ratio, p.comp_makeup_db) = (s.comp_threshold_db, s.comp_ratio, s.comp_makeup_db),
            Node::Echo | Node::Level | Node::Limiter | Node::Output => {}
        }
    }
}

/// Smart setup in progress: room noise first, then you reading, one sample per audio frame.
struct Calib {
    start: Instant,
    /// Last meters frame sampled, so a frame isn't counted twice.
    frame: u64,
    quiet: Vec<smart::Quiet>,
    talk: Vec<smart::Take>,
}

const CALIB_QUIET: f32 = 3.0;
const CALIB_END: f32 = 18.0;
/// Read aloud during Smart setup: plenty of S sounds for the de-esser, and a range of pitch.
const PASSAGE: &str = "Some days start slowly. I sip my coffee, check the schedule, and see what's waiting for us. \
    Six messages, a missed call, and a song stuck in my head since Sunday. \
    So let's get started: say hello, share the plan, and keep it simple.";

/// Every library license compiled into the exe, written by licenses.ps1.
const NOTICES: &str = include_str!("../THIRD-PARTY-NOTICES.txt");

struct App {
    saved: Saved,
    shared: Arc<Mutex<Shared>>,
    engine: Option<Engine>,
    engine_devices: Devices,
    inputs: Vec<String>,
    outputs: Vec<String>,
    status: String,
    glitches: (u32, String),
    eq_ui: graphs::EqEditor,
    learn_msg: String,
    meters: Meters,
    last_frames: u64,
    last_beat: Instant,
    next_check: Instant,
    needs_restart: bool,
    noise_name: String,
    pending_noise_name: String,
    clip: Option<Vec<f32>>,
    record_progress: Option<f32>,
    playback: Option<(cpal::Stream, Instant)>,
    tray: Option<TrayIcon>,
    tray_open: bool,
    show_id: MenuId,
    quit_id: MenuId,
    quitting: bool,
    autostart: bool,
    updater: update::Updater,
    /// Only the installed copy can replace itself, so only it checks.
    installed: bool,
    next_update_check: Instant,
    update_prompt: bool,
    /// Version the user said "Later" to; it isn't offered again until the next launch.
    update_dismissed: Option<String>,
    preset_name: String,
    /// A/B: the stage being held off this frame, and the one the audio got (and the button shows).
    ab_hold: Option<Stage>,
    ab_shown: Option<Stage>,
    /// The window drew last frame, so the display-only spectra are worth computing.
    viz: bool,
    /// Device delays in microseconds: mic capture, Discord output.
    latency_us: (u32, u32),
    agc_hist: graphs::History<f32, 300>,
    pitch_hist: graphs::History<(f32, bool), 180>,
    /// Frames until a --minimized start hides the window.
    hide_in: u8,
    page: Page,
    /// The chain box whose settings are open on Home.
    node: Option<Node>,
    calib: Option<Calib>,
    /// What the last Smart setup changed (or why it couldn't), and the settings from before it.
    smart_report: Vec<String>,
    smart_undo: Option<Params>,
    /// Windows' rate for the input, output and monitor devices, looked up on each restart.
    rates: [Option<u32>; 3],
    show_notices: bool,
    /// What the log file already has, so it gets changes only.
    logged_status: String,
    logged_glitches: u32,
    next_glitch_log: Instant,
    logged_update: Option<std::mem::Discriminant<update::State>>,
    hwnd: Option<isize>,
    title_dark: Option<bool>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let _ = CTX.set(cc.egui_ctx.clone());
        ui::install(&cc.egui_ctx);
        let hwnd = cc.window_handle().ok().and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        });
        logfile::line(&format!("MicClean {} started", update::VERSION));
        let saved = cc.storage.map(load).unwrap_or_default();
        let shared = Shared {
            params: saved.params.clone(),
            profiles: saved.profiles.clone(),
            ..Default::default()
        };

        let show = MenuItem::new("Show MicClean", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append(&show);
        let _ = menu.append(&quit);
        // No tray (e.g. no shell running) just means closing the window quits.
        let tray = tray_icon::Icon::from_rgba(icon_rgba(false), ICON, ICON).ok().and_then(|icon| {
            TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip("MicClean")
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .ok()
        });

        let now = Instant::now();
        let mut app = Self {
            saved,
            shared: Arc::new(Mutex::new(shared)),
            engine: None,
            engine_devices: Devices::default(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            status: String::new(),
            glitches: (0, String::new()),
            eq_ui: Default::default(),
            learn_msg: String::new(),
            meters: Meters::default(),
            last_frames: 0,
            last_beat: now,
            next_check: now,
            needs_restart: false,
            noise_name: String::new(),
            pending_noise_name: String::new(),
            clip: None,
            record_progress: None,
            playback: None,
            tray,
            tray_open: false,
            show_id: show.id().clone(),
            quit_id: quit.id().clone(),
            quitting: false,
            autostart: autostart::enabled(),
            updater: Default::default(),
            installed: install::is_installed_copy(),
            next_update_check: now + Duration::from_secs(3), // after the audio has started
            update_prompt: false,
            update_dismissed: None,
            preset_name: String::new(),
            ab_hold: None,
            ab_shown: None,
            viz: false,
            latency_us: (0, 0),
            agc_hist: graphs::History::new(0.0, 100),                    // 30 s
            pitch_hist: graphs::History::new((f32::NAN, false), 33), // 6 s
            hide_in: 0,
            page: Page::Home,
            node: None,
            calib: None,
            smart_report: Vec::new(),
            smart_undo: None,
            rates: [None; 3],
            show_notices: false,
            logged_status: String::new(),
            logged_glitches: 0,
            next_glitch_log: now,
            logged_update: None,
            hwnd,
            title_dark: None,
        };
        app.refresh_devices();
        let d = &mut app.saved.devices;
        if d.input.is_empty() {
            d.input = app.inputs.first().cloned().unwrap_or_default();
        }
        if d.output.is_empty() {
            // VB-Cable's playback side is what Discord then picks up as "CABLE Output".
            d.output = app.outputs.iter().find(|n| n.contains("CABLE Input")).cloned().unwrap_or_default();
        }
        app.restart();
        if app.engine.is_none() {
            app.node = Some(Node::Input); // first run or a missing mic: start where it gets fixed
        }
        app
    }

    fn refresh_devices(&mut self) {
        self.inputs = audio::device_names(true);
        self.outputs = audio::device_names(false);
    }

    fn restart(&mut self) {
        self.engine = None; // stop old streams before opening the devices again
        self.engine_devices = self.saved.devices.clone();
        self.last_beat = Instant::now();
        self.needs_restart = false;
        let d = &self.saved.devices;
        let rate = |input, name: &str| if name.is_empty() { None } else { audio::device_rate(input, name) };
        self.rates = [rate(true, &d.input), rate(false, &d.output), d.monitor.as_deref().and_then(|n| rate(false, n))];
        if self.saved.devices.input.is_empty() || self.saved.devices.output.is_empty() {
            self.status = "Click Input and Output in the signal chain to pick your devices.".into();
            return;
        }
        match audio::start(&self.saved.devices, self.shared.clone()) {
            Ok(e) => {
                self.status = format!("{} → {} · {}", self.saved.devices.input, self.saved.devices.output, e.info);
                self.engine = Some(e);
            }
            Err(e) => self.status = format!("{e} (retrying)"),
        }
    }

    /// Restarts audio when the mic stream dies, a device errors, or a missing one comes back.
    fn watchdog(&mut self) {
        let now = Instant::now();
        if self.meters.frames != self.last_frames {
            self.last_frames = self.meters.frames;
            self.last_beat = now;
        }
        if now < self.next_check || self.saved.devices.input.is_empty() || self.saved.devices.output.is_empty() {
            return;
        }
        self.next_check = now + Duration::from_secs(3);
        let stalled = self.engine.is_some() && now - self.last_beat > Duration::from_secs(2);
        let waiting_for_device = self.engine.as_ref().is_none_or(|e| !e.missing.is_empty());
        if waiting_for_device {
            self.refresh_devices();
        }
        let came_back = self
            .engine
            .as_ref()
            .is_some_and(|e| e.missing.iter().any(|m| self.outputs.contains(m)));
        if stalled {
            logfile::line("No sound from the input for 2 s, restarting audio");
        }
        if came_back {
            logfile::line("A missing device is back, restarting audio");
        }
        if self.engine.is_none() || stalled || came_back || self.needs_restart {
            self.refresh_devices();
            self.restart();
        }
    }

    fn sync(&mut self) {
        let mut sh = self.shared.lock().unwrap();
        let mut want = self.saved.params.clone();
        if let Some(s) = self.ab_shown {
            s.turn_off(&mut want);
        }
        if self.calib.is_some() {
            // Smart setup needs every stage's meter on your voice: nothing bypassed, the gate
            // open, the de-esser and compressor running.
            (want.bypass, want.gate, want.deess, want.comp) = (false, false, true, true);
        }
        if sh.params != want {
            sh.params = want;
            sh.params_ver += 1;
        }
        sh.viz = self.viz;
        self.latency_us = (sh.latency.capture_us.load(Ordering::Relaxed), sh.latency.output_us.load(Ordering::Relaxed));
        if let Some((kind, prof)) = sh.learned.take() {
            match prof {
                None => self.learn_msg = "That clip was too quiet, nothing saved.".into(),
                Some(profile) if kind == Kind::Keep => {
                    self.saved.profiles.keep = Some(profile);
                    self.learn_msg = "Learned your voice.".into();
                }
                Some(profile) => {
                    let name = std::mem::take(&mut self.pending_noise_name);
                    self.learn_msg = format!("Learned \"{name}\".");
                    let noises = &mut self.saved.profiles.noises;
                    match noises.iter_mut().find(|n| n.name == name) {
                        Some(n) => {
                            n.profile = profile;
                            n.enabled = true;
                        }
                        None => noises.push(NoiseProfile { name, enabled: true, profile }),
                    }
                }
            }
        }
        if sh.profiles != self.saved.profiles {
            sh.profiles = self.saved.profiles.clone();
            sh.profiles_ver += 1;
        }
        if let Some(clip) = sh.recorded.take() {
            self.clip = Some(clip);
        }
        self.record_progress = sh.record_progress;
        if let Some(e) = sh.error.take() {
            self.status = format!("Audio error: {e} (reconnecting)");
            self.needs_restart = true;
        }
        if sh.glitches != self.glitches.0 {
            self.glitches = (sh.glitches, sh.glitch_dev.clone());
        }
        self.meters = sh.meters.clone();
    }

    fn play(&mut self, processed: bool) {
        self.stop_playback();
        let Some(raw) = &self.clip else { return };
        let clip = if processed {
            dsp::render(raw, &self.saved.params, &self.saved.profiles)
        } else {
            raw.clone()
        };
        let end = Instant::now() + Duration::from_secs_f32(clip.len() as f32 / dsp::SR);
        let monitor = self.saved.devices.monitor.clone();
        match audio::play(monitor.as_deref(), clip) {
            Ok(s) => {
                self.shared.lock().unwrap().mute_monitor = true;
                self.playback = Some((s, end));
            }
            Err(e) => self.status = format!("Playback failed: {e}"),
        }
    }

    fn stop_playback(&mut self) {
        self.playback = None;
        self.shared.lock().unwrap().mute_monitor = false;
    }

    fn learn(&mut self, kind: Kind, secs: f32) {
        self.learn_msg.clear();
        self.shared.lock().unwrap().learn = Some((kind, secs));
    }

    /// Status changes, glitches (at most a line a minute) and update steps go to the log file.
    fn log_changes(&mut self, update: &update::State, now: Instant) {
        if self.status != self.logged_status {
            logfile::line(&self.status);
            self.logged_status.clone_from(&self.status);
        }
        if self.glitches.0 != self.logged_glitches && now >= self.next_glitch_log {
            logfile::line(&format!("{} glitches so far, last on {}", self.glitches.0, self.glitches.1));
            (self.logged_glitches, self.next_glitch_log) = (self.glitches.0, now + Duration::from_secs(60));
        }
        let kind = std::mem::discriminant(update);
        if self.logged_update != Some(kind) {
            self.logged_update = Some(kind);
            match update {
                update::State::Available(r) => logfile::line(&format!("Update {} available", r.version)),
                update::State::Installing => logfile::line("Installing the update"),
                update::State::Failed(e) => logfile::line(&format!("Update failed: {e}")),
                _ => {}
            }
        }
    }

    /// Samples the meters once per audio frame while Smart setup runs, then applies what it
    /// chose and keeps the old settings for Undo.
    fn smart_step(&mut self) {
        let Some(c) = &mut self.calib else { return };
        let t = c.start.elapsed().as_secs_f32();
        if t >= CALIB_END {
            let c = self.calib.take().expect("checked above");
            match smart::analyse(&self.saved.params, &c.quiet, &c.talk) {
                Ok(o) => {
                    logfile::line(&format!("Smart setup {}", o.measured));
                    let old = self.saved.params.clone();
                    for n in Node::SMART {
                        n.suggest(&mut self.saved.params, &o.params);
                    }
                    self.smart_report = smart::changes(&old, &self.saved.params);
                    if self.smart_report.is_empty() {
                        self.smart_report.push("Your settings already suit your voice. Nothing changed.".into());
                    }
                    self.smart_report.extend(o.notes);
                    // Suggest offers the full EQ, even where setup kept your own bands.
                    self.saved.smart = Some(Params { bands: o.full_eq, ..o.params });
                    self.smart_undo = Some(old);
                }
                Err(e) => self.smart_report = vec![e],
            }
            logfile::line(&format!("Smart setup: {}", self.smart_report.join(" / ")));
            return;
        }
        let m = &self.meters;
        if m.frames == c.frame {
            return;
        }
        c.frame = m.frames;
        if t < CALIB_QUIET {
            if t > 0.5 {
                // Past the click of the button.
                c.quiet.push(smart::Quiet { level: m.gate_level_db, peak: m.in_db, bands: m.bands_db });
            }
        } else if m.vad >= 0.5 {
            // Talking, not the pauses between words.
            let (level, peak, agc, deess, comp) = (m.gate_level_db, m.in_db, m.agc_db, m.deess_level_db, m.comp_in_db);
            c.talk.push(smart::Take { level, peak, agc, deess, comp, pitch: m.pitch_hz, bands: m.bands_db });
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync();
        self.viz = false; // set again by ui() if the window still draws
        self.watchdog();
        let now = Instant::now();
        self.agc_hist.push(self.meters.agc_db, now);
        self.pitch_hist.push((self.meters.pitch_hz.unwrap_or(f32::NAN), self.meters.gate_open), now);
        self.smart_step();
        if self.installed && !self.saved.update_check_off && now >= self.next_update_check {
            self.next_update_check = now + Duration::from_secs(24 * 3600);
            self.updater.check();
        }
        let st = self.updater.state();
        self.log_changes(&st, now);
        if let update::State::Available(r) = st
            && self.update_dismissed.as_deref() != Some(&r.version)
        {
            if self.saved.auto_update {
                // Never in the middle of a sentence: wait until the gate has closed.
                if self.installed && !self.meters.gate_open {
                    self.updater.install(r);
                }
            } else if !self.saved.update_popup_off {
                self.update_prompt = true;
            }
        }
        if self.saved.devices != self.engine_devices {
            self.restart();
        }

        let dark = ctx.theme() == egui::Theme::Dark;
        if self.title_dark != Some(dark) {
            if let Some(h) = self.hwnd {
                ui::title_bar(h, dark);
            }
            self.title_dark = Some(dark);
        }

        if self.hide_in > 0 {
            self.hide_in -= 1;
            if self.hide_in == 0 {
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }
        let mut show = SHOW.swap(false, Ordering::Relaxed);
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            show |= matches!(
                e,
                TrayIconEvent::DoubleClick { .. }
                    | TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            );
        }
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            if e.id == self.show_id {
                show = true;
            } else if e.id == self.quit_id {
                self.quitting = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
        if show {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }

        // Tray dot: green while the gate lets you through to Discord.
        if let Some(tray) = &self.tray
            && self.meters.gate_open != self.tray_open
        {
            self.tray_open = self.meters.gate_open;
            if let Ok(icon) = tray_icon::Icon::from_rgba(icon_rgba(self.tray_open), ICON, ICON) {
                let _ = tray.set_icon(Some(icon));
            }
        }
        if self.playback.as_ref().is_some_and(|(_, end)| Instant::now() >= *end) {
            self.stop_playback();
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && self.tray.is_some() && !self.quitting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }

        self.viz = true;
        // Whatever was held down last frame is what the audio hears now; buttons re-arm it.
        self.ab_shown = self.ab_hold.take();
        if self.update_prompt {
            self.update_popup(&ctx);
        }
        if self.show_notices {
            self.notices(&ctx);
        }
        let p = ui::pal(ui);
        // Like Windows' NavigationView: labels when there's room, icons only when narrow or
        // folded with the menu button.
        let compact = self.saved.nav_compact || ui.available_width() < 720.0;
        egui::Panel::left("nav")
            .exact_size(if compact { 56.0 } else { 240.0 })
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 4, right: 4, top: 10, bottom: 8 }))
            .show(ui, |ui| self.nav(ui, compact));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 24, right: 24, top: 16, bottom: 0 }))
            .show(ui, |ui| {
                let title = match self.page {
                    Page::Settings => "Settings",
                    page => NAV.iter().find(|n| n.0 == page).map_or("", |n| n.2),
                };
                ui.heading(title);
                ui.add_space(10.0);
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    // A cap, not a size: set_max_width(1000) alone widened the column past
                    // narrower windows and pushed every card's controls off the right edge.
                    ui.set_max_width(ui.available_width().min(1000.0));
                    ui.spacing_mut().item_spacing.y = 4.0;
                    match self.page {
                        Page::Home => self.home(ui),
                        Page::Teach => self.teach(ui),
                        Page::Test => self.test(ui),
                        Page::Settings => self.settings(ui),
                    }
                    ui.add_space(24.0);
                });
            });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.saved);
    }
}

/// Section title between card groups, like the Settings app.
fn section(ui: &mut egui::Ui, text: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(text).font(ui::semibold(14.0)));
    ui.add_space(2.0);
}

/// "CABLE Input runs at 44100 Hz", with what that costs and how to fix it.
fn rate_note(ui: &mut egui::Ui, name: &str, rate: u32, desc: &str) {
    ui::setting(ui, icon::WARNING, &format!("{name} runs at {rate} Hz"), desc, |_| {});
}

/// "In  -23 dB  ━━━━━━──": a label, the level and a -60..0 dB meter.
fn level_row(ui: &mut egui::Ui, label: &str, db: f32) {
    let p = ui::pal(ui);
    ui.horizontal(|ui| {
        ui.add_sized([28.0, 18.0], egui::Label::new(RichText::new(label).small().color(p.text2)));
        ui.add_sized([60.0, 18.0], egui::Label::new(RichText::new(format!("{db:.0} dB")).small()));
        ui::level(ui, db);
    });
}

fn slider(ui: &mut egui::Ui, v: &mut f32, r: std::ops::RangeInclusive<f32>, unit: &str) -> egui::Response {
    ui.add(egui::Slider::new(v, r).suffix(unit))
}

/// How close the current sound is to a learned one. The marker is the match point set by
/// "Match looseness"; past it the bar turns accent and the sound counts as that one.
fn match_bar(ui: &mut egui::Ui, dist: f32, looseness: f32) {
    let p = ui::pal(ui);
    let fill = (0.6 * looseness / dist).clamp(0.0, 1.0); // dist == looseness sits on the marker
    let color = if dist < looseness { p.accent } else { p.text2 };
    ui.add_sized([56.0, 18.0], egui::Label::new(RichText::new(if dist < looseness { "match" } else { "" }).small().color(p.accent)));
    graphs::meter(ui, 160.0, fill, 0.0, 1.0, Some(0.6), color);
}

impl App {
    fn nav(&mut self, ui: &mut egui::Ui, compact: bool) {
        let fold = if compact { "Show labels" } else { "MicClean" };
        if ui::nav_item(ui, icon::MENU, fold, false, compact).clicked() {
            self.saved.nav_compact = !compact;
        }
        ui.add_space(4.0);
        for (page, ic, label) in NAV {
            if ui::nav_item(ui, ic, label, self.page == page, compact).clicked() {
                self.page = page;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if ui::nav_item(ui, icon::SETTINGS, "Settings", self.page == Page::Settings, compact).clicked() {
                self.page = Page::Settings;
            }
        });
    }

    fn live_meters(&self) -> Meters {
        if self.engine.is_some() {
            self.meters.clone()
        } else {
            let quiet = -120.0;
            Meters { in_db: quiet, out_db: quiet, comp_in_db: quiet, gate_level_db: quiet, reason: "no audio", ..Default::default() }
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        let mut restart = false;
        let title = if self.engine.is_some() { "Running" } else { "Not running" };
        let status = match &self.glitches {
            (0, _) => self.status.clone(),
            (n, dev) => format!("{} · {n} glitch{} (last on {dev})", self.status, if *n == 1 { "" } else { "es" }),
        };
        ui::setting(ui, icon::MIC, title, &status, |ui| {
            restart = ui::icon_button(ui, true, icon::REFRESH, "Restart").clicked();
        });
        if restart {
            self.restart();
        }

        let m = self.live_meters();
        self.chain_strip(ui, &m);
        match self.node {
            Some(n) => {
                ui.add_space(10.0);
                let (mut close, mut reset, mut suggest) = (false, false, false);
                let (params, smart) = (&self.saved.params, self.saved.smart.as_ref().filter(|_| Node::SMART.contains(&n)));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(n.title()).font(ui::semibold(20.0)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = ui::icon_button(ui, true, icon::CLOSE, "Close").clicked();
                        if n != Node::Echo {
                            reset = ui::icon_button(ui, true, icon::REFRESH, "Reset")
                                .on_hover_text("Back to the defaults for this box. On/off stays as it is")
                                .clicked();
                        }
                        if let Some(s) = smart {
                            suggest = ui::icon_button(ui, true, icon::SMART, "Suggest")
                                .on_hover_ui(|ui| {
                                    let mut q = params.clone();
                                    n.suggest(&mut q, s);
                                    let lines = smart::changes(params, &q);
                                    ui.label(match lines.is_empty() {
                                        true => "Already set the way your Smart setup chose".to_string(),
                                        false => format!("From your Smart setup:\n{}", lines.join("\n")),
                                    });
                                })
                                .clicked();
                        }
                    });
                });
                ui.add_space(4.0);
                self.panel(ui, n, &m);
                if reset {
                    n.reset(&mut self.saved.params);
                }
                if suggest && let Some(s) = self.saved.smart.clone() {
                    n.suggest(&mut self.saved.params, &s);
                }
                if close {
                    self.node = None;
                }
            }
            None => {
                self.presets_card(ui);
                self.overview(ui, &m);
            }
        }
    }

    /// Every stage in the order Discord hears them: on/off, a light that glows with how hard it
    /// is working, the live amount, and the delay end to end. Click a box for its settings.
    fn chain_strip(&mut self, ui: &mut egui::Ui, m: &Meters) {
        let p = ui::pal(ui);
        let (cap, out) = (self.latency_us.0 as f32 / 1000.0, self.latency_us.1 as f32 / 1000.0);
        let pr = &mut self.saved.params;
        let off = pr.bypass;
        let rnn = if pr.denoise && !off { 10.0 } else { 0.0 }; // RNNoise's one frame
        let running = self.engine.is_some();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Signal chain").font(ui::semibold(14.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if running {
                    let text = format!("≈ {:.0} ms delay  ·  DSP {:.1}%", cap + rnn + out, m.dsp_load * 100.0);
                    ui.label(RichText::new(text).small().color(p.text2)).on_hover_text(format!(
                        "Input buffer {cap:.0} ms · noise removal {rnn:.0} ms · output buffer {out:.0} ms.\nThe app you send to adds its own delay on top.\n\nDSP: time spent cleaning each 10 ms of sound, as a share of one CPU core."
                    ));
                }
            });
        });
        ui.add_space(2.0);

        let d = &self.saved.devices;
        let echo_on = d.echo_ref.is_some();
        let bands = pr.bands.iter().flatten().filter(|b| b.on).count();
        // Input and Output carry sound whenever audio runs, Bypass or not.
        let active = [running, echo_on, pr.denoise, pr.gate, pr.agc, pr.eq, pr.deess, pr.comp, true, running]
            .into_iter()
            .enumerate()
            .map(|(i, a)| a && running && (!off || i == 0 || i == 9));
        let db = |v: f32| format!("-{v:.1} dB");
        let fill = |v: f32| (v + 60.0) / 60.0;
        let echo = if echo_on { m.echo_erle_db.map_or("listening".into(), |e| format!("-{e:.0} dB")) } else { "off".into() };
        let device = |name: &str, db: f32, none: &str| match (name.is_empty(), running) {
            (true, _) => none.to_string(),
            (_, false) => "-".into(),
            _ => format!("{db:.0} dB"),
        };
        let input = device(&d.input, m.in_db, "pick input");
        let output = match self.rates[1] {
            Some(r) if r != dsp::SR as u32 && !d.output.is_empty() => "set 48 kHz".into(),
            _ => device(&d.output, m.out_db, "pick output"),
        };
        let stages: [(Node, &str, Option<&mut bool>, f32, String); 10] = [
            (Node::Input, "Input", None, fill(m.in_db), input),
            (Node::Echo, "Echo", None, m.echo_erle_db.unwrap_or(0.0) / 30.0, echo),
            (Node::Noise, "Noise", Some(&mut pr.denoise), m.denoise_db / 20.0, db(m.denoise_db)),
            (Node::Gate, "Gate", Some(&mut pr.gate), if m.gate_open { 1.0 } else { 0.0 }, if m.gate_open { "open" } else { "muted" }.into()),
            (Node::Level, "Auto level", Some(&mut pr.agc), m.agc_db.abs() / 12.0, format!("{:+.1} dB", m.agc_db)),
            (Node::Eq, "EQ", Some(&mut pr.eq), 0.5, format!("{bands} band{}", if bands == 1 { "" } else { "s" })),
            (Node::Deess, "De-esser", Some(&mut pr.deess), m.deess_db / 8.0, db(m.deess_db)),
            (Node::Comp, "Compressor", Some(&mut pr.comp), m.gr_db / 12.0, db(m.gr_db)),
            (Node::Limiter, "Limiter", None, m.lim_db / 6.0, if m.lim_db > 0.1 { db(m.lim_db) } else { "idle".into() }),
            (Node::Output, "Output", None, fill(m.out_db), output),
        ];
        // As many 100 px boxes per row as fit, then even rows (5 + 5, not 8 + 2) filling the width.
        let avail = ui.available_width();
        let rows = stages.len().div_ceil((((avail + 6.0) / 106.0) as usize).clamp(1, stages.len()));
        let per = stages.len().div_ceil(rows);
        let w = (avail - 6.0 * (per - 1) as f32) / per as f32 - 0.5;
        let mut go = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for ((node, name, on, level, value), active) in stages.into_iter().zip(active) {
                let value = match (active, off, running) {
                    (true, ..) => value,
                    _ if matches!(node, Node::Input | Node::Output) => value, // "pick input" or "-"
                    (_, true, _) => "bypassed".into(),
                    (_, _, false) => "-".into(),
                    _ if node == Node::Echo => value,
                    _ => "off".into(),
                };
                if graphs::stage_box(ui, w, name, on, self.node == Some(node), active, level, &value).clicked() {
                    go = Some(node);
                }
            }
        });
        if let Some(node) = go {
            self.node = if self.node == Some(node) { None } else { Some(node) };
        }
    }

    /// Save and recall whole settings. Picking one keeps Bypass as it is.
    fn presets_card(&mut self, ui: &mut egui::Ui) {
        let current = self.saved.presets.iter().position(|pr| same_preset(&pr.params, &self.saved.params));
        let desc = match current {
            Some(i) => format!("Using \"{}\"", self.saved.presets[i].name),
            None => "Changed since you picked one. Type a name and Save to keep these settings".into(),
        };
        let (mut pick, mut save, mut delete) = (None, false, false);
        let (presets, name) = (&self.saved.presets, &mut self.preset_name);
        ui::setting(ui, icon::SETTINGS, "Preset", &desc, |ui| {
            let bin = egui::Button::new(RichText::new(icon::DELETE).font(ui::icon_font(14.0))).frame(false);
            delete = ui.add_enabled(current.is_some(), bin).on_hover_text("Delete this preset").clicked();
            save = ui
                .add_enabled(!name.trim().is_empty(), egui::Button::new("Save"))
                .on_hover_text("Saves the current settings under this name (replaces one with the same name)")
                .clicked();
            ui.add(egui::TextEdit::singleline(name).hint_text("New preset name").desired_width(130.0));
            let shown = current.map_or("Choose", |i| presets[i].name.as_str());
            egui::ComboBox::from_id_salt("preset").width(130.0).selected_text(shown).truncate().show_ui(ui, |ui| {
                for (i, pr) in presets.iter().enumerate() {
                    if ui.add(egui::Button::selectable(current == Some(i), &pr.name)).clicked() {
                        pick = Some(i);
                    }
                }
            });
        });
        if let Some(i) = pick {
            let bypass = self.saved.params.bypass;
            self.saved.params = Params { bypass, ..self.saved.presets[i].params.clone() };
        }
        if save {
            let name = self.preset_name.trim().to_string();
            let params = Params { bypass: false, ..self.saved.params.clone() };
            match self.saved.presets.iter_mut().find(|p| p.name == name) {
                Some(p) => p.params = params,
                None => self.saved.presets.push(Preset { name, params }),
            }
            self.preset_name.clear();
        }
        if let (true, Some(i)) = (delete, current) {
            self.saved.presets.remove(i);
        }
    }

    /// Room noise, then you reading a passage; every effect is set from that. While it runs
    /// the passage and a progress bar show here; afterwards what changed, with Undo.
    fn smart_card(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let can_start = self.engine.is_some() && !self.saved.params.bypass;
        let (calib, report, has_undo) = (&mut self.calib, &self.smart_report, self.smart_undo.is_some());
        let (mut start, mut undo) = (false, false);
        ui::card(ui, |ui| {
            let desc = "Listens to your room for 3 seconds and to you reading for 15, then sets each effect for your voice";
            ui::header(ui, icon::SMART, "Smart setup", desc, |ui| {
                if calib.is_some() {
                    if ui.button("Cancel").clicked() {
                        *calib = None;
                    }
                } else {
                    start = ui::accent_button(ui, can_start, "Start").on_disabled_hover_text("Needs audio running and Bypass off").clicked();
                }
            });
            if let Some(c) = calib {
                let t = c.start.elapsed().as_secs_f32();
                ui.add_space(6.0);
                if t < CALIB_QUIET {
                    ui.label(RichText::new("Stay quiet for a moment...").font(ui::semibold(16.0)));
                } else {
                    ui.label(RichText::new("Now read this out loud, like you would in a call:").color(p.text2));
                    ui.label(RichText::new(PASSAGE).size(17.0));
                }
                ui.add_space(4.0);
                ui.add(egui::ProgressBar::new((t / CALIB_END).min(1.0)).desired_height(6.0));
            } else if !report.is_empty() {
                ui.add_space(4.0);
                for line in report {
                    ui.label(RichText::new(line).color(p.text2));
                }
                if has_undo {
                    undo = ui.button("Undo").on_hover_text("Puts every setting back to how it was before").clicked();
                }
            }
        });
        if start {
            self.calib = Some(Calib { start: Instant::now(), frame: 0, quiet: Vec::new(), talk: Vec::new() });
            self.smart_report.clear();
        }
        if undo && let Some(old) = self.smart_undo.take() {
            self.saved.params = Params { bypass: self.saved.params.bypass, ..old };
            self.smart_report = vec!["Undone: your settings are back to how they were.".into()];
        }
    }

    /// Nothing selected in the chain: Smart setup, levels, spectrum and Bypass.
    fn overview(&mut self, ui: &mut egui::Ui, m: &Meters) {
        let p = ui::pal(ui);
        self.smart_card(ui);
        ui::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Levels").font(ui::semibold(14.0)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let reason = match m.noise_match.and_then(|i| self.saved.profiles.noises.get(i)) {
                        Some(n) if m.reason == "learned noise" => format!("noise: {}", n.name),
                        _ => m.reason.to_string(),
                    };
                    if m.gate_open {
                        ui::pill(ui, &format!("Open · {reason}"), p.good, p.on_accent);
                    } else {
                        ui::pill(ui, &format!("Muted · {reason}"), p.control_stroke, p.text2);
                    }
                    let pitch = m.pitch_hz.map_or("-".into(), |f| format!("{f:.0} Hz"));
                    let detail = RichText::new(format!("voice {:.0}%  ·  pitch {pitch}", m.vad * 100.0)).small().color(p.text2);
                    ui.add(egui::Label::new(detail).truncate());
                });
            });
            level_row(ui, "In", m.in_db);
            level_row(ui, "Out", m.out_db);
        });
        spectrum_card(ui, m, &self.saved.params);
        ui::setting(ui, icon::POWER, "Bypass", "Send the raw mic to compare (echo cancellation stays on)", |ui| {
            ui::toggle(ui, &mut self.saved.params.bypass);
        });
        ui.label(RichText::new("Click a box in the signal chain to open its settings here.").small().color(p.text2));
    }

    /// The settings of one chain box, shown right under the chain: its graph or meters, the
    /// hold-to-compare button and every setting it has.
    fn panel(&mut self, ui: &mut egui::Ui, node: Node, m: &Meters) {
        let p = ui::pal(ui);
        let snap = self.saved.params.clone(); // heap-free copy for the graphs to read
        let (shown, hold) = (self.ab_shown, &mut self.ab_hold);
        let (agc_hist, pitch_hist, eq_ui) = (&self.agc_hist, &self.pitch_hist, &mut self.eq_ui);
        let (d, inputs, outputs) = (&mut self.saved.devices, &self.inputs, &self.outputs);
        let rates = self.rates;
        let off_rate = |i: usize| rates[i].filter(|&r| r != dsp::SR as u32);
        let pr = &mut self.saved.params;
        let mut refresh = false;
        let refresh_button = |ui: &mut egui::Ui| ui.button("Refresh list").on_hover_text("Plugged something in?").clicked();
        match node {
            Node::Input => {
                ui::setting(ui, icon::MIC, "Input device", "The mic or line input to clean up", |ui| {
                    refresh |= refresh_button(ui);
                    pick(ui, "mic", &mut d.input, inputs);
                });
                if let Some(r) = off_rate(0) {
                    let desc = "MicClean converts it to 48000 Hz. Setting the device to 48000 Hz in Windows skips that step";
                    rate_note(ui, &d.input, r, desc);
                }
                ui::group(ui, icon::MIC, "Input gain", "Boost or cut the input before anything else", None, |ui| {
                    level_row(ui, "In", m.in_db);
                    ui::row(ui, "Gain", |ui| {
                        slider(ui, &mut pr.input_gain_db, -20.0..=30.0, " dB");
                    });
                });
            }
            Node::Echo => {
                ui::setting(ui, icon::SPEAKERS, "Cancel echo from", "What you listen on: speakers, or open-back headphones your mic can hear", |ui| {
                    refresh |= refresh_button(ui);
                    pick_opt(ui, "echo", &mut d.echo_ref, outputs, "Off");
                });
                match &d.echo_ref {
                    None => {
                        let desc = "Off. Turn it on if your mic hears your speakers or open-back headphones (friends hear themselves echo). Closed-back headphones don't need it";
                        ui::setting(ui, icon::HEADPHONE, "Echo cancellation", desc, |_| {});
                    }
                    Some(spk) => ui::card(ui, |ui| {
                        ui::header(ui, icon::SPEAKERS, "Echo cancellation", &format!("Removing what {spk} plays from your mic"), |_| {});
                        let level = format!("{:.0} dB", m.echo_ref_db);
                        graphs::meter_row(ui, "Playback level", m.echo_ref_db, -60.0, 0.0, None, p.accent, &level);
                        let erle = m.echo_erle_db.map_or("learning...".into(), |e| format!("{e:.0} dB"));
                        graphs::meter_row(ui, "Echo removed", m.echo_erle_db.unwrap_or(0.0), 0.0, 40.0, None, p.good, &erle);
                        ui::row(ui, "Speaker delay it found", |ui| {
                            ui.label(m.echo_delay_ms.map_or("-".into(), |d| format!("{d} ms")));
                        });
                    }),
                }
            }
            Node::Noise => ui::group(ui, icon::VOLUME, "Noise removal", "Removes fans, keyboards and room noise", Some(&mut pr.denoise), |ui| {
                graphs::denoise_view(ui, m);
                ui.add_space(6.0);
                ab_row(ui, Stage::Denoise, shown, hold);
                ui::row(ui, "Strength", |ui| {
                    ui.add(egui::Slider::new(&mut pr.denoise_strength, 0.0..=1.0));
                });
                ui::row(ui, "Back off while singing", |ui| {
                    ui.add(egui::Slider::new(&mut pr.sing_protect, 0.0..=1.0))
                        .on_hover_text("Raise this if held notes sound watery or thin.");
                });
            }),
            Node::Gate => {
                ui::group(ui, icon::MUTE, "Voice gate", "Mutes you when you're not talking or singing", Some(&mut pr.gate), |ui| {
                    if let Some(t) = graphs::gate_graph(ui, &snap, m) {
                        pr.gate_threshold_db = t.clamp(-80.0, -10.0);
                    }
                    ui.add_space(6.0);
                    ab_row(ui, Stage::Gate, shown, hold);
                    ui::row(ui, "Minimum level", |ui| {
                        slider(ui, &mut pr.gate_threshold_db, -80.0..=-10.0, " dB");
                    });
                    ui::row(ui, "Close below it by", |ui| {
                        slider(ui, &mut pr.gate_hysteresis_db, 0.0..=20.0, " dB")
                            .on_hover_text("Keeps fading notes open. 0 = opens and closes at the same level.");
                    });
                    ui::row(ui, "Voice confidence needed", |ui| {
                        ui.add(egui::Slider::new(&mut pr.vad_threshold, 0.05..=0.95));
                    });
                    ui::row(ui, "Hold open after you stop", |ui| {
                        slider(ui, &mut pr.gate_hold_ms, 0.0..=2000.0, " ms");
                    });
                    ui::row(ui, "Fade in", |ui| {
                        slider(ui, &mut pr.gate_attack_ms, 0.5..=50.0, " ms").on_hover_text("How fast it opens. Too fast can click.");
                    });
                    ui::row(ui, "Fade out", |ui| {
                        slider(ui, &mut pr.gate_release_ms, 10.0..=500.0, " ms").on_hover_text("How fast it closes once muting.");
                    });
                    ui::row(ui, "Volume while muted", |ui| {
                        slider(ui, &mut pr.gate_floor_db, -90.0..=0.0, " dB");
                    });
                });
                ui::card(ui, |ui| {
                    let desc = "A held note that started as voice keeps the gate open, even when the voice detector loses it";
                    ui::header(ui, icon::MUSIC, "Singing detector", desc, |_| {});
                    graphs::pitch_view(ui, pitch_hist, m, "last 6 s");
                });
            }
            Node::Level => {
                let desc = format!("Evens out quiet and loud · now {:+.1} dB", m.agc_db);
                ui::group(ui, icon::VOLUME, "Auto level", &desc, Some(&mut pr.agc), |ui| {
                    graphs::history_graph(ui, agc_hist, -12.0, snap.agc_max_gain_db.max(1.0), Some(0.0), p.accent, "gain it applied, last 30 s");
                    ui.add_space(6.0);
                    ab_row(ui, Stage::Agc, shown, hold);
                    ui::row(ui, "Target level", |ui| {
                        slider(ui, &mut pr.agc_target_db, -36.0..=-10.0, " dB");
                    });
                    ui::row(ui, "Max boost", |ui| {
                        slider(ui, &mut pr.agc_max_gain_db, 0.0..=30.0, " dB");
                    });
                });
            }
            Node::Eq => ui::group(ui, icon::EQ, "Equalizer", "Up to 5 bands over your live output spectrum", Some(&mut pr.eq), |ui| {
                eq_ui.show(ui, &mut pr.bands, &m.spec_out);
                ab_row(ui, Stage::Eq, shown, hold);
            }),
            Node::Deess => {
                let desc = format!("Tames harsh S sounds · now -{:.1} dB", m.deess_db);
                ui::group(ui, icon::MUSIC, "De-esser", &desc, Some(&mut pr.deess), |ui| {
                    let (lvl, thr) = (m.deess_level_db, snap.deess_threshold_db);
                    let hot = if lvl > thr { graphs::AMBER } else { p.accent };
                    graphs::meter_row(ui, "S sounds (5 kHz+)", lvl, -60.0, 0.0, Some(thr), hot, &format!("{lvl:.0} dB"));
                    graphs::meter_row(ui, "Cutting them by", m.deess_db, 0.0, 12.0, None, graphs::AMBER, &format!("-{:.1} dB", m.deess_db));
                    ab_row(ui, Stage::Deess, shown, hold);
                    ui::row(ui, "Threshold", |ui| {
                        slider(ui, &mut pr.deess_threshold_db, -50.0..=0.0, " dB");
                    });
                });
            }
            Node::Comp => {
                let desc = format!("Keeps loud parts in check · now -{:.1} dB", m.gr_db);
                ui::group(ui, icon::EQ, "Compressor", &desc, Some(&mut pr.comp), |ui| {
                    if let Some(t) = graphs::comp_graph(ui, &snap, m) {
                        pr.comp_threshold_db = t.clamp(-50.0, 0.0);
                    }
                    ui.add_space(6.0);
                    ab_row(ui, Stage::Comp, shown, hold);
                    ui::row(ui, "Threshold", |ui| {
                        slider(ui, &mut pr.comp_threshold_db, -50.0..=0.0, " dB");
                    });
                    ui::row(ui, "Ratio", |ui| {
                        ui.add(egui::Slider::new(&mut pr.comp_ratio, 1.0..=20.0).logarithmic(true).suffix(":1"));
                    });
                    ui::row(ui, "Attack", |ui| {
                        slider(ui, &mut pr.comp_attack_ms, 0.5..=50.0, " ms");
                    });
                    ui::row(ui, "Release", |ui| {
                        slider(ui, &mut pr.comp_release_ms, 20.0..=1000.0, " ms");
                    });
                    ui::row(ui, "Makeup gain", |ui| {
                        slider(ui, &mut pr.comp_makeup_db, 0.0..=24.0, " dB");
                    });
                });
            }
            Node::Limiter => ui::group(ui, icon::VOLUME, "Limiter", "Always on, so nothing you send can clip", None, |ui| {
                let red = Color32::from_rgb(232, 72, 72);
                let limiting = m.lim_db > 0.1;
                let ceil = 20.0 * dsp::LIMIT_CEIL.log10();
                let peak = format!("{:.1} dB", m.out_db);
                graphs::meter_row(ui, "Output peak", m.out_db, -60.0, 0.0, Some(ceil), if limiting { red } else { p.accent }, &peak);
                let amount = if limiting { format!("-{:.1} dB", m.lim_db) } else { "idle".into() };
                graphs::meter_row(ui, "Limiting", m.lim_db, 0.0, 12.0, None, red, &amount);
                ui::row(ui, "Output gain", |ui| {
                    slider(ui, &mut pr.output_gain_db, -20.0..=30.0, " dB");
                });
            }),
            Node::Output => {
                ui::setting(ui, icon::SPEAKERS, "Send to", "Where the cleaned sound goes. For Discord or other apps, pick CABLE Input; they then use CABLE Output as their mic", |ui| {
                    refresh |= refresh_button(ui);
                    pick(ui, "out", &mut d.output, outputs);
                });
                let convert = "Windows converts MicClean's 48000 Hz for it, which adds delay. Set it to 48000 Hz in Sound settings (Properties → Advanced)";
                if let Some(r) = off_rate(1) {
                    rate_note(ui, &d.output, r, convert);
                }
                ui::setting(ui, icon::HEADPHONE, "Monitor", "Hear yourself, e.g. on your headphones", |ui| {
                    pick_opt(ui, "mon", &mut d.monitor, outputs, "Off");
                });
                if let (Some(r), Some(mon)) = (off_rate(2), &d.monitor) {
                    rate_note(ui, mon, r, convert);
                }
                ui::group(ui, icon::SPEAKERS, "What gets sent", "After every effect and the limiter", None, |ui| {
                    level_row(ui, "Out", m.out_db);
                    ui::row(ui, "Output gain", |ui| {
                        slider(ui, &mut pr.output_gain_db, -20.0..=30.0, " dB");
                    });
                });
                section(ui, "Using it with Discord");
                ui::card(ui, |ui| {
                    for step in [
                        "1. Install VB-Cable (free, vb-audio.com) and set Send to = CABLE Input above.",
                        "2. In Windows Sound settings, set CABLE Input and CABLE Output to 48000 Hz (Properties → Advanced). Optional: rename CABLE Output to MicClean.",
                        "3. Windows Settings → System → Sound → your mic: Audio enhancements = Off.",
                        "4. In Discord, Voice & Video: Input Device = CABLE Output (or MicClean).",
                        "5. Turn off Discord's Noise Suppression, Echo Cancellation and Automatic Gain Control.",
                        "6. Turn off Automatic Input Sensitivity and drag its slider all the way left.",
                    ] {
                        ui.label(RichText::new(step).color(p.text2));
                    }
                });
            }
        }
        if refresh {
            self.refresh_devices();
        }
    }

    fn teach(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let learning = self.meters.learning;
        let busy = learning.is_some() || self.engine.is_none();

        section(ui, "Sounds to remove");
        let mut learn_noise = false;
        let mut forget = None;
        ui::card(ui, |ui| {
            ui::header(ui, icon::LEARN, "Learn a noise", "Name it, stay quiet, then make the sound for 5 seconds", |ui| {
                learn_noise = ui::accent_button(ui, !busy, "Learn (5 s)").clicked();
                ui.add(egui::TextEdit::singleline(&mut self.noise_name).hint_text("e.g. keyboard").desired_width(140.0));
            });
            if let Some((Kind::Noise, prog)) = learning {
                ui.add(egui::ProgressBar::new(prog).desired_height(4.0));
            }
            let looseness = self.saved.params.learn_match;
            for (i, n) in self.saved.profiles.noises.iter_mut().enumerate() {
                ui::row(ui, &n.name, |ui| {
                    let del = egui::Button::new(RichText::new(icon::DELETE).font(ui::icon_font(14.0))).frame(false);
                    if ui.add(del).on_hover_text("Forget").clicked() {
                        forget = Some(i);
                    }
                    ui::toggle(ui, &mut n.enabled).on_hover_text("Off lets this sound through for now");
                    if let Some(&d) = self.meters.match_dist.get(i) {
                        match_bar(ui, d, looseness);
                    }
                });
            }
        });
        if learn_noise {
            let n = self.noise_name.trim();
            self.pending_noise_name =
                if n.is_empty() { format!("Noise {}", self.saved.profiles.noises.len() + 1) } else { n.to_string() };
            self.noise_name.clear();
            self.learn(Kind::Noise, 5.0);
        }
        if let Some(i) = forget {
            self.saved.profiles.noises.remove(i);
        }

        section(ui, "Your voice");
        let learned = self.saved.profiles.keep.is_some();
        let desc = if learned {
            "Learned. Sounds like this are never muted"
        } else {
            "Talk and sing for 10 seconds so it never cuts you"
        };
        let (mut learn_voice, mut forget_voice) = (false, false);
        ui::card(ui, |ui| {
            ui::header(ui, icon::MUSIC, "Learn your voice", desc, |ui| {
                learn_voice = ui::accent_button(ui, !busy, "Learn (10 s)").clicked();
                forget_voice = learned && ui.button("Forget").clicked();
            });
            if let Some((Kind::Keep, prog)) = learning {
                ui.add(egui::ProgressBar::new(prog).desired_height(4.0));
            }
            if learned {
                let looseness = self.saved.params.learn_match;
                ui::row(ui, "Sounds like you right now", |ui| match_bar(ui, self.meters.keep_dist, looseness));
            }
        });
        if learn_voice {
            self.learn(Kind::Keep, 10.0);
        }
        if forget_voice {
            self.saved.profiles.keep = None;
        }
        ui::setting(ui, icon::SETTINGS, "Match looseness", "Higher means more sounds count as the learned ones", |ui| {
            ui.add(egui::Slider::new(&mut self.saved.params.learn_match, 1.0..=8.0));
        });
        if !self.learn_msg.is_empty() {
            ui.label(RichText::new(&self.learn_msg).color(p.text2));
        }
    }

    fn test(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let recording = self.record_progress.is_some();
        let mut record = false;
        ui::card(ui, |ui| {
            ui::header(ui, icon::RECORD, "Record a test", "10 seconds of your raw mic. Talk, then sing a long note", |ui| {
                record = ui::accent_button(ui, !recording && self.engine.is_some(), "Record").clicked();
            });
            if let Some(prog) = self.record_progress {
                ui.add(egui::ProgressBar::new(prog).desired_height(4.0));
            }
        });
        if record {
            self.shared.lock().unwrap().record = Some(vec![0.0; RECORD_SECS * dsp::SR as usize]);
        }

        let has_clip = self.clip.is_some() && !recording;
        let (mut original, mut processed, mut stop) = (false, false, false);
        ui::setting(ui, icon::PLAY, "Play it back", "Processed runs the clip through your current settings: tweak, replay", |ui| {
            stop = ui::icon_button(ui, self.playback.is_some(), icon::STOP, "Stop").clicked();
            processed = ui::icon_button(ui, has_clip, icon::PLAY, "Processed").clicked();
            original = ui::icon_button(ui, has_clip, icon::PLAY, "Original").clicked();
        });
        if original {
            self.play(false);
        }
        if processed {
            self.play(true);
        }
        if stop {
            self.stop_playback();
        }
        ui.label(RichText::new("Plays on the Monitor device, or your default speakers if Monitor is off.").small().color(p.text2));
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui::setting(ui, icon::POWER, "Start with Windows", "Opens hidden in the tray when you sign in", |ui| {
            changed = ui::toggle(ui, &mut self.autostart).changed();
        });
        if changed && !autostart::set(self.autostart) {
            self.autostart = autostart::enabled();
            self.status = "Couldn't change the Windows startup entry.".into();
        }
        let mut reset = false;
        ui::setting(ui, icon::REFRESH, "Reset all sliders", "Back to the defaults. Learned sounds are kept", |ui| {
            reset = ui.button("Reset").clicked();
        });
        if reset {
            self.saved.params = Params::default();
        }
        self.updates(ui);
        let mut show_log = false;
        ui::setting(ui, icon::FOLDER, "Log file", "Device errors, glitches, restarts and updates, for troubleshooting. It stays on this PC", |ui| {
            show_log = ui.button("Show in folder").clicked();
        });
        if show_log {
            use std::os::windows::process::CommandExt;
            // raw_arg: explorer wants /select,"path" exactly, which normal argument quoting breaks.
            let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", logfile::path().display())).spawn();
        }
        ui::setting(
            ui,
            icon::INFO,
            "About MicClean",
            &format!("Version {}. Closing the window keeps it running in the tray. Right-click the tray icon to quit", update::VERSION),
            |_| {},
        );
        ui::setting(ui, icon::INFO, "Open-source licenses", "MicClean is built with these libraries", |ui| {
            self.show_notices |= ui.button("View").clicked();
        });
    }

    /// The licenses of every library in the exe, a visible screenful at a time.
    fn notices(&mut self, ctx: &egui::Context) {
        static LINES: OnceLock<Vec<&str>> = OnceLock::new();
        let lines = LINES.get_or_init(|| NOTICES.lines().collect());
        let mut close = false;
        let resp = egui::Modal::new(egui::Id::unique("notices")).show(ctx, |ui| {
            let size = ctx.content_rect().size();
            ui.set_width((size.x - 80.0).clamp(300.0, 760.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new("Open-source licenses").font(ui::semibold(18.0)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = ui::icon_button(ui, true, icon::CLOSE, "Close").clicked();
                });
            });
            ui.add_space(6.0);
            let row = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both().max_height(size.y - 160.0).auto_shrink(false).show_rows(ui, row, lines.len(), |ui, range| {
                for line in &lines[range] {
                    ui.add(egui::Label::new(RichText::new(*line).monospace()).extend());
                }
            });
        });
        if close || resp.should_close() {
            self.show_notices = false;
        }
    }
}

impl App {
    /// "Update available" dialog on launch. Both switches apply whichever button is pressed.
    fn update_popup(&mut self, ctx: &egui::Context) {
        let update::State::Available(r) = self.updater.state() else {
            self.update_prompt = false;
            return;
        };
        let p = ui::pal_ctx(ctx);
        let (mut now, mut later) = (false, false);
        let (mut auto, mut quiet) = (self.saved.auto_update, self.saved.update_popup_off);
        let resp = egui::Modal::new(egui::Id::unique("update_prompt")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new(format!("MicClean {} is available", r.version)).font(ui::semibold(18.0)));
            ui.add_space(4.0);
            ui.label(RichText::new(format!("You have {}. Updating closes MicClean for a few seconds, so your voice drops out briefly.", update::VERSION)).color(p.text2));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui::toggle(ui, &mut auto);
                ui.label("Update automatically from now on");
            });
            ui.horizontal(|ui| {
                ui::toggle(ui, &mut quiet);
                ui.label("Don't show this popup again");
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                now = ui::accent_button(ui, true, "Update now").clicked();
                later = ui.button("Later").clicked();
            });
        });
        later |= resp.should_close();
        self.saved.auto_update = auto;
        self.saved.update_popup_off = quiet;
        if now || later {
            self.update_prompt = false;
            self.update_dismissed = Some(r.version.clone());
        }
        if now {
            self.updater.install(r);
        }
    }

    fn updates(&mut self, ui: &mut egui::Ui) {
        use update::State;
        let st = self.updater.state();
        let desc = match &st {
            _ if !self.installed => "This copy isn't installed, so it doesn't update itself".to_string(),
            State::Idle => format!("You have {}", update::VERSION),
            State::Checking => "Checking GitHub...".into(),
            State::UpToDate => format!("You're up to date ({})", update::VERSION),
            State::Available(r) => format!("Version {} is available. Installing closes MicClean for a few seconds, so your voice drops out briefly", r.version),
            State::Installing => "Downloading and installing. MicClean restarts by itself".into(),
            State::Failed(e) => format!("Couldn't update: {e}"),
        };
        let (mut install_rel, mut check) = (None, false);
        let installed = self.installed;
        ui::setting(ui, icon::REFRESH, "Updates", &desc, |ui| match &st {
            State::Available(r) if installed => {
                if ui::accent_button(ui, true, &format!("Update to {}", r.version)).clicked() {
                    install_rel = Some(r.clone());
                }
            }
            State::Checking | State::Installing => {
                ui.spinner();
            }
            _ => check = ui.add_enabled(installed, egui::Button::new("Check now")).clicked(),
        });
        let (mut check_on, mut popup_on) = (!self.saved.update_check_off, !self.saved.update_popup_off);
        ui::setting(ui, icon::INFO, "Check for updates", "Asks GitHub when MicClean starts, then once a day. Nothing else is sent", |ui| {
            ui::toggle(ui, &mut check_on);
        });
        ui::setting(ui, icon::INFO, "Show a popup when there's an update", "Off: the Updates card above still shows it", |ui| {
            ui::toggle(ui, &mut popup_on);
        });
        ui::setting(ui, icon::REFRESH, "Install updates automatically", "Waits until you aren't talking, then closes MicClean for a few seconds", |ui| {
            ui::toggle(ui, &mut self.saved.auto_update);
        });
        (self.saved.update_check_off, self.saved.update_popup_off) = (!check_on, !popup_on);
        if let Some(r) = install_rel {
            self.updater.install(r);
        }
        if check {
            self.updater.check();
        }
    }
}

/// Output spectrum (bars), input spectrum (line) and the EQ curve (yellow, its own +-18 dB scale).
fn spectrum_card(ui: &mut egui::Ui, m: &Meters, p: &Params) {
    let pal = ui::pal(ui);
    ui::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Spectrum").font(ui::semibold(14.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new("bars: out  ·  line: in  ·  yellow: EQ").small().color(pal.text2)).truncate());
            });
        });
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 140.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, pal.field);
        let x_of = |f: f32| rect.left() + rect.width() * ((f / SPEC_LO).ln() / SPEC_SPAN.ln()).clamp(0.0, 1.0);
        let y_db = |db: f32| rect.bottom() - rect.height() * ((db + 90.0) / 90.0).clamp(0.0, 1.0);
        let y_eq = |db: f32| rect.center().y - rect.height() * 0.5 * (db / 18.0).clamp(-1.0, 1.0);
        for (f, label) in [(100.0, "100"), (1000.0, "1k"), (10_000.0, "10k")] {
            let x = x_of(f);
            painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(1.0, pal.divider));
            painter.text(
                Pos2::new(x + 3.0, rect.bottom() - 2.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                egui::FontId::proportional(10.0),
                pal.text2,
            );
        }
        let w = rect.width() / SPEC_BINS as f32;
        for (i, &db) in m.spec_out.0.iter().enumerate() {
            let x0 = rect.left() + i as f32 * w;
            let bar = Rect::from_min_max(Pos2::new(x0 + 0.5, y_db(db)), Pos2::new(x0 + w - 0.5, rect.bottom()));
            painter.rect_filled(bar, 1.0, pal.accent.gamma_multiply(0.8));
        }
        let input: Vec<Pos2> =
            m.spec_in.0.iter().enumerate().map(|(i, &db)| Pos2::new(rect.left() + (i as f32 + 0.5) * w, y_db(db))).collect();
        painter.add(egui::Shape::line(input, Stroke::new(1.0, pal.text2)));

        let freqs: Vec<f32> = (0..=150).map(|i| SPEC_LO * SPEC_SPAN.powf(i as f32 / 150.0)).collect();
        let gains = dsp::eq_curve(p, &freqs);
        let amber = Color32::from_rgb(240, 190, 50);
        let col = if p.eq { amber } else { amber.gamma_multiply(0.3) };
        let curve =
            freqs.iter().zip(&gains).map(|(&f, &g)| Pos2::new(x_of(f), y_eq(if p.eq { g } else { 0.0 }))).collect();
        painter.add(egui::Shape::line(curve, Stroke::new(2.0, col)));
    });
}

fn pick(ui: &mut egui::Ui, id: &str, v: &mut String, list: &[String]) {
    egui::ComboBox::from_id_salt(id).selected_text(v.as_str()).truncate().show_ui(ui, |ui| {
        for n in list {
            ui.selectable_value(v, n.clone(), n);
        }
    });
}

fn pick_opt(ui: &mut egui::Ui, id: &str, v: &mut Option<String>, list: &[String], none: &str) {
    egui::ComboBox::from_id_salt(id).selected_text(v.as_deref().unwrap_or(none)).truncate().show_ui(ui, |ui| {
        ui.selectable_value(v, None, none);
        for n in list {
            ui.selectable_value(v, Some(n.clone()), n);
        }
    });
}

const ICON: u32 = appicon::SIZE;

fn icon_rgba(open: bool) -> Vec<u8> {
    appicon::rgba(open, ICON)
}

/// "Start with Windows" = a value under the per-user Run key, written with reg.exe.
mod autostart {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> bool {
        Command::new("reg")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn enabled() -> bool {
        reg(&["query", KEY, "/v", "MicClean"])
    }

    pub fn set(on: bool) -> bool {
        if !on {
            return reg(&["delete", KEY, "/v", "MicClean", "/f"]);
        }
        let Ok(exe) = std::env::current_exe() else { return false };
        let cmd = format!("\"{}\" --minimized", exe.display());
        reg(&["add", KEY, "/v", "MicClean", "/t", "REG_SZ", "/d", &cmd, "/f"])
    }

    #[cfg(test)]
    mod tests {
        /// Touches the real Run key, so opt-in: `cargo test -- --ignored`. Restores what was there.
        #[test]
        #[ignore]
        fn run_entry_is_quoted() {
            let before = super::enabled();
            assert!(super::set(true));
            let out = super::Command::new("reg").args(["query", super::KEY, "/v", "MicClean"]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            super::set(before);
            let exe = std::env::current_exe().unwrap();
            assert!(text.contains(&format!("\"{}\" --minimized", exe.display())), "{text}");
            assert_eq!(super::enabled(), before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(String);

    impl eframe::Storage for Mem {
        fn get_string(&self, _: &str) -> Option<String> {
            Some(self.0.clone())
        }
        fn set_string(&mut self, _: &str, value: String) {
            self.0 = value;
        }
        fn remove_string(&mut self, _: &str) {}
        fn flush(&mut self) {}
    }

    #[test]
    fn suggested_gate_follows_the_input_gain_you_kept() {
        let s = Params { input_gain_db: 12.0, gate_threshold_db: -49.0, ..Params::default() };
        let mut p = Params { input_gain_db: 6.0, ..Params::default() };
        Node::Gate.suggest(&mut p, &s);
        assert_eq!(p.gate_threshold_db, -55.0, "6 dB less gain than suggested: threshold 6 dB lower");
        assert_eq!(p.input_gain_db, 6.0, "the gate's Suggest leaves the gain alone");
    }

    #[test]
    fn old_fixed_eq_save_loads_as_bands() {
        // A real save from before the parametric EQ.
        let old = r#"(devices:(input:"Test Microphone",output:"CABLE Input (VB-Audio Virtual Cable)",monitor:None,echo_ref:None),params:(bypass:false,input_gain_db:0.0,denoise:true,denoise_strength:1.0,sing_protect:0.3,gate:false,gate_threshold_db:-50.0,vad_threshold:0.5,gate_hold_ms:400.0,gate_floor_db:-60.0,learn_match:3.0,agc:true,agc_target_db:-14.0,agc_max_gain_db:15.0,eq:true,low_cut_hz:165.0,low_gain_db:0.0,mid_freq_hz:300.0,mid_gain_db:2.8,high_gain_db:3.0,deess:true,deess_threshold_db:-24.0,comp:true,comp_threshold_db:-24.0,comp_ratio:3.0,comp_attack_ms:5.0,comp_release_ms:120.0,comp_makeup_db:4.0,output_gain_db:6.0),profiles:(noises:[],keep:None))"#;
        let mut store = Mem(old.into());
        let s = load(&store);
        assert_eq!(s.devices.input, "Test Microphone", "settings were thrown away");
        assert_eq!(s.params.output_gain_db, 6.0);
        let b = s.params.bands;
        assert_eq!(b[0].map(|b| (b.kind, b.freq)), Some((BandKind::LowCut, 165.0)));
        assert!(b[1].is_none(), "0 dB low shelf should not become a band");
        assert_eq!(b[2].map(|b| (b.kind, b.freq, b.gain_db)), Some((BandKind::Bell, 300.0, 2.8)));
        assert_eq!(b[3].map(|b| (b.kind, b.gain_db)), Some((BandKind::HighShelf, 3.0)));
        // Starter presets are built from these settings, so the EQ carries into all of them.
        let names: Vec<&str> = s.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Talking", "Singing", "Gaming"]);
        assert!(same_preset(&s.presets[0].params, &s.params), "Talking should match the loaded settings");
        assert!(s.presets.iter().all(|p| p.params.bands == s.params.bands));

        // After one save the file has bands only, and editing them sticks.
        let mut edited = s;
        edited.params.bands[4] = Some(Band { kind: BandKind::Notch, freq: 60.0, gain_db: 0.0, q: 8.0, on: true });
        eframe::set_value(&mut store, eframe::APP_KEY, &edited);
        assert!(!store.0.contains("low_cut_hz"));
        assert_eq!(load(&store).params.bands, edited.params.bands);
    }
}
