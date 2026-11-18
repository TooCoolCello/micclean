#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appicon;
mod audio;
mod backup;
mod dsp;
mod graphs;
#[cfg(windows)]
mod install;
mod logfile;
mod os;
mod plugins;
mod smart;
mod ui;
mod update;

use audio::{Devices, Engine, Shared};
use dsp::{Band, BandKind, Denoiser, Fx, FxKind, Kind, Meters, NoiseProfile, Params, Profiles, SPEC_BINS, SPEC_LO, SPEC_SPAN};
use eframe::egui::{self, Align, Color32, Layout, Pos2, Rect, RichText, Stroke, ViewportCommand};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use os::autostart;
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use ui::icon;

/// "Show your window" pings from a second launch go to this localhost port plus the Windows
/// session number (the user id on a Mac), so each signed-in user has their own.
const INSTANCE_PORT: u16 = 47391;
const RECORD_SECS: usize = 10;
/// A raw mic peak this close to full scale (-0.3 dBFS) is clipping before MicClean.
const CLIP_LEVEL: f32 = 0.966;
/// Set when a second launch or the tray asks for the window.
static SHOW: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

fn main() -> eframe::Result {
    // Velopack's installer and updater run this exe for their hooks: answered, then it exits.
    update::startup();
    // A plugin search runs each plugin file in a copy of this exe, which only lists it.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--scan-plugin") {
        plugins::scan_main(&args[2..]);
        return Ok(());
    }
    // Each plugin in the rack runs in a copy of this exe of its own.
    if args.get(1).is_some_and(|a| a == "--plugin-host") {
        plugins::host_main(&args[2..]);
        return Ok(());
    }
    // Settings -> Apps -> Uninstall runs this, before the single-instance check below.
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--uninstall") {
        install::uninstall(std::env::args().any(|a| a == "--silent"));
        return Ok(());
    }
    let mut first = os::first_copy();
    if !first && std::env::args().any(|a| a == "--updated") {
        // Started by setup after an update: the old copy may still be closing.
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            first = os::first_copy();
            if first {
                break;
            }
        }
    }
    let test_build = if os::TEST_FEED.is_some() { 100 } else { 0 };
    let port = INSTANCE_PORT + test_build + (os::session() % 100) as u16;
    if !first {
        // Already running: that copy shows its window, this one leaves. Two copies would
        // both grab the mic and double the audio going to Discord.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
        return Ok(());
    }
    // Only for the window pings: if another program holds the port, the lock still works.
    if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        std::thread::spawn(move || {
            for _ in listener.incoming().flatten() {
                SHOW.store(true, Ordering::Relaxed);
                if let Some(ctx) = CTX.get() {
                    ctx.request_repaint();
                }
            }
        });
    }

    backup::run(&os::settings_dir());
    let hidden = std::env::args().any(|a| a == "--minimized");
    let options = eframe::NativeOptions {
        // eframe's own default on Windows; on a Mac it would skip the data folder.
        persistence_path: Some(os::settings_dir().join("app.ron")),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            // Below this, header rows (a picker or buttons beside the text) don't fit.
            .with_min_inner_size([640.0, 480.0])
            .with_title(os::NAME)
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

/// Everything that survives a restart (eframe stores it in os::settings_dir).
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
    /// Setup has been seen, so the app opens on Home.
    setup_done: bool,
    /// Plugins in the rack and in presets, by key, with their settings.
    plugins: Vec<plugins::Entry>,
    /// What the last plugin search found.
    plugin_list: Vec<plugins::Info>,
    /// Folders you added for the plugin search, on top of the usual ones.
    plugin_dirs: Vec<std::path::PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Preset {
    name: String,
    params: Params,
}

/// `current` with a preset picked: a preset is a style (its rack included), so what Smart
/// setup measured for your mic and voice (input gain, gate level, the first Tone's EQ, the
/// first compressor's level, loudness) stays, and so does Bypass (a listening aid, not part of
/// a preset).
fn with_preset(current: &Params, preset: &Params) -> Params {
    let mut p = Params {
        bypass: current.bypass,
        input_gain_db: current.input_gain_db,
        gate_threshold_db: current.gate_threshold_db,
        output_gain_db: current.output_gain_db,
        ..preset.clone()
    };
    if let (Some(b), Some(mine)) = (p.bands_mut(), current.bands()) {
        *b = *mine;
    }
    if let (Some(c), Some(mine)) = (p.comp_mut(), current.comp()) {
        (c.threshold_db, c.makeup_db) = (mine.threshold_db, mine.makeup_db);
    }
    p
}

/// The current settings are this preset's style.
fn same_preset(preset: &Params, current: &Params) -> bool {
    with_preset(current, preset) == *current
}

/// Talking = your settings as they are; Singing and Gaming are small changes from there,
/// so your EQ and gains carry into all three.
fn starter_presets(current: &Params) -> Vec<Preset> {
    let talking = Params { bypass: false, ..current.clone() };
    let mut singing = Params { sing_protect: 0.7, gate_hold_ms: 900.0, gate_hysteresis_db: 10.0, ..talking.clone() };
    if let Some(c) = singing.comp_mut() {
        c.ratio = 2.0;
    }
    if let Some(t) = singing.deess_threshold_mut() {
        *t += 4.0;
    }
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
    /// A rack effect, by position.
    Fx(usize),
}

impl Stage {
    fn turn_off(self, p: &mut Params) {
        match self {
            Stage::Denoise => p.denoise = false,
            Stage::Gate => p.gate = false,
            Stage::Agc => p.agc = false,
            Stage::Fx(i) => {
                if let Some(s) = &mut p.rack[i] {
                    s.on = false;
                }
            }
        }
    }
}

/// "Hold to hear it off": bypasses one stage only while the button is held down. It changes
/// what's sent, so without Hear myself (`can_hear`) only friends would notice.
fn ab_row(ui: &mut egui::Ui, stage: Stage, shown: Option<Stage>, hold: &mut Option<Stage>, can_hear: bool) {
    ui::row(ui, "Compare", |ui| {
        let held = shown == Some(stage);
        let label = if held { "Off while held" } else { "Hold to hear it off" };
        let button = ui.add_enabled(can_hear, egui::Button::new(label).selected(held));
        if button.on_disabled_hover_text("Turn on Hear myself (Home) to hear the difference").is_pointer_button_down_on() {
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

/// Before the rack (0.10 and older), the effects in it were fixed fields, in this order. Read
/// from a save that has no rack yet; their defaults are the old ones. Air (new with the rack)
/// joins them, off.
#[derive(Deserialize)]
#[serde(default)]
struct OldFx {
    /// The save has a rack: nothing to convert.
    #[serde(deserialize_with = "present")]
    rack: bool,
    eq: bool,
    bands: [Option<Band>; dsp::BANDS],
    deess: bool,
    deess_threshold_db: f32,
    warmth: bool,
    warmth_amount: f32,
    comp: bool,
    comp_threshold_db: f32,
    comp_ratio: f32,
    comp_attack_ms: f32,
    comp_release_ms: f32,
    comp_makeup_db: f32,
}

impl Default for OldFx {
    fn default() -> Self {
        let comp = dsp::Comp { threshold_db: -24.0, ratio: 3.0, attack_ms: 5.0, release_ms: 120.0, makeup_db: 4.0 };
        let Fx::Eq { bands } = FxKind::Eq.new() else { unreachable!() };
        Self {
            rack: false,
            eq: true,
            bands,
            deess: true,
            deess_threshold_db: -24.0,
            warmth: false,
            warmth_amount: 0.3,
            comp: true,
            comp_threshold_db: comp.threshold_db,
            comp_ratio: comp.ratio,
            comp_attack_ms: comp.attack_ms,
            comp_release_ms: comp.release_ms,
            comp_makeup_db: comp.makeup_db,
        }
    }
}

impl OldFx {
    /// `p` with these as its rack, if the save had none.
    fn convert(&self, p: &mut Params) {
        if self.rack {
            return;
        }
        let comp = dsp::Comp {
            threshold_db: self.comp_threshold_db,
            ratio: self.comp_ratio,
            attack_ms: self.comp_attack_ms,
            release_ms: self.comp_release_ms,
            makeup_db: self.comp_makeup_db,
        };
        let fx = [
            (self.eq, Fx::Eq { bands: self.bands }),
            (false, FxKind::Air.new()),
            (self.deess, Fx::Deess { threshold_db: self.deess_threshold_db }),
            (self.warmth, Fx::Warmth { amount: self.warmth_amount }),
            (self.comp, Fx::Comp(comp)),
        ];
        p.rack = [None; dsp::RACK];
        for (i, (on, fx)) in fx.into_iter().enumerate() {
            p.rack[i] = Some(dsp::Slot { id: i as u32 + 1, on, fx });
        }
    }
}

/// For `OldFx::rack`: true when the field is there at all.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(d).map(|_| true)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OldRack {
    params: OldFx,
    presets: Vec<OldPreset>,
    smart: Option<OldFx>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OldPreset {
    params: OldFx,
}

fn load(storage: &dyn eframe::Storage) -> Saved {
    parse_saved(storage.get_string(eframe::APP_KEY).as_deref()).unwrap_or_else(|e| {
        // The file itself is still in the backups.
        logfile::line(&format!("Couldn't read the settings, so they're back to defaults: {e}"));
        parse_saved(None).unwrap_or_default()
    })
}

/// Settings from their saved text (eframe's "app" value; restoring a backup uses it too),
/// turning an old fixed EQ into the same filters as bands, old fixed effects into a rack, old
/// learned-sound floors into ones before input gain, and every value into its slider's range.
/// The next save writes the new forms, so each converts once.
// ponytail: drop OldEq once nobody has a pre-parametric save left.
fn parse_saved(text: Option<&str>) -> Result<Saved, ron::error::SpannedError> {
    let mut saved: Saved = text.map(ron::from_str).transpose()?.unwrap_or_default();
    if let Some(old) = text.and_then(|t| ron::from_str::<OldRack>(t).ok()) {
        old.params.convert(&mut saved.params);
        for (p, o) in saved.presets.iter_mut().zip(&old.presets) {
            o.params.convert(&mut p.params);
        }
        if let (Some(s), Some(o)) = (&mut saved.smart, &old.smart) {
            o.convert(s);
        }
    }
    if let Some(OldSave { params: o }) = text.and_then(|t| ron::from_str(t).ok())
        && let Some(bands) = saved.params.bands_mut()
    {
        let band = |kind, freq, q, gain_db| Some(Band { kind, freq, gain_db, q, on: true });
        let boost = |kind, freq, q, gain_db: f32| if gain_db != 0.0 { band(kind, freq, q, gain_db) } else { None };
        *bands = [
            band(BandKind::LowCut, o.low_cut_hz, 0.707, 0.0),
            boost(BandKind::LowShelf, 200.0, 0.707, o.low_gain_db),
            boost(BandKind::Bell, o.mid_freq_hz, 1.0, o.mid_gain_db),
            boost(BandKind::HighShelf, 5000.0, 0.707, o.high_gain_db),
            None,
        ];
    }
    saved.params.sanitize();
    saved.presets.iter_mut().for_each(|p| p.params.sanitize());
    saved.smart.iter_mut().for_each(Params::sanitize);
    saved.profiles.migrate_floors(saved.params.input_gain_db);
    if !saved.presets_seeded {
        saved.presets = starter_presets(&saved.params);
        saved.presets_seeded = true;
    }
    // A listening aid: left on, it would send the raw mic after the next reboot.
    saved.params.bypass = false;
    Ok(saved)
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Setup,
    Advanced,
    Settings,
}

const NAV: [(Page, &str, &str); 3] = [
    (Page::Home, icon::HOME, "Home"),
    (Page::Setup, icon::SETUP, "Setup"),
    (Page::Advanced, icon::EQ, "Advanced"),
];

/// What a problem banner's button does.
#[derive(Clone, Copy)]
enum Fix {
    Setup,
    Restart,
    BypassOff,
    SoundSettings,
    Quieter,
    EchoEverything,
}

/// Levels while you talk, averaged over about a second, for the Home verdicts and banners.
#[derive(Default)]
struct Stats {
    /// Last meters frame counted, and how many talking frames so far.
    frame: u64,
    voiced: u32,
    /// Typical peak before MicClean's input gain, and at the output (dBFS).
    raw_peak: f32,
    out_peak: f32,
    /// Share of talking frames the limiter worked on.
    lim_busy: f32,
}

/// What you listen on: the system's default playback device, unless that's the cable. (On
/// Windows only VB-Cable's CABLE ends: Voicemeeter's "VB-Audio" outputs are real listening.)
fn listen_device(outputs: &[String]) -> Option<String> {
    let cable = |n: &String| if cfg!(windows) { n.contains("CABLE") } else { audio::is_cable(n) };
    audio::default_output_name().filter(|n| !cable(n)).or_else(|| outputs.iter().find(|n| !cable(n)).cloned())
}

/// What echo cancellation can listen to: every app but MicClean (the default), or one device.
fn echo_choices(outputs: &[String]) -> Vec<String> {
    std::iter::once(audio::EVERYTHING.to_string()).chain(outputs.iter().cloned()).collect()
}

/// A box in the signal chain; clicking one opens its settings under the chain.
#[derive(Clone, Copy, PartialEq)]
enum Node {
    Input,
    Echo,
    Noise,
    Gate,
    Level,
    /// A rack effect, by position.
    Fx(usize),
    Limiter,
    Output,
}

impl Node {
    fn title(self) -> &'static str {
        match self {
            Node::Input => "Your mic",
            Node::Echo => "Echo cancellation",
            Node::Noise => "Noise removal",
            Node::Gate => "Mute between words",
            Node::Level => "Even out volume",
            Node::Fx(_) => "Effect",
            Node::Limiter => "Clip guard",
            Node::Output => "What gets sent",
        }
    }

    /// Its title, a rack effect's by its kind (a plugin's by its own name).
    fn name(self, p: &Params, plugins: &[plugins::Entry]) -> String {
        match self {
            Node::Fx(i) => match p.rack[i] {
                Some(dsp::Slot { fx: Fx::Plugin { key }, .. }) => plugins.iter().find(|e| e.key == key).map_or("Plugin".into(), |e| e.info.name.clone()),
                s => s.map_or("Effect", |s| s.fx.kind().title()).into(),
            },
            n => n.title().into(),
        }
    }

    /// The step's on/off switch, for those that have one.
    fn switch(self, p: &mut Params) -> Option<&mut bool> {
        match self {
            Node::Noise => Some(&mut p.denoise),
            Node::Gate => Some(&mut p.gate),
            Node::Level => Some(&mut p.agc),
            Node::Fx(i) => p.rack[i].as_mut().map(|s| &mut s.on),
            Node::Input | Node::Echo | Node::Limiter | Node::Output => None,
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
            Node::Fx(i) => {
                // A plugin's settings are its own (in its window).
                if let Some(s) = p.rack[i].as_mut().filter(|s| s.fx.kind() != FxKind::Plugin) {
                    s.fx = s.fx.kind().new();
                }
            }
            Node::Limiter | Node::Output => p.output_gain_db = d.output_gain_db,
        }
    }

    /// Smart setup chooses settings for this box: the fixed steps it sets, and the first rack
    /// effect of each kind it sets.
    fn smart(self, p: &Params) -> bool {
        match self {
            Node::Input | Node::Noise | Node::Gate | Node::Limiter | Node::Output => true,
            Node::Fx(i) => p.rack[i].is_some_and(|s| s.fx.kind() != FxKind::Plugin && p.first(s.fx.kind()).is_some_and(|f| f.id == s.id)),
            Node::Echo | Node::Level => false,
        }
    }

    /// The boxes Smart setup chooses settings for in `p`.
    fn smart_nodes(p: &Params) -> Vec<Node> {
        let fixed = [Node::Input, Node::Noise, Node::Gate, Node::Limiter, Node::Output];
        fixed.into_iter().chain((0..dsp::RACK).map(Node::Fx)).filter(|n| n.smart(p)).collect()
    }

    /// Copies what Smart setup chose (`s`) for this box. Other boxes are left alone.
    fn suggest(self, p: &mut Params, s: &Params) {
        match self {
            Node::Input => p.input_gain_db = s.input_gain_db,
            Node::Noise => p.denoise_strength = s.denoise_strength,
            // Chosen for the suggested input gain, so it follows the gain you actually have.
            Node::Gate => p.gate_threshold_db = (s.gate_threshold_db + p.input_gain_db - s.input_gain_db).clamp(-80.0, -10.0),
            Node::Fx(i) => {
                let Some(mine) = &mut p.rack[i] else { return };
                let Some(chosen) = s.first(mine.fx.kind()) else { return };
                match (&mut mine.fx, chosen.fx) {
                    (Fx::Eq { bands }, Fx::Eq { bands: b }) => *bands = b,
                    (Fx::Deess { threshold_db }, Fx::Deess { threshold_db: t }) => *threshold_db = t,
                    (Fx::Warmth { .. }, Fx::Warmth { .. }) => (mine.on, mine.fx) = (chosen.on, chosen.fx),
                    // Attack and release stay yours.
                    (Fx::Comp(c), Fx::Comp(cs)) => (c.threshold_db, c.ratio, c.makeup_db) = (cs.threshold_db, cs.ratio, cs.makeup_db),
                    _ => {}
                }
            }
            Node::Limiter | Node::Output => p.output_gain_db = s.output_gain_db,
            Node::Echo | Node::Level => {}
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
    /// Your mic before the chain, to try the chosen settings on. Arrives just before the end.
    audio: Option<Vec<f32>>,
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
    /// The window is hidden in the tray, so an update starts the new copy there too.
    hidden: bool,
    /// Last time you were talking, so an automatic update waits for a pause.
    last_voice: Instant,
    /// When the mic clipped before MicClean in the last 10 s; 3 of them show a warning until
    /// `clipping_until`.
    clips: Vec<Instant>,
    clipping_until: Option<Instant>,
    /// Settings' Reset asks once more; then Undo has the settings from before.
    reset_armed: bool,
    reset_undo: Option<Params>,
    /// What the last backup, restore or dropped file did.
    backup_msg: String,
    page: Page,
    /// The chain box whose settings are open on Home.
    node: Option<Node>,
    calib: Option<Calib>,
    /// What the last Smart setup changed (or why it couldn't), and the settings from before it.
    smart_report: Vec<String>,
    smart_undo: Option<Params>,
    /// Windows' rate for the input, output and monitor devices, looked up on each restart.
    rates: [Option<u32>; 3],
    /// How many channels the input device has, for its channel picker.
    in_channels: u16,
    /// Falling peak holds for the meters (dBFS): each input channel and the input before
    /// MicClean's gain, and what gets sent.
    chan_hold: [f32; audio::MAX_CH],
    raw_hold: f32,
    out_hold: f32,
    stats: Stats,
    /// Problem banners closed until the next start.
    dismissed: Vec<&'static str>,
    show_notices: bool,
    /// What the log file already has, so it gets changes only.
    logged_status: String,
    logged_glitches: u32,
    next_glitch_log: Instant,
    logged_update: Option<std::mem::Discriminant<update::State>>,
    hwnd: Option<isize>,
    title_dark: Option<bool>,
    /// The loaded plugins (GUI side), a search in progress, and the plugin the last run
    /// crashed in.
    plugins: plugins::Host,
    search: Option<Arc<Mutex<plugins::Search>>>,
    plugin_crash: Option<String>,
    /// What the plugin list in "Add effect" is filtered by.
    plugin_filter: String,
    /// The rack effect being dragged to a new place.
    drag_fx: Option<Drag>,
    /// One effect row's height plus the gap under it, as last drawn, for laying them out while
    /// one is dragged.
    row_pitch: f32,
    /// The plugin search's bar is folded down to its tab.
    search_folded: bool,
    /// When the last plugin search ended, and how many effects it found.
    search_done: Option<(Instant, usize)>,
    /// The settings as last written to the safe copy (backup::keep).
    kept: String,
    /// DeepFilterNet: loading, loaded (handed to the audio), or why it couldn't load.
    deep_load: Option<std::thread::JoinHandle<Result<dsp::Deep, String>>>,
    deep_ready: bool,
    deep_error: Option<String>,
    /// The recording being cleaned up for "Cleaned", off the GUI thread.
    rendering: Option<std::thread::JoinHandle<Vec<f32>>>,
}

/// An effect being dragged: its place, where the pointer holds it (from its top), and the
/// drag's start time (a fresh set of slide animations each drag).
#[derive(Clone, Copy)]
struct Drag {
    from: usize,
    grab: f32,
    started: u64,
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
        // Nothing saved yet. (No storage at all isn't one: it would turn autostart on each start.)
        let first_start = cc.storage.is_some_and(|s| s.get_string(eframe::APP_KEY).is_none());
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
                .with_tooltip(os::NAME)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .ok()
        });

        let now = Instant::now();
        let updater = update::Updater::new();
        let installed = updater.installed();
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
            updater,
            installed,
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
            hidden: false,
            last_voice: now,
            clips: Vec::new(),
            clipping_until: None,
            reset_armed: false,
            reset_undo: None,
            backup_msg: String::new(),
            page: Page::Home,
            node: None,
            calib: None,
            smart_report: Vec::new(),
            smart_undo: None,
            rates: [None; 3],
            in_channels: 1,
            chan_hold: [-120.0; audio::MAX_CH],
            raw_hold: -120.0,
            out_hold: -120.0,
            stats: Stats::default(),
            dismissed: Vec::new(),
            show_notices: false,
            logged_status: String::new(),
            logged_glitches: 0,
            next_glitch_log: now,
            logged_update: None,
            hwnd,
            title_dark: None,
            plugins: plugins::Host::default(),
            search: None,
            plugin_crash: None,
            plugin_filter: String::new(),
            drag_fx: None,
            row_pitch: 44.0,
            search_folded: false,
            search_done: None,
            kept: String::new(),
            deep_load: None,
            deep_ready: false,
            deep_error: None,
            rendering: None,
        };
        if app.installed {
            // The install from before Velopack (0.11 and older), if it's still there.
            #[cfg(windows)]
            if let Ok(exe) = std::env::current_exe() {
                install::remove_legacy(&exe).iter().for_each(|n| logfile::line(n));
            }
            // An update to this version didn't take: it isn't installed on its own again or
            // offered in a popup (Settings can still try).
            match update::failed_version() {
                Some(version) => {
                    if update::restarted() {
                        // Velopack's updater started this copy again just now: say so (and log_changes logs it).
                        let why = format!("version {version} didn't install, so you still have {}", update::VERSION);
                        app.updater.set(update::State::Failed(why));
                        app.next_update_check = now + Duration::from_secs(24 * 3600);
                    }
                    app.update_dismissed = Some(version);
                }
                None if update::restarted() => logfile::line(&format!("Updated to {}", update::VERSION)),
                None => {}
            }
        }
        app.refresh_devices();
        // A save with a mic in it is from before Setup existed: that person is set up.
        app.saved.setup_done |= !app.saved.devices.input.is_empty();
        if first_start && app.installed && !app.autostart {
            // On unless turned off (Setup's last step and Settings have the switch): Discord
            // listens to the cable from sign-in, and a silent one sounds like a broken mic.
            app.autostart = autostart::set(true);
        }
        app.page = if app.saved.setup_done { Page::Home } else { Page::Setup };
        let d = &mut app.saved.devices;
        if d.input.is_empty() {
            d.input = audio::first_mic(&app.inputs).unwrap_or_default();
        }
        if d.output.is_empty() {
            // VB-Cable's playback side is what Discord then picks up as "CABLE Output".
            d.output = app.outputs.iter().find(|n| n.contains(os::cable::SEND)).cloned().unwrap_or_default();
        }
        app.restart();
        app
    }

    fn refresh_devices(&mut self) {
        self.inputs = audio::device_names(true);
        self.outputs = audio::device_names(false);
    }

    fn restart(&mut self) {
        self.engine = None; // stop old streams before opening the devices again
        self.plugins.resend = true; // the new chain starts without them
        self.engine_devices = self.saved.devices.clone();
        self.stats = Stats::default();
        self.last_beat = Instant::now();
        self.needs_restart = false;
        if os::TEST_FEED.is_some() {
            // Beside the real MicClean, a test build would take the mic and the cable from it.
            self.status = "Test build: no audio.".into();
            return;
        }
        let d = &self.saved.devices;
        let format = |input, name: &str| if name.is_empty() { None } else { audio::device_format(input, name) };
        let mic = format(true, &d.input);
        self.in_channels = mic.map_or(1, |f| f.1);
        let rate = |name: &str| format(false, name).map(|f| f.0);
        self.rates = [mic.map(|f| f.0), rate(&d.output), d.monitor.as_deref().and_then(rate)];
        if self.saved.devices.input.is_empty() {
            self.status = "Pick your mic in Setup.".into();
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
        if now < self.next_check || self.saved.devices.input.is_empty() {
            return;
        }
        self.next_check = now + Duration::from_secs(3);
        let stalled = self.engine.is_some() && now - self.last_beat > Duration::from_secs(2);
        let waiting_for_device = self.engine.as_ref().is_none_or(|e| !e.missing.is_empty());
        if waiting_for_device {
            self.refresh_devices();
        }
        // Like a device change, it waits for Smart setup, a recording or a learn to finish.
        let came_back = !self.busy()
            && self
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

    /// A device change restarts audio, which would cut Smart setup, a recording or a learn
    /// short without a word: it waits for them to finish. Without audio those can't finish
    /// (and their state is stale), so nothing waits then.
    fn busy(&self) -> bool {
        self.engine.is_some() && (self.calib.is_some() || self.record_progress.is_some() || self.meters.learning.is_some())
    }

    /// Loads and unloads plugins as the rack changes, hands the list to the audio thread and
    /// runs their GUI-thread chores. A finished plugin search comes in here too.
    /// Loads DeepFilterNet the first time it's picked, off the GUI thread, and hands it to the
    /// audio. ponytail: it stays loaded until MicClean quits, even after switching back.
    fn deep_tick(&mut self) {
        if let Some(h) = self.deep_load.take_if(|h| h.is_finished()) {
            match h.join().unwrap_or_else(|_| Err("it crashed while loading".into())) {
                Ok(d) => {
                    self.shared.lock().unwrap().deep = Some(d);
                    self.deep_ready = true;
                }
                Err(e) => {
                    logfile::line(&format!("DeepFilterNet didn't load: {e}"));
                    self.deep_error = Some(e);
                }
            }
        }
        let wanted = self.saved.params.denoiser == Denoiser::DeepFilterNet;
        if wanted && !self.deep_ready && self.deep_load.is_none() && self.deep_error.is_none() {
            self.deep_load = Some(std::thread::spawn(dsp::load_deep));
        }
        if let Some(h) = self.rendering.take_if(|h| h.is_finished())
            && let Ok(clip) = h.join()
        {
            self.start_playback(clip);
        }
    }

    fn plugin_tick(&mut self) {
        let wanted: Vec<u32> = self.saved.params.plugins().collect();
        if let Some(list) = self.plugins.sync(&wanted, &mut self.saved.plugins) {
            self.shared.lock().unwrap().new_plugins = Some(list);
        }
        // A plugin whose process crashed or hung stays off (and unloaded) until "Try again".
        for (key, why) in self.plugins.idle() {
            if let Some(e) = self.saved.plugins.iter_mut().find(|e| e.key == key) {
                e.crashed = true;
                self.plugin_crash = Some(format!("{} ({why})", e.info.name));
                self.dismissed.retain(|d| *d != "plugin_crash");
            }
            for s in self.saved.params.rack.iter_mut().flatten().filter(|s| s.fx == Fx::Plugin { key }) {
                s.on = false;
            }
        }
        if let Some(found) = self.search.as_ref().and_then(|s| s.lock().unwrap().found.take()) {
            logfile::line(&format!("Plugin search found {} effects", found.len()));
            self.search_done = Some((Instant::now(), found.len()));
            self.saved.plugin_list = found;
            self.search = None;
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
            // open, the (first) de-esser and compressor running.
            (want.bypass, want.gate) = (false, false);
            for k in [FxKind::Deess, FxKind::Comp] {
                if let Some(s) = want.first_mut(k) {
                    s.on = true;
                }
            }
        }
        if sh.params != want {
            sh.params = want;
            sh.params_ver += 1;
        }
        sh.viz = self.viz;
        sh.mute_sent = self.calib.is_some();
        self.latency_us = (sh.latency.capture_us.load(Ordering::Relaxed), sh.latency.output_us.load(Ordering::Relaxed));
        // Dropped here, on the GUI thread: the audio thread never frees anything.
        (sh.old_profiles, sh.discarded, sh.old_plugins) = (None, None, None);
        if let Some(l) = sh.learned.take() {
            match l.finish() {
                Err(why) => self.learn_msg = why.into(),
                Ok(profile) if l.kind == Kind::Keep => {
                    self.saved.profiles.keep = Some(profile);
                    self.learn_msg = "Learned your voice.".into();
                }
                Ok(profile) => {
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
            sh.new_profiles = Some(self.saved.profiles.clone());
        }
        // Smart setup's own recording while it runs, otherwise the test page's. Empty ones
        // were stopped.
        if let Some(clip) = sh.recorded.take().filter(|c| !c.is_empty()) {
            match &mut self.calib {
                Some(c) => c.audio = Some(clip),
                None => self.clip = Some(clip),
            }
        }
        self.record_progress = sh.record_progress;
        let peaks = std::mem::take(&mut sh.channel_peak);
        for (h, p) in self.chan_hold.iter_mut().zip(peaks) {
            *h = (20.0 * p.max(1e-6).log10()).max(*h - 0.5);
        }
        // Clipping before MicClean: the mic channel (or all of them, mixed) hit full scale.
        let raw = match self.saved.devices.input_channel {
            Some(c) => peaks.get(c as usize).copied().unwrap_or(0.0),
            None => peaks.iter().fold(0.0f32, |m, &p| m.max(p)),
        };
        let now = Instant::now();
        self.clips.retain(|t| now - *t < Duration::from_secs(10));
        if raw >= CLIP_LEVEL && self.engine.is_some() {
            self.clips.push(now);
            if self.clips.len() >= 3 {
                self.clipping_until = Some(now + Duration::from_secs(30));
            }
        }
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
        if processed {
            // What friends get with cleaning on, whatever Bypass is set to right now. On its
            // own thread: with DeepFilterNet that takes a couple of seconds.
            let (raw, prof) = (raw.clone(), self.saved.profiles.clone());
            let p = Params { bypass: false, ..self.saved.params.clone() };
            self.rendering = Some(std::thread::spawn(move || dsp::render(&raw, &p, &prof)));
        } else {
            self.start_playback(raw.clone());
        }
    }

    fn start_playback(&mut self, clip: Vec<f32>) {
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
        self.rendering = None;
        self.shared.lock().unwrap().mute_monitor = false;
    }

    fn learn(&mut self, kind: Kind, secs: f32) {
        self.learn_msg.clear();
        self.shared.lock().unwrap().learn = Some(dsp::Learner::new(kind, secs, self.saved.params.input_gain_db));
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
                update::State::Restarting => logfile::line("Closing so Velopack can install the update"),
                update::State::Failed(e) => logfile::line(&format!("Update failed: {e}")),
                _ => {}
            }
        }
    }

    /// Falling peak holds for the meters, and your levels while talking for Home's verdict and
    /// the problem banners.
    fn watch(&mut self) {
        let m = &self.meters;
        let running = self.engine.is_some();
        let (raw, out) = if running { (m.in_db - self.saved.params.input_gain_db, m.out_db) } else { (-120.0, -120.0) };
        self.raw_hold = raw.max(self.raw_hold - 0.5);
        self.out_hold = out.max(self.out_hold - 0.5);
        // By the voice detector, not the gate: a mic too quiet to open the gate is exactly the
        // one the "very quiet" banner is for.
        let talking = running && m.vad >= 0.5 && !self.saved.params.bypass;
        if running && m.vad >= 0.5 && m.gate_level_db > self.saved.params.gate_threshold_db {
            self.last_voice = Instant::now();
        }
        let (new, lim) = (m.frames != self.stats.frame, m.lim_db > 0.5);
        let s = &mut self.stats;
        s.frame = m.frames;
        if new && talking {
            // A plain average over the first second, then a moving one over about a second.
            let k = 1.0 / (s.voiced.min(99) + 1) as f32;
            s.voiced += 1;
            s.raw_peak += (raw - s.raw_peak) * k;
            s.out_peak += (out - s.out_peak) * k;
            s.lim_busy += (lim as u8 as f32 - s.lim_busy) * k;
        }
    }

    /// The most pressing thing to fix, if any: id (for dismissing), title, detail, button.
    fn problem(&self) -> Option<(&'static str, String, String, Option<Fix>)> {
        let (d, s) = (&self.saved.devices, &self.stats);
        let open = |id| !self.dismissed.contains(&id);
        if d.input.is_empty() {
            return Some(("no_input", "Pick your mic".into(), "Setup walks you through it.".into(), Some(Fix::Setup)));
        }
        if d.output.is_empty() {
            let desc = format!("Pick where the cleaned sound goes, like {} for Discord.", os::cable::NAME);
            return Some(("no_output", "Nothing is being sent".into(), desc, Some(Fix::Setup)));
        }
        if self.engine.is_none() {
            // Unplugged or renamed: Restart would only try the same device again.
            let gone = [(&d.input, &self.inputs), (&d.output, &self.outputs)].into_iter().find(|(n, list)| !n.is_empty() && !list.contains(n));
            if let Some((name, _)) = gone {
                let desc = format!("{name} isn't connected. Plug it back in, or pick another one in Setup.");
                return Some(("stopped", "A device is missing".into(), desc, Some(Fix::Setup)));
            }
            return Some(("stopped", "Audio isn't running".into(), self.status.clone(), Some(Fix::Restart)));
        }
        if self.saved.params.bypass {
            let desc = "Your mic is being sent without any cleaning.".into();
            return Some(("bypass", "Bypass is on".into(), desc, Some(Fix::BypassOff)));
        }
        if let Some(name) = &self.plugin_crash
            && open("plugin_crash")
        {
            let desc = "MicClean kept running without it. It stays off until you try it again in its box in Advanced.".into();
            return Some(("plugin_crash", format!("Plugin {name} stopped"), desc, None));
        }
        if audio::is_cable(&d.input) && open("mic_cable") {
            let desc = "MicClean would hear its own output and loop it back. Pick your real mic in Setup.".into();
            return Some(("mic_cable", format!("Your mic is set to {}", os::cable::NAME), desc, Some(Fix::Setup)));
        }
        if self.clipping_until.is_some_and(|t| Instant::now() < t) && open("clipping") {
            let desc = format!("It distorts before MicClean gets it, and nothing after can undo that. Turn down the gain knob on your mic or audio interface, or the mic level in {}.", os::SOUND_SETTINGS);
            return Some(("clipping", "Your mic is clipping".into(), desc, Some(Fix::SoundSettings)));
        }
        if let Some((name, why)) = self.engine.as_ref().and_then(|e| e.failed.first())
            && open("failed")
        {
            let desc = format!("{name} is plugged in but wouldn't start: {why}. Close other apps using it, then Restart.");
            return Some(("failed", "A device couldn't start".into(), desc, Some(Fix::Restart)));
        }
        if let Some(name) = self.engine.as_ref().and_then(|e| e.missing.first())
            && open("missing")
        {
            let what = if d.monitor.as_ref() == Some(name) { "Hear myself" } else { "Echo cancellation" };
            let desc = format!("{name} isn't connected, so {what} is paused. It starts again when the device is back.");
            return Some(("missing", format!("{what} is paused"), desc, None));
        }
        // On a Mac "Everything you hear" can't leave MicClean out yet, so it hears Hear myself too.
        let hears_monitor = d.monitor == d.echo_ref || (cfg!(not(windows)) && d.echo_ref.as_deref() == Some(audio::EVERYTHING));
        if d.monitor.is_some() && hears_monitor && open("echo_monitor") {
            let title = "Hear myself is cutting into your voice".into();
            if cfg!(not(windows)) {
                let desc = "Echo cancellation hears your own voice played back, and removes part of it. Turn off Hear myself (Output) while you talk.".into();
                return Some(("echo_monitor", title, desc, None));
            }
            let desc = "Echo cancellation hears your own voice played back on the same device, and removes part of it. \"Everything you hear\" leaves MicClean out.".into();
            return Some(("echo_monitor", title, desc, Some(Fix::EchoEverything)));
        }
        if let Some(why) = self.engine.as_ref().and_then(|e| e.shared_output.as_ref())
            && open("shared_output")
        {
            let desc = format!("MicClean couldn't take {} for itself: {why}. A screen share may send your voice twice.", d.output);
            return Some(("shared_output", "Not hidden from screen share".into(), desc, Some(Fix::SoundSettings)));
        }
        let names = [Some(&d.input), Some(&d.output), d.monitor.as_ref()];
        if let Some((name, r)) = names.iter().zip(self.rates).find_map(|(n, r)| Some((n.as_ref()?, r.filter(|&r| r != dsp::SR as u32)?)))
            && open("rate")
        {
            let desc = format!("{name} runs at {r} Hz, which adds delay. {}", os::SET_48K);
            return Some(("rate", "A device isn't at 48000 Hz".into(), desc, Some(Fix::SoundSettings)));
        }
        if s.voiced >= 300 && s.raw_peak < -42.0 && open("quiet") {
            let desc = "Turn up the gain knob on your mic or audio interface. It's cleaner than adding gain here.".into();
            return Some(("quiet", "Your mic is very quiet".into(), desc, None));
        }
        if s.voiced >= 300 && s.lim_busy > 0.15 && open("limiter") {
            let desc = "The clip guard is squashing them, which sounds like clipping. Turn Loudness down.".into();
            return Some(("limiter", "Your loud words are too loud".into(), desc, Some(Fix::Quieter)));
        }
        None
    }

    fn banner(&mut self, ui: &mut egui::Ui) {
        let Some((id, title, desc, fix)) = self.problem() else { return };
        // Home shows every problem; the other pages only the one that changes what's sent.
        if self.page != Page::Home && id != "bypass" {
            return;
        }
        let (mut act, mut close) = (false, false);
        ui::setting(ui, icon::WARNING, &title, &desc, |ui| {
            // Missing devices, stopped audio and Bypass can't be put off.
            if !matches!(id, "no_input" | "no_output" | "stopped" | "bypass") {
                close = ui::icon_button(ui, true, icon::CLOSE, "Hide").on_hover_text("Until MicClean restarts").clicked();
            }
            let label = match fix {
                Some(Fix::Setup) => "Open Setup",
                Some(Fix::Restart) => "Restart",
                Some(Fix::BypassOff) => "Turn it off",
                Some(Fix::SoundSettings) => os::SOUND_SETTINGS,
                Some(Fix::Quieter) => "3 dB quieter",
                Some(Fix::EchoEverything) => "Use Everything you hear",
                None => "",
            };
            if fix.is_some() {
                act = ui::accent_button(ui, true, label).clicked();
            }
        });
        if close {
            self.dismissed.push(id);
        }
        match fix.filter(|_| act) {
            Some(Fix::Setup) => self.page = Page::Setup,
            Some(Fix::Restart) => self.restart(),
            Some(Fix::BypassOff) => self.saved.params.bypass = false,
            Some(Fix::SoundSettings) => os::sound_settings(),
            Some(Fix::Quieter) => {
                let g = &mut self.saved.params.output_gain_db;
                *g = (*g - 3.0).max(-20.0);
                self.stats = Stats::default(); // measure again at the new level
            }
            Some(Fix::EchoEverything) => self.saved.devices.echo_ref = Some(audio::EVERYTHING.into()),
            None => {}
        }
    }

    /// An empty buffer ends a recording at once.
    fn stop_recording(&self) {
        self.shared.lock().unwrap().record = Some(Vec::new());
    }

    /// Samples the meters once per audio frame while Smart setup runs, then applies what it
    /// chose and keeps the old settings for Undo.
    fn smart_step(&mut self) {
        let Some(c) = &mut self.calib else { return };
        let t = c.start.elapsed().as_secs_f32();
        if t >= CALIB_END {
            let c = self.calib.take().expect("checked above");
            if c.audio.is_none() {
                // Not back in time (the audio restarted?): it mustn't turn up as a test clip.
                self.stop_recording();
            }
            match smart::analyse(&self.saved.params, &c.quiet, &c.talk) {
                Ok(mut o) => {
                    logfile::line(&format!("Smart setup {}", o.measured));
                    if let Some(audio) = &c.audio {
                        let before = o.params.output_gain_db;
                        o.params.output_gain_db = smart::output_gain(&o.params, &self.saved.profiles, audio);
                        if o.params.output_gain_db < before {
                            o.notes.push("Loudness turned down so your loudest words stay clear of the clip guard. Working on every word, it squashes them and sounds like clipping.".into());
                        }
                    }
                    let old = self.saved.params.clone();
                    for n in Node::smart_nodes(&self.saved.params) {
                        n.suggest(&mut self.saved.params, &o.params);
                    }
                    self.smart_report = smart::changes(&old, &self.saved.params);
                    if self.smart_report.is_empty() {
                        self.smart_report.push("Your settings already suit your voice. Nothing changed.".into());
                    }
                    self.smart_report.extend(o.notes);
                    // Suggest offers the full EQ, even where setup kept your own bands.
                    let mut chosen = o.params;
                    if let Some(b) = chosen.bands_mut() {
                        *b = o.full_eq;
                    }
                    self.saved.smart = Some(chosen);
                    self.smart_undo = Some(old);
                }
                Err(e) => self.smart_report = vec![e],
            }
            logfile::line(&format!("Smart setup: {}", self.smart_report.join(" / ")));
            return;
        }
        let (m, p) = (&self.meters, &self.saved.params);
        if m.frames == c.frame {
            return;
        }
        // What the first effect of a kind measures (-120: there's none).
        let fx_level = |k| p.rack.iter().position(|s| s.is_some_and(|s| s.fx.kind() == k)).map_or(-120.0, |i| m.fx[i].level_db);
        c.frame = m.frames;
        if t < CALIB_QUIET {
            if t > 0.5 {
                // Past the click of the button.
                c.quiet.push(smart::Quiet { level: m.gate_level_db, peak: m.in_db, bands: m.bands_db });
            }
        } else if m.vad >= 0.5 {
            // Talking, not the pauses between words.
            let (level, peak, agc, deess, comp) = (m.gate_level_db, m.in_db, m.agc_db, fx_level(FxKind::Deess), fx_level(FxKind::Comp));
            c.talk.push(smart::Take { level, peak, agc, deess, comp, pitch: m.pitch_hz, bands: m.bands_db });
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.plugin_tick();
        self.deep_tick();
        self.sync();
        self.viz = false; // set again by ui() if the window still draws
        self.watchdog();
        let now = Instant::now();
        self.agc_hist.push(self.meters.agc_db, now);
        self.pitch_hist.push((self.meters.pitch_hz.unwrap_or(f32::NAN), self.meters.gate_open), now);
        self.smart_step();
        self.watch();
        if self.installed && !self.saved.update_check_off && now >= self.next_update_check {
            self.next_update_check = now + Duration::from_secs(24 * 3600);
            self.updater.check();
        }
        let st = self.updater.state();
        self.log_changes(&st, now);
        if let update::State::Available(r) = &st
            && self.update_dismissed.as_deref() != Some(&r.version)
        {
            if self.saved.auto_update {
                // Never in the middle of a sentence, nor while Smart setup, a recording, a learn
                // or a playback is going.
                let quiet = now - self.last_voice > Duration::from_secs(10);
                let busy = self.calib.is_some() || self.record_progress.is_some() || self.meters.learning.is_some() || self.playback.is_some();
                if self.installed && quiet && !busy {
                    self.start_update(r.clone());
                }
            } else if !self.saved.update_popup_off {
                self.update_prompt = true;
            }
        }
        if st == update::State::Restarting && !self.quitting {
            // The updater waits for this copy to close; closing normally saves your settings first.
            self.quitting = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if self.saved.devices != self.engine_devices && !self.busy() {
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
                self.hidden = true;
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
            self.hidden = false;
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
            self.hidden = true;
        }
        // A settings file dropped on the window (a backup, or one a friend sent) loads.
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf())) {
            self.load_settings_file(&path);
            self.page = Page::Settings;
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
        let narrow = ui.available_width() < 720.0;
        let compact = self.saved.nav_compact || narrow;
        // First, so it spans the whole window's width under the nav rail too.
        self.search_strip(ui);
        egui::Panel::left("nav")
            .exact_size(if compact { 56.0 } else { 240.0 })
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 4, right: 4, top: 10, bottom: 8 }))
            .show(ui, |ui| self.nav(ui, compact, narrow));
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
                    self.banner(ui);
                    match self.page {
                        Page::Home => self.home(ui),
                        Page::Setup => self.setup(ui),
                        Page::Advanced => self.advanced(ui),
                        Page::Settings => self.settings(ui),
                    }
                    ui.add_space(24.0);
                });
            });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.plugins.store(&mut self.saved.plugins);
        // Plugins nothing uses any more (not the rack, a preset, Smart setup or an Undo).
        let s = &self.saved;
        let used: Vec<u32> = [Some(&s.params), s.smart.as_ref(), self.reset_undo.as_ref(), self.smart_undo.as_ref()]
            .into_iter()
            .flatten()
            .chain(s.presets.iter().map(|p| &p.params))
            .flat_map(|p| p.plugins())
            .collect();
        self.saved.plugins.retain(|e| used.contains(&e.key));
        eframe::set_value(storage, eframe::APP_KEY, &self.saved);
        // eframe saves every 30 s; the safe copy is only rewritten when something changed.
        if let Ok(text) = ron::to_string(&self.saved)
            && text != self.kept
        {
            match backup::keep(&text) {
                Ok(()) => self.kept = text,
                Err(e) => logfile::line(&format!("Couldn't save the settings' safe copy: {e}")),
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Audio first: nothing may still be processing a plugin when it unloads.
        self.engine = None;
        self.plugins.shutdown();
    }
}

/// One format's plugins in "Add effect", filtered by `filter`; `pick` gets the list index.
fn plugin_menu(ui: &mut egui::Ui, list: &[plugins::Info], f: plugins::Format, filter: &mut String, pick: &mut Option<usize>) {
    ui.set_min_width(320.0);
    if !list.iter().any(|p| p.format == f) {
        ui.label(if list.is_empty() { "Search for plugins first" } else { "None found" });
        return;
    }
    ui.add(egui::TextEdit::singleline(filter).hint_text("Find a plugin"));
    let q = filter.to_lowercase();
    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        for (i, p) in list.iter().enumerate().filter(|(_, p)| p.format == f) {
            if !q.is_empty() && !format!("{} {}", p.name, p.vendor).to_lowercase().contains(&q) {
                continue;
            }
            if ui.button(format!("{}    {}", p.name, p.vendor)).clicked() {
                *pick = Some(i);
                ui.close();
            }
        }
    });
}

/// A plugin search's progress: files checked of those found.
fn search_bar(ui: &mut egui::Ui, (done, of): (usize, usize)) {
    let (frac, text) = match of {
        0 => (0.0, "Looking for plugin files…".to_string()),
        of => (done as f32 / of as f32, format!("Checked {done} of {of} plugin files")),
    };
    ui.add(egui::ProgressBar::new(frac).desired_width(280.0).text(text));
}

fn search_button(ui: &mut egui::Ui, again: bool) -> bool {
    ui.button(if again { "Search again" } else { "Search for plugins" })
        .on_hover_text("Finds the VST3 and CLAP effects in the usual folders and the ones added in Settings. Each file is tried in its own copy of MicClean, so a broken plugin can't take this one down")
        .clicked()
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
    /// `narrow`: the window is too narrow for labels, so the fold button has nothing to do.
    fn nav(&mut self, ui: &mut egui::Ui, compact: bool, narrow: bool) {
        let fold = if compact { "Show labels" } else { "MicClean" };
        if !narrow && ui::nav_item(ui, icon::MENU, fold, false, compact).clicked() {
            self.saved.nav_compact = !compact;
        }
        ui.add_space(4.0);
        // Going anywhere counts as having seen Setup: it won't open on it again.
        for (page, ic, label) in NAV {
            if ui::nav_item(ui, ic, label, self.page == page, compact).clicked() {
                self.page = page;
                self.saved.setup_done = true;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if ui::nav_item(ui, icon::SETTINGS, "Settings", self.page == Page::Settings, compact).clicked() {
                self.page = Page::Settings;
                self.saved.setup_done = true;
            }
        });
    }

    fn live_meters(&self) -> Meters {
        if self.engine.is_some() {
            self.meters.clone()
        } else {
            let quiet = -120.0;
            Meters { in_db: quiet, out_db: quiet, gate_level_db: quiet, reason: "no audio", ..Default::default() }
        }
    }

    /// The everyday view: anything to fix, what friends hear, the main switches, Smart setup
    /// and a before/after recording. Everything else is in Advanced.
    fn home(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let running = self.engine.is_some();
        let status = match &self.glitches {
            (0, _) => self.status.clone(),
            (n, dev) => format!("{} · {n} glitch{} (last on {dev})", self.status, if *n == 1 { "" } else { "es" }),
        };
        let d = &self.saved.devices;
        let or = |s: &str, none: &str| if s.is_empty() { none.to_string() } else { s.to_string() };
        let route = format!("{} → {}", or(&d.input, "no mic"), or(&d.output, "nowhere"));
        let mut restart = false;
        ui::setting(ui, icon::MIC, if running { "On" } else { "Not running" }, &route, |ui| {
            restart = ui::icon_button(ui, true, icon::REFRESH, "Restart").on_hover_text(&status).clicked();
        });
        if restart {
            self.restart();
        }

        let (m, s) = (self.live_meters(), &self.stats);
        // Typical peaks while talking: the zone below is where the loud ones should land.
        let verdict = match () {
            _ if !running => "Not running",
            _ if s.voiced < 100 => "Talk to see your level",
            _ if s.out_peak < -26.0 => "Quiet. Turn Loudness up",
            _ if s.out_peak > -8.0 => "Loud. Turn Loudness down",
            _ => "A good level",
        };
        ui::card(ui, |ui| {
            ui::header(ui, icon::SPEAKERS, "What your friends hear", verdict, |ui| {
                if m.gate_open {
                    ui::pill(ui, "Sending", p.good, p.on_accent);
                } else {
                    ui::pill(ui, "Muted while you're quiet", p.control_stroke, p.text2);
                }
            });
            graphs::zone_meter(ui, ui.available_width(), self.out_hold, -60.0, 0.0, (-20.0, -6.0), p.accent);
            ui.add_space(8.0);
        });

        let (pr, d, outputs) = (&mut self.saved.params, &mut self.saved.devices, &self.outputs);
        ui::group(ui, icon::VOLUME, "Your sound", "The main switches. Every setting is in Advanced", None, |ui| {
            ui::row(ui, Node::Noise.title(), |ui| {
                // Normal covers Smart setup's gentler strengths for quiet rooms.
                let mut level = if !pr.denoise { 0 } else if pr.denoise_strength < 0.95 { 1 } else { 2 };
                let before = level;
                for (v, label) in [(2, "Strong"), (1, "Normal"), (0, "Off")] {
                    ui.selectable_value(&mut level, v, label);
                }
                if level != before {
                    pr.denoise = level > 0;
                    if level > 0 {
                        pr.denoise_strength = if level == 2 { 1.0 } else { 0.8 };
                    }
                }
            });
            ui::row(ui, Node::Gate.title(), |ui| {
                ui::toggle(ui, &mut pr.gate);
            });
            ui::row(ui, "Loudness", |ui| {
                slider(ui, &mut pr.output_gain_db, -20.0..=30.0, " dB");
            });
            ui::row(ui, "Hear myself", |ui| {
                let mut on = d.monitor.is_some();
                if ui::toggle(ui, &mut on).changed() {
                    d.monitor = if on { listen_device(outputs) } else { None };
                }
                if let Some(mon) = &d.monitor {
                    ui.add(egui::Label::new(RichText::new(mon).small().color(p.text2)).truncate());
                }
            });
        });
        self.smart_card(ui);
        self.compare_card(ui);
    }

    /// Record 10 seconds of the raw mic, then play it back as it was and cleaned up.
    fn compare_card(&mut self, ui: &mut egui::Ui) {
        let recording = self.record_progress.is_some() && self.calib.is_none();
        // Not while Smart setup listens: the playback would end up in its measurement.
        let has_clip = self.clip.is_some() && !recording && self.calib.is_none();
        let can_record = self.record_progress.is_none() && self.engine.is_some();
        let (mut record, mut original, mut processed, mut stop) = (false, false, false, false);
        let desc = "Record 10 seconds, then play it as it was and cleaned up, on Hear myself's device or your speakers";
        ui::card(ui, |ui| {
            ui::header(ui, icon::RECORD, "Hear the difference", desc, |ui| {
                let rendering = self.rendering.is_some();
                stop = ui::icon_button(ui, self.playback.is_some() || rendering, icon::STOP, "Stop").clicked();
                let label = if rendering { "Cleaning..." } else { "Cleaned" };
                processed = ui::icon_button(ui, has_clip && !rendering, icon::PLAY, label).clicked();
                original = ui::icon_button(ui, has_clip, icon::PLAY, "Original").clicked();
                record = ui::accent_button(ui, can_record, "Record").clicked();
            });
            if let Some(prog) = self.record_progress.filter(|_| recording) {
                ui.add(egui::ProgressBar::new(prog).desired_height(4.0));
            }
        });
        if record {
            self.shared.lock().unwrap().record = Some(vec![0.0; RECORD_SECS * dsp::SR as usize]);
        }
        if original {
            self.play(false);
        }
        if processed {
            self.play(true);
        }
        if stop {
            self.stop_playback();
        }
    }

    /// Mic, where the sound goes, what you listen on, then Smart setup. Opens on first run;
    /// any step can be changed again later.
    fn setup(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let running = self.engine.is_some();
        let (holds, raw_hold, n) = (self.chan_hold, self.raw_hold, self.in_channels);
        let (voiced, raw_peak) = (self.stats.voiced, self.stats.raw_peak);
        let out_rate = self.rates[1].filter(|&r| r != dsp::SR as u32);
        let (d, inputs, outputs) = (&mut self.saved.devices, &self.inputs, &self.outputs);
        let mut refresh = false;

        step(ui, 1, "Your mic", running && !d.input.is_empty());
        ui::card(ui, |ui| {
            ui::header(ui, icon::MIC, "Mic", "Pick it, then talk for a few seconds to check it", |ui| {
                let before = d.input.clone();
                pick(ui, "setup_mic", &mut d.input, inputs);
                if d.input != before {
                    d.input_channel = None;
                }
                refresh |= ui.button("Refresh list").clicked();
            });
            let n = usize::from(n).min(audio::MAX_CH);
            if n > 1 {
                // Interfaces show up as one device for all their inputs: the one your voice is
                // on is far louder than the others.
                for (c, &db) in holds[..n].iter().enumerate() {
                    graphs::meter_row(ui, &format!("Input {}", c + 1), db, -60.0, 0.0, None, p.accent, &format!("{db:.0} dB"));
                }
                let mut order: Vec<usize> = (0..n).collect();
                order.sort_by(|&a, &b| holds[b].total_cmp(&holds[a]));
                let top = order[0];
                if holds[top] > -50.0 && holds[top] - holds[order[1]] >= 15.0 && d.input_channel != Some(top as u16) {
                    ui::row(ui, &format!("Your voice is on Input {}", top + 1), |ui| {
                        if ui::accent_button(ui, true, format!("Use Input {}", top + 1)).clicked() {
                            d.input_channel = Some(top as u16);
                        }
                    });
                }
                ui::row(ui, "Use", |ui| channel_pick(ui, &mut d.input_channel, n as u16));
            }
            ui::row(ui, "Level", |ui| graphs::zone_meter(ui, 260.0, raw_hold, -60.0, 0.0, (-24.0, -9.0), p.accent));
            let level = match () {
                _ if !running => "Not running yet.",
                _ if voiced < 100 => "Talk normally for a few seconds.",
                _ if raw_peak < -42.0 => "Very quiet. Turn up the gain knob on your mic or audio interface: that's cleaner than adding gain here.",
                _ if raw_peak > -8.0 => "Very loud. Turn the gain knob on your mic or interface down a little, or it may clip.",
                _ => "Your level is good.",
            };
            ui.label(RichText::new(level).small().color(p.text2));
            ui.add_space(4.0);
        });

        step(ui, 2, "Where it goes", !d.output.is_empty());
        use os::cable;
        let found = outputs.iter().find(|n| n.contains(cable::SEND)).cloned();
        ui::card(ui, |ui| {
            let desc = if cable::SEND == cable::MIC {
                format!("For Discord and other apps, {}. They then use it as their mic too", cable::MIC)
            } else {
                format!("For Discord and other apps, {}'s {}. They then use {} as their mic", cable::NAME, cable::SEND, cable::MIC)
            };
            ui::header(ui, icon::SPEAKERS, "Send to", &desc, |ui| {
                pick(ui, "setup_out", &mut d.output, outputs);
            });
            match &found {
                Some(c) if d.output != *c => ui::row(ui, cable::FOUND, |ui| {
                    if ui::accent_button(ui, true, format!("Use {}", cable::SEND)).clicked() {
                        d.output.clone_from(c);
                    }
                }),
                None => ui::row(ui, cable::MISSING, |ui| {
                    refresh |= ui.button("Refresh list").clicked();
                    if ui.button(cable::GET).clicked() {
                        cable::get();
                    }
                }),
                _ => {}
            }
            if let Some(note) = cable::note() {
                ui.label(RichText::new(note).small().color(p.text2));
            }
            refresh |= cable::just_installed();
            if let Some(r) = out_rate {
                ui::row(ui, &format!("Runs at {r} Hz, set it to 48000"), |ui| {
                    if ui.button(os::SOUND_SETTINGS).clicked() {
                        os::sound_settings();
                    }
                });
            }
            // Open: without step 1 there, MicClean does nothing for Discord.
            egui::CollapsingHeader::new("Then in Discord").default_open(true).show(ui, |ui| {
                for line in cable::STEPS {
                    ui.label(RichText::new(*line).color(p.text2));
                }
            });
        });

        step(ui, 3, "What you listen on", true);
        let choices = echo_choices(outputs);
        ui::card(ui, |ui| {
            let desc = "Speakers and open-back headphones leak into your mic, so friends hear themselves. MicClean can cancel that";
            ui::header(ui, icon::HEADPHONE, "Headphones or speakers", desc, |_| {});
            let mut open = d.echo_ref.is_some();
            let before = open;
            ui.horizontal(|ui| {
                ui.radio_value(&mut open, false, "Closed-back headphones");
                ui.radio_value(&mut open, true, "Speakers or open-back headphones");
            });
            if open != before {
                d.echo_ref = open.then(|| audio::EVERYTHING.to_string());
            }
            if open {
                ui::row(ui, "Cancel", |ui| pick_opt(ui, "setup_echo", &mut d.echo_ref, &choices, "Off"));
            }
            ui.add_space(4.0);
        });
        if refresh {
            self.refresh_devices();
        }

        step(ui, 4, "Tune it to your voice", self.saved.smart.is_some());
        self.smart_card(ui);
        step(ui, 5, "Keep it running", self.autostart);
        self.autostart_row(ui);
        ui.add_space(14.0);
        if ui::accent_button(ui, true, "Done").clicked() {
            self.saved.setup_done = true;
            self.page = Page::Home;
        }
    }

    /// Presets, then every step of the signal chain with its settings opening in place, then
    /// levels, Bypass and learned sounds.
    fn advanced(&mut self, ui: &mut egui::Ui) {
        let m = self.live_meters();
        self.presets_card(ui);
        self.chain_rack(ui, &m);
        self.overview(ui, &m);
        self.teach(ui);
    }

    /// The signal chain top to bottom in the order the sound goes through it: your mic, three
    /// groups of steps, what gets sent. Each row shows what its step does to your voice right
    /// now; clicking one opens its settings right under it.
    fn chain_rack(&mut self, ui: &mut egui::Ui, m: &Meters) {
        let p = ui::pal(ui);
        let (cap, out) = (self.latency_us.0 as f32 / 1000.0, self.latency_us.1 as f32 / 1000.0);
        let rnn = 10.0; // RNNoise's one frame, there even with noise removal off
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
        let rack: Vec<Node> = (0..dsp::RACK).filter(|&i| self.saved.params.rack[i].is_some()).map(Node::Fx).collect();
        let groups: [(&str, &[Node]); 5] = [
            ("", &[Node::Input]),
            ("Clean up", &[Node::Echo, Node::Noise, Node::Gate, Node::Level]),
            ("Effects", &rack),
            ("Protect", &[Node::Limiter]),
            ("", &[Node::Output]),
        ];
        // Moving or removing an effect waits for the end of the list, which is drawn by position.
        let mut edit = None;
        for (group, nodes) in groups {
            if !group.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(group).small().color(p.text2));
            }
            if group == "Effects"
                && let Some(d) = self.drag_fx
            {
                self.drag_rack(ui, m, d);
                self.add_effect(ui);
                continue;
            }
            for &n in nodes {
                let (bar, value) = self.row_state(n, m);
                let open = self.node == Some(n);
                // Echo cancellation is on when it has something to cancel.
                let mut echo_on = self.saved.devices.echo_ref.is_some();
                let title = n.name(&self.saved.params, &self.saved.plugins);
                let on = if n == Node::Echo { Some(&mut echo_on) } else { n.switch(&mut self.saved.params) };
                let icon = match n {
                    Node::Input => icon::MIC,
                    Node::Output => icon::SPEAKERS,
                    _ => icon::VOLUME,
                };
                let top = ui.cursor().top();
                let row = ui::slim_card(ui, |ui| graphs::rack_row(ui, on, icon, &title, &bar, &value, open));
                if row.clicked() {
                    self.node = if open { None } else { Some(n) };
                }
                if let Node::Fx(i) = n {
                    if !open {
                        self.row_pitch = ui.cursor().top() - top;
                    }
                    if row.drag_started() {
                        let (press, time) = ui.input(|i| (i.pointer.press_origin(), i.time));
                        self.drag_fx = Some(Drag { from: i, grab: press.map_or(0.0, |p| p.y - top), started: time.to_bits() });
                    }
                }
                if echo_on != self.saved.devices.echo_ref.is_some() {
                    self.saved.devices.echo_ref = echo_on.then(|| audio::EVERYTHING.to_string());
                }
                if self.node == Some(n) {
                    edit = self.expanded(ui, n, m).or(edit);
                }
            }
            if group == "Effects" {
                self.add_effect(ui);
            }
        }
        if let Some((i, by)) = edit {
            let pr = &mut self.saved.params;
            self.node = match by {
                0 => {
                    pr.remove_fx(i);
                    None
                }
                by => Some(Node::Fx(pr.move_fx(i, by))),
            };
        }
    }

    /// The effects while one is dragged: it follows the pointer, see-through, over the others,
    /// which slide out of its way; on release it lands in the gap. Open settings fold away
    /// meanwhile (and come back after).
    fn drag_rack(&mut self, ui: &mut egui::Ui, m: &Meters, d: Drag) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        let (pitch, gap) = (self.row_pitch, ui.spacing().item_spacing.y);
        let n = self.saved.params.rack.iter().flatten().count();
        let (block, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), n as f32 * pitch - gap), egui::Sense::hover());
        let size = egui::vec2(block.width(), pitch - gap);
        // The lifted one's top, kept within half a row of the list.
        let y = ui.ctx().pointer_latest_pos().map_or(block.top(), |p| p.y - d.grab);
        let top = y.clamp(block.top() - pitch / 2.0, block.bottom() - size.y + pitch / 2.0);
        let to = ((top - block.top()) / pitch).round().clamp(0.0, n.saturating_sub(1) as f32) as usize;
        for (k, i) in (0..n).filter(|&i| i != d.from).enumerate() {
            let place = if k < to { k } else { k + 1 };
            let id = egui::Id::unique(("fx_slide", self.saved.params.rack[i].map_or(0, |s| s.id), d.started));
            let at = ui.ctx().animate_value_with_time(id, place as f32, 0.12);
            self.fx_row(ui, m, i, Rect::from_min_size(Pos2::new(block.left(), block.top() + at * pitch), size), false);
        }
        self.fx_row(ui, m, d.from, Rect::from_min_size(Pos2::new(block.left(), top), size), true);
        if ui.input(|i| !i.pointer.any_down()) {
            self.drag_fx = None;
            let pr = &mut self.saved.params;
            // The open one stays open, wherever it ends up.
            let open = match self.node {
                Some(Node::Fx(j)) => pr.rack[j].map(|s| s.id),
                _ => None,
            };
            pr.move_fx_to(d.from, to);
            if let Some(id) = open {
                self.node = pr.rack.iter().position(|s| s.is_some_and(|s| s.id == id)).map(Node::Fx);
            }
        }
    }

    /// One effect's row at `rect`, while one is dragged. `lifted` is the dragged one: on a layer
    /// above the rest, see-through.
    fn fx_row(&self, ui: &mut egui::Ui, m: &Meters, i: usize, rect: Rect, lifted: bool) {
        let n = Node::Fx(i);
        let (bar, value) = self.row_state(n, m);
        let title = n.name(&self.saved.params, &self.saved.plugins);
        let mut on = self.saved.params.rack[i].is_some_and(|s| s.on);
        let mut b = egui::UiBuilder::new().max_rect(rect);
        if lifted {
            b = b.layer_id(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("fx_lifted")));
        }
        // A child that takes no space: the list's own space was set aside in one piece.
        let mut row = ui.new_child(b);
        if lifted {
            row.multiply_opacity(0.75);
        }
        ui::slim_card(&mut row, |ui| graphs::rack_row(ui, Some(&mut on), icon::VOLUME, &title, &bar, &value, false));
    }

    /// The plugin search along the bottom of the window, on every page. The tab on its right
    /// folds it down to just the tab. It stays a few seconds after the search ends.
    fn search_strip(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let (frac, text, short) = match (self.search_progress(), self.search_done) {
            (Some((done, of)), _) => {
                let f = if of == 0 { 0.0 } else { done as f32 / of as f32 };
                let text = if of == 0 { "Looking for plugin files…".to_string() } else { format!("Searching for plugins: checked {done} of {of} files") };
                (f, text, format!("{:.0}%", f * 100.0))
            }
            (None, Some((t, n))) if t.elapsed() < Duration::from_secs(6) => (1.0, format!("Plugin search done: {n} effects found"), "done".to_string()),
            _ => return,
        };
        let bottom = ui.ctx().content_rect().bottom();
        let bar_top = if self.search_folded {
            bottom
        } else {
            let frame = egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.card_stroke)).inner_margin(egui::Margin::symmetric(12, 7));
            egui::Panel::bottom("plugin_search")
                .resizable(false)
                .show_separator_line(false)
                .frame(frame)
                .show(ui, |ui| ui.add(egui::ProgressBar::new(frac).desired_width(ui.available_width()).text(text)))
                .response
                .rect
                .top()
        };
        // The tab, on the right, sitting on the bar (or on the window's edge when folded).
        let tab = egui::Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.card_stroke))
            .corner_radius(egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 })
            .inner_margin(egui::Margin::symmetric(10, 3));
        let folded = self.search_folded;
        let clicked = egui::Area::new(egui::Id::unique("plugin_search_tab"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, bar_top - bottom))
            .show(ui.ctx(), |ui| {
                tab.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(if folded { format!("Plugins {short}") } else { "Plugins".into() }).small());
                        let arrow = if folded { icon::CHEVRON_UP } else { icon::CHEVRON_DOWN };
                        ui.label(RichText::new(arrow).font(ui::icon_font(10.0)));
                    });
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(if folded { "Show the plugin search" } else { "Fold it away. The search carries on" })
                .clicked()
            })
            .inner;
        self.search_folded ^= clicked;
    }
    fn start_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(plugins::search(self.saved.plugin_dirs.clone()));
        }
    }

    /// Files checked and found, while a plugin search runs.
    fn search_progress(&self) -> Option<(usize, usize)> {
        self.search.as_ref().map(|s| s.lock().unwrap().progress)
    }

    /// The "Add effect" menu under the rack's effects: MicClean's own that aren't in the rack
    /// yet, then the plugins the last search found, VST3 and CLAP apart.
    fn add_effect(&mut self, ui: &mut egui::Ui) {
        let pr = &self.saved.params;
        let full = pr.rack.iter().all(Option::is_some);
        let empty = pr.rack.iter().all(Option::is_none);
        let missing: Vec<FxKind> = FxKind::ALL.into_iter().filter(|&k| pr.first(k).is_none()).collect();
        let (mut added, mut plugin, mut search) = (None, None, false);
        let progress = self.search_progress();
        let (list, filter) = (&self.saved.plugin_list, &mut self.plugin_filter);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!full, |ui| {
                ui.menu_button(ui::icon_text(ui, icon::ADD, "Add effect"), |ui| {
                    for k in &missing {
                        if ui.button(k.title()).clicked() {
                            added = Some(k.new());
                            ui.close();
                        }
                    }
                    if !plugins::SUPPORTED {
                        return;
                    }
                    if !missing.is_empty() {
                        ui.separator();
                    }
                    for f in [plugins::Format::Vst3, plugins::Format::Clap] {
                        ui.menu_button(format!("{} plugins", f.name()), |ui| plugin_menu(ui, list, f, filter, &mut plugin));
                    }
                    ui.separator();
                    match progress {
                        Some(pr) => search_bar(ui, pr),
                        None => search = search_button(ui, !list.is_empty()),
                    }
                })
                .response
                .on_disabled_hover_text(format!("The rack holds {} effects", dsp::RACK));
            });
            if empty {
                ui.label(RichText::new("No effects: your voice goes straight from cleaning up to the clip guard").small());
            }
        });
        if search {
            self.start_search();
        }
        if let Some(i) = plugin {
            // A key nothing has used, not even a preset whose plugin entry is gone.
            let s = &self.saved;
            let used = s.plugins.iter().map(|e| e.key).chain(s.presets.iter().flat_map(|p| p.params.plugins())).chain(s.params.plugins());
            let key = used.max().unwrap_or(0) + 1;
            let info = self.saved.plugin_list[i].clone();
            self.saved.plugins.push(plugins::Entry { key, info, state: String::new(), crashed: false });
            added = Some(Fx::Plugin { key });
        }
        let pr = &mut self.saved.params;
        if let Some(fx) = added
            && pr.add_fx(fx)
        {
            // Opens it, at the end of the rack.
            self.node = pr.rack.iter().rposition(Option::is_some).map(Node::Fx);
        }
    }

    /// Settings' Plugins card: the search and its progress, and the folders it looks in.
    fn plugin_settings(&mut self, ui: &mut egui::Ui) {
        if !plugins::SUPPORTED {
            return;
        }
        let p = ui::pal(ui);
        let progress = self.search_progress();
        let list = &self.saved.plugin_list;
        let count = |f| list.iter().filter(|i| i.format == f).count();
        let desc = match progress {
            Some(_) => "Searching. Each file is tried in its own copy of MicClean, so a broken plugin can't take this one down".to_string(),
            None if list.is_empty() => "VST3 and CLAP effects for the rack. Search to find the ones on this PC".to_string(),
            None => format!("{} effects found: {} VST3, {} CLAP", list.len(), count(plugins::Format::Vst3), count(plugins::Format::Clap)),
        };
        let (mut search, mut add, mut remove) = (false, false, None);
        ui::card(ui, |ui| {
            ui::header(ui, icon::EQ, "Plugins", &desc, |ui| {
                if progress.is_none() {
                    search = search_button(ui, !list.is_empty());
                }
            });
            if let Some(pr) = progress {
                search_bar(ui, pr);
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Folders it searches").small().color(p.text2));
            let folder = |ui: &mut egui::Ui, d: &std::path::Path, note: &str| {
                let text = format!("{}{note}", d.display());
                ui.add(egui::Label::new(RichText::new(text).small()).truncate());
            };
            for d in plugins::default_dirs() {
                ui.horizontal(|ui| folder(ui, &d, if d.is_dir() { "" } else { "  (not on this PC)" }));
            }
            for (i, d) in self.saved.plugin_dirs.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui::icon_button(ui, true, icon::DELETE, "Remove").clicked() {
                            remove = Some(i);
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| folder(ui, d, ""));
                    });
                });
            }
            ui.add_space(4.0);
            add = ui::icon_button(ui, true, icon::ADD, "Add folder").on_hover_text("Plugins kept somewhere else. VST3 and CLAP files in it and its subfolders are found").clicked();
        });
        if let Some(i) = remove {
            self.saved.plugin_dirs.remove(i);
        }
        if add && let Some(d) = plugins::pick_folder(self.hwnd) {
            if !self.saved.plugin_dirs.contains(&d) && !plugins::default_dirs().contains(&d) {
                self.saved.plugin_dirs.push(d);
            }
            search = true; // what's in it shows up in "Add effect"
        }
        if search {
            self.start_search();
        }
    }
    /// A chain row's bar and value: what that step is doing right now.
    fn row_state(&self, n: Node, m: &Meters) -> (graphs::RowBar, String) {
        use graphs::RowBar::{Change, Curve, Empty, Level};
        let (pr, d) = (&self.saved.params, &self.saved.devices);
        let device = match n {
            Node::Input => Some(&d.input),
            Node::Output => Some(&d.output),
            _ => None,
        };
        // Read through the same switch the row toggles.
        let on = if n == Node::Echo { d.echo_ref.is_some() } else { n.switch(&mut pr.clone()).is_none_or(|on| *on) };
        let cut = |v: f32| format!("-{v:.1} dB");
        match () {
            _ if device.is_some_and(|d| d.is_empty()) => return (Empty, "pick one".into()),
            _ if self.engine.is_none() => return (Empty, "-".into()),
            // Echo cancellation and the clip guard still work during Bypass.
            _ if pr.bypass && device.is_none() && !matches!(n, Node::Echo | Node::Limiter) => return (Empty, "bypassed".into()),
            _ if !on => return (Empty, "off".into()),
            _ => {}
        }
        match n {
            Node::Input => (Level(m.in_db), format!("{:.0} dB", m.in_db)),
            Node::Echo => (Change(-m.echo_erle_db.unwrap_or(0.0)), m.echo_erle_db.map_or("listening".into(), |e| cut(e))),
            Node::Noise => (Change(-m.denoise_db), cut(m.denoise_db)),
            Node::Gate => (Change(m.gate_gain_db), if m.gate_open { "open" } else { "muted" }.into()),
            Node::Level => (Change(m.agc_db), format!("{:+.1} dB", m.agc_db)),
            Node::Fx(i) => {
                let fm = m.fx[i];
                match pr.rack[i].map(|s| s.fx) {
                    Some(Fx::Eq { bands }) => {
                        let freqs: Vec<f32> = (0..32).map(|i| 60.0 * (16_000f32 / 60.0).powf(i as f32 / 31.0)).collect();
                        let n = bands.iter().flatten().filter(|b| b.on).count();
                        (Curve(dsp::eq_curve(&bands, &freqs)), format!("{n} band{}", if n == 1 { "" } else { "s" }))
                    }
                    Some(Fx::Deess { .. } | Fx::Comp(_)) => (Change(fm.change_db), cut(-fm.change_db)),
                    Some(Fx::Warmth { amount }) => (Level(fm.level_db), format!("{:.0}%", amount * 100.0)),
                    Some(Fx::Air { amount }) => (Change(fm.change_db), format!("{:.0}%", amount * 100.0)),
                    Some(Fx::Plugin { key }) => match self.plugins.get(key) {
                        Some(Ok(_)) => (Change(fm.change_db), format!("{:+.1} dB", fm.change_db)),
                        Some(Err(_)) => (Empty, "didn't load".into()),
                        None if self.saved.plugins.iter().any(|e| e.key == key && e.crashed) => (Empty, "kept off".into()),
                        None if !plugins::SUPPORTED => (Empty, "Windows only".into()),
                        None => (Empty, "loading".into()),
                    },
                    None => (Empty, String::new()),
                }
            }
            Node::Limiter => (Change(-m.lim_db), if m.lim_db > 0.1 { cut(m.lim_db) } else { "idle".into() }),
            Node::Output => match self.rates[1] {
                Some(r) if r != dsp::SR as u32 => (Level(m.out_db), "set 48 kHz".into()),
                _ => (Level(m.out_db), format!("{:.0} dB", m.out_db)),
            },
        }
    }

    /// An open row's settings, right under it: Suggest and Reset (and for a rack effect, move
    /// and remove), then the step's panel. Returns a rack effect to move by one place (-1, 1)
    /// or remove (0), for the caller to do once the rack is drawn.
    fn expanded(&mut self, ui: &mut egui::Ui, n: Node, m: &Meters) -> Option<(usize, isize)> {
        let mut edit = None;
        let id = match n {
            Node::Fx(i) => format!("fx{i}"),
            n => n.title().into(),
        };
        ui.indent(id, |ui| {
            let (mut reset, mut suggest) = (false, false);
            {
                let (params, smart) = (&self.saved.params, self.saved.smart.as_ref().filter(|_| n.smart(&self.saved.params)));
                ui.horizontal(|ui| {
                    if let Node::Fx(i) = n {
                        let last = params.rack.get(i + 1).is_none_or(Option::is_none);
                        if ui::icon_button(ui, i > 0, icon::CHEVRON_UP, "Earlier").on_hover_text("Moves it one step earlier in the chain").clicked() {
                            edit = Some((i, -1));
                        }
                        if ui::icon_button(ui, !last, icon::CHEVRON_DOWN, "Later").on_hover_text("Moves it one step later in the chain").clicked() {
                            edit = Some((i, 1));
                        }
                        if ui::icon_button(ui, true, icon::DELETE, "Remove").on_hover_text("Takes it out of the chain").clicked() {
                            edit = Some((i, 0));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let plugin = matches!(n, Node::Fx(i) if params.rack[i].is_some_and(|s| s.fx.kind() == FxKind::Plugin));
                        if n != Node::Echo && !plugin {
                            reset = ui::icon_button(ui, true, icon::REFRESH, "Reset")
                                .on_hover_text("Back to the defaults for this step. On/off stays as it is")
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
            }
            self.panel(ui, n, m);
            if reset {
                n.reset(&mut self.saved.params);
            }
            if suggest && let Some(s) = self.saved.smart.clone() {
                n.suggest(&mut self.saved.params, &s);
            }
        });
        ui.add_space(6.0);
        edit
    }

    /// Save and recall whole settings. Picking one keeps Bypass as it is.
    fn presets_card(&mut self, ui: &mut egui::Ui) {
        let current = self.saved.presets.iter().position(|pr| same_preset(&pr.params, &self.saved.params));
        let desc = match current {
            Some(i) => format!("Using \"{}\". Presets change the style; Smart setup's levels and EQ stay", self.saved.presets[i].name),
            None => "Changed since you picked one. Type a name and Save to keep this style".into(),
        };
        let (mut pick, mut save, mut delete) = (None, false, None);
        let (presets, name) = (&self.saved.presets, &mut self.preset_name);
        ui::setting(ui, icon::SETTINGS, "Preset", &desc, |ui| {
            save = ui
                .add_enabled(!name.trim().is_empty(), egui::Button::new("Save"))
                .on_hover_text("Saves the current style under this name (replaces one with the same name)")
                .clicked();
            ui.add(egui::TextEdit::singleline(name).hint_text("New preset").desired_width(90.0));
            let shown = current.map_or("Choose", |i| presets[i].name.as_str());
            egui::ComboBox::from_id_salt("preset").width(110.0).selected_text(shown).truncate().show_ui(ui, |ui| {
                for (i, pr) in presets.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let bin = egui::Button::new(RichText::new(icon::DELETE).font(ui::icon_font(12.0))).frame(false);
                        if ui.add(bin).on_hover_text("Delete this preset").clicked() {
                            delete = Some(i);
                        }
                        if ui.add(egui::Button::selectable(current == Some(i), &pr.name)).clicked() {
                            pick = Some(i);
                        }
                    });
                }
            });
        });
        if let Some(i) = pick {
            self.saved.params = with_preset(&self.saved.params, &self.saved.presets[i].params);
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
        if let Some(i) = delete {
            self.saved.presets.remove(i);
        }
    }

    /// Room noise, then you reading a passage; every effect is set from that. While it runs
    /// the passage and a progress bar show here; afterwards what changed, with Undo.
    fn smart_card(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let can_start = self.engine.is_some() && !self.saved.params.bypass && self.record_progress.is_none();
        let (calib, report, has_undo) = (&self.calib, &self.smart_report, self.smart_undo.is_some());
        let (mut start, mut cancel, mut undo, mut dismiss) = (false, false, false, false);
        ui::card(ui, |ui| {
            let desc = "Listens to your room for 3 seconds and to you reading for 15, then sets each effect for your voice. Friends hear silence while it listens";
            ui::header(ui, icon::SMART, "Smart setup", desc, |ui| {
                if calib.is_some() {
                    cancel = ui.button("Cancel").clicked();
                } else {
                    start = ui::accent_button(ui, can_start, "Start").on_disabled_hover_text("Needs audio running, Bypass off and no test recording running").clicked();
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
                ui.horizontal(|ui| {
                    if has_undo {
                        undo = ui.button("Undo").on_hover_text("Puts every setting back to how it was before Smart setup").clicked();
                    }
                    dismiss = ui.button("Dismiss").on_hover_text("Hides this. Smart setup's settings stay").clicked();
                });
            }
        });
        if dismiss {
            // Undo would also throw away whatever was changed since, so it goes with the report.
            (self.smart_report, self.smart_undo) = (Vec::new(), None);
        }
        if start {
            self.calib = Some(Calib { start: Instant::now(), frame: 0, quiet: Vec::new(), talk: Vec::new(), audio: None });
            // A second shorter than setup, so it's back in time.
            self.shared.lock().unwrap().record = Some(vec![0.0; (CALIB_END as usize - 1) * dsp::SR as usize]);
            self.smart_report.clear();
        }
        if cancel {
            self.calib = None;
            self.stop_recording();
        }
        if undo && let Some(old) = self.smart_undo.take() {
            self.saved.params = Params { bypass: self.saved.params.bypass, ..old };
            self.smart_report = vec!["Undone: your settings are back to how they were.".into()];
        }
    }

    /// Nothing selected in the chain: levels, spectrum and Bypass.
    fn overview(&mut self, ui: &mut egui::Ui, m: &Meters) {
        let p = ui::pal(ui);
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
        let desc = if self.saved.devices.monitor.is_some() {
            "Send your mic without cleaning, to compare. Echo cancellation and the clip guard stay on"
        } else {
            "Send your mic without cleaning, to compare. Turn on Hear myself to hear the difference yourself"
        };
        ui::setting(ui, icon::POWER, "Bypass", desc, |ui| {
            ui::toggle(ui, &mut self.saved.params.bypass);
        });
    }

    /// The settings of one chain box, shown right under the chain: its graph or meters, the
    /// hold-to-compare button and every setting it has.
    fn panel(&mut self, ui: &mut egui::Ui, node: Node, m: &Meters) {
        let p = ui::pal(ui);
        let snap = self.saved.params.clone(); // heap-free copy for the graphs to read
        let can_hear = self.saved.devices.monitor.is_some();
        let (shown, hold) = (self.ab_shown, &mut self.ab_hold);
        let (agc_hist, pitch_hist, eq_ui) = (&self.agc_hist, &self.pitch_hist, &mut self.eq_ui);
        let (d, inputs, outputs) = (&mut self.saved.devices, &self.inputs, &self.outputs);
        let (rates, in_channels) = (self.rates, self.in_channels);
        let off_rate = |i: usize| rates[i].filter(|&r| r != dsp::SR as u32);
        let deep_state = (self.deep_ready, self.deep_error.clone());
        let pr = &mut self.saved.params;
        let (host, entries, owner) = (&mut self.plugins, &mut self.saved.plugins, self.hwnd);
        let mut refresh = false;
        let refresh_button = |ui: &mut egui::Ui| ui.button("Refresh list").on_hover_text("Plugged something in?").clicked();
        match node {
            Node::Input => {
                ui::setting(ui, icon::MIC, "Input device", "The mic or line input to clean up", |ui| {
                    refresh |= refresh_button(ui);
                    let before = d.input.clone();
                    pick(ui, "mic", &mut d.input, inputs);
                    if d.input != before {
                        d.input_channel = None;
                    }
                });
                // Audio interfaces show up in Windows as one device for all their inputs.
                if in_channels > 1 {
                    let desc = "Which of the device's inputs your mic is plugged into. Mixing them makes a mic on one input 6 dB quieter and adds the other's hiss";
                    ui::setting(ui, icon::MIC, "Channel", desc, |ui| channel_pick(ui, &mut d.input_channel, in_channels));
                }
                if let Some(r) = off_rate(0) {
                    let desc = &format!("MicClean converts it to 48000 Hz. Setting the device to 48000 Hz in {} skips that step", os::SOUND_SETTINGS);
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
                let desc = "What you hear on speakers, or open-back headphones your mic can hear. \"Everything you hear\" leaves out MicClean itself, so Hear myself can stay on";
                ui::setting(ui, icon::SPEAKERS, "Cancel echo from", desc, |ui| {
                    refresh |= refresh_button(ui);
                    pick_opt(ui, "echo", &mut d.echo_ref, &echo_choices(outputs), "Off");
                });
                match &d.echo_ref {
                    None => {
                        let desc = "Off. Turn it on if your mic hears your speakers or open-back headphones (friends hear themselves echo). Closed-back headphones don't need it";
                        ui::setting(ui, icon::HEADPHONE, "Echo cancellation", desc, |_| {});
                    }
                    Some(spk) => ui::card(ui, |ui| {
                        let what = if spk == audio::EVERYTHING { "your apps play (not MicClean itself)".to_string() } else { format!("{spk} plays") };
                        ui::header(ui, icon::SPEAKERS, "Echo cancellation", &format!("Removing what {what} from your mic"), |_| {});
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
                ab_row(ui, Stage::Denoise, shown, hold, can_hear);
                ui::row(ui, "Strength", |ui| {
                    ui.add(egui::Slider::new(&mut pr.denoise_strength, 0.0..=1.0));
                });
                ui::row(ui, "Model", |ui| {
                    ui.selectable_value(&mut pr.denoiser, Denoiser::DeepFilterNet, "DeepFilterNet");
                    ui.selectable_value(&mut pr.denoiser, Denoiser::RNNoise, "RNNoise");
                });
                let note = match (pr.denoiser, &deep_state) {
                    (Denoiser::RNNoise, _) => "Light on the CPU, and gentle on held sung notes. DeepFilterNet gives a cleaner voice in loud rooms for about 10% of one CPU core".to_string(),
                    (Denoiser::DeepFilterNet, (true, _)) => "A cleaner voice in loud rooms, for about 10% of one CPU core. RNNoise is lighter and gentler on held sung notes".to_string(),
                    (Denoiser::DeepFilterNet, (_, Some(e))) => format!("DeepFilterNet didn't load ({e}), so RNNoise is running"),
                    (Denoiser::DeepFilterNet, _) => "Loading DeepFilterNet... RNNoise runs until it's ready".to_string(),
                };
                ui.label(RichText::new(note).small().color(p.text2));
                more(ui, "noise", |ui| {
                    ui::row(ui, "Back off while singing", |ui| {
                        ui.add(egui::Slider::new(&mut pr.sing_protect, 0.0..=1.0))
                            .on_hover_text("Raise this if held notes sound watery or thin.");
                    });
                });
            }),
            Node::Gate => {
                ui::group(ui, icon::MUTE, Node::Gate.title(), "Mutes you when you're not talking or singing", Some(&mut pr.gate), |ui| {
                    if let Some(t) = graphs::gate_graph(ui, &snap, m) {
                        pr.gate_threshold_db = t.clamp(-80.0, -10.0);
                    }
                    ui.add_space(6.0);
                    ab_row(ui, Stage::Gate, shown, hold, can_hear);
                    ui::row(ui, "Minimum level", |ui| {
                        slider(ui, &mut pr.gate_threshold_db, -80.0..=-10.0, " dB");
                    });
                    ui::row(ui, "Hold open after you stop", |ui| {
                        slider(ui, &mut pr.gate_hold_ms, 0.0..=2000.0, " ms");
                    });
                    more(ui, "gate", |ui| {
                        ui::row(ui, "Close below it by", |ui| {
                            slider(ui, &mut pr.gate_hysteresis_db, 0.0..=20.0, " dB")
                                .on_hover_text("Keeps fading notes open. 0 = opens and closes at the same level.");
                        });
                        ui::row(ui, "Voice confidence needed", |ui| {
                            ui.add(egui::Slider::new(&mut pr.vad_threshold, 0.05..=0.95));
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
                });
                ui::card(ui, |ui| {
                    let desc = "A held note that started as voice keeps the gate open, even when the voice detector loses it";
                    ui::header(ui, icon::MUSIC, "Singing detector", desc, |_| {});
                    graphs::pitch_view(ui, pitch_hist, m, "last 6 s");
                });
            }
            Node::Level => {
                let desc = format!("Evens out quiet and loud · now {:+.1} dB", m.agc_db);
                ui::group(ui, icon::VOLUME, Node::Level.title(), &desc, Some(&mut pr.agc), |ui| {
                    graphs::history_graph(ui, agc_hist, -12.0, snap.agc_max_gain_db.max(1.0), Some(0.0), p.accent, "gain it applied, last 30 s");
                    ui.add_space(6.0);
                    ab_row(ui, Stage::Agc, shown, hold, can_hear);
                    ui::row(ui, "Target level", |ui| {
                        slider(ui, &mut pr.agc_target_db, -36.0..=-10.0, " dB");
                    });
                    more(ui, "level", |ui| {
                        ui::row(ui, "Max boost", |ui| {
                            slider(ui, &mut pr.agc_max_gain_db, 0.0..=30.0, " dB");
                        });
                    });
                });
            }
            Node::Fx(i) => {
                let Some(dsp::Slot { on, fx, .. }) = &mut pr.rack[i] else { return };
                let (fm, stage) = (m.fx[i], Stage::Fx(i));
                let title = fx.kind().title();
                match fx {
                    Fx::Eq { bands } => ui::group(ui, icon::EQ, title, "Up to 5 bands over your live output spectrum", Some(on), |ui| {
                        eq_ui.show(ui, bands, &m.spec_out);
                        ab_row(ui, stage, shown, hold, can_hear);
                    }),
                    Fx::Deess { threshold_db } => {
                        let cut = -fm.change_db;
                        let desc = format!("Tames harsh S sounds · now -{cut:.1} dB");
                        ui::group(ui, icon::MUSIC, title, &desc, Some(on), |ui| {
                            let (lvl, thr) = (fm.level_db, *threshold_db);
                            let hot = if lvl > thr { graphs::AMBER } else { p.accent };
                            graphs::meter_row(ui, "S sounds (5 kHz+)", lvl, -60.0, 0.0, Some(thr), hot, &format!("{lvl:.0} dB"));
                            graphs::meter_row(ui, "Cutting them by", cut, 0.0, 12.0, None, graphs::AMBER, &format!("-{cut:.1} dB"));
                            ab_row(ui, stage, shown, hold, can_hear);
                            ui::row(ui, "Threshold", |ui| {
                                slider(ui, threshold_db, -50.0..=0.0, " dB");
                            });
                        });
                    }
                    Fx::Warmth { amount } => {
                        let desc = "Soft saturation, like tube gear and tape: adds harmonics that make a voice sound fuller";
                        let added = if *on { format!("{:.0} dB", fm.level_db) } else { "off".into() };
                        ui::group(ui, icon::MUSIC, title, desc, Some(on), |ui| {
                            graphs::meter_row(ui, "Harmonics added", fm.level_db, -60.0, 0.0, None, p.accent, &added);
                            ab_row(ui, stage, shown, hold, can_hear);
                            ui::row(ui, "Amount", |ui| {
                                ui.add(egui::Slider::new(amount, 0.0..=1.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                            });
                        });
                    }
                    Fx::Air { amount } => {
                        let desc = "Opens up the top end for a clearer, more present voice, and backs off on S sounds so they never get harsh";
                        let lift = if *on { format!("+{:.1} dB", fm.change_db) } else { "off".into() };
                        ui::group(ui, icon::MUSIC, title, desc, Some(on), |ui| {
                            graphs::meter_row(ui, "Top end lifted by", fm.change_db, 0.0, dsp::AIR_MAX_DB, None, p.good, &lift);
                            ab_row(ui, stage, shown, hold, can_hear);
                            ui::row(ui, "Amount", |ui| {
                                ui.add(egui::Slider::new(amount, 0.0..=1.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                            });
                        });
                    }
                    Fx::Plugin { key } => {
                        let key = *key;
                        let entry = entries.iter_mut().find(|e| e.key == key);
                        let (name, desc) = match &entry {
                            Some(e) => (e.info.name.clone(), format!("{} · {} plugin", e.info.vendor, e.info.format.name())),
                            None => ("Plugin".to_string(), "Its settings are missing".to_string()),
                        };
                        let (mut retry, mut again) = (false, false);
                        let change = if *on { format!("{:+.1} dB", fm.change_db) } else { "off".into() };
                        ui::group(ui, icon::EQ, &name, &desc, Some(on), |ui| match (entry, host.get_mut(key)) {
                            (Some(e), _) if e.crashed => {
                                ui.label("It crashed or stopped responding while it ran, so it's kept off. MicClean kept running without it.");
                                if ui.button("Try again").clicked() {
                                    e.crashed = false;
                                    again = true;
                                }
                            }
                            (_, Some(Ok(inst))) => {
                                ui::row(ui, "Plugin window", |ui| {
                                    let open = inst.editor_open();
                                    if ui.button(if open { "Close" } else { "Open" }).clicked() {
                                        if open {
                                            inst.close_editor();
                                        } else {
                                            inst.open_editor(owner, ui.visuals().dark_mode);
                                        }
                                    }
                                });
                                if let Some(why) = &inst.editor_error {
                                    ui.label(RichText::new(why).color(graphs::AMBER));
                                }
                                ui::row(ui, "Level change", |ui| {
                                    ui.label(change);
                                });
                                ab_row(ui, stage, shown, hold, can_hear);
                            }
                            (_, Some(Err(why))) => {
                                ui.label(format!("It didn't load: {why}"));
                                retry = ui.button("Try again").clicked();
                            }
                            (None, _) => {
                                ui.label("This plugin isn't in your settings any more. Remove it, then add it again.");
                            }
                            _ if !plugins::SUPPORTED => {
                                ui.label("Plugins run on Windows only for now, so it stays off here.");
                            }
                            _ => {
                                ui.label("Loading…");
                            }
                        });
                        if retry {
                            host.retry(key);
                        }
                        *on |= again;
                    }
                    Fx::Comp(c) => {
                        let desc = format!("Keeps loud parts in check · now -{:.1} dB", -fm.change_db);
                        ui::group(ui, icon::EQ, title, &desc, Some(on), |ui| {
                            if let Some(t) = graphs::comp_graph(ui, &{ *c }, &fm) {
                                c.threshold_db = t.clamp(-50.0, 0.0);
                            }
                            ui.add_space(6.0);
                            ab_row(ui, stage, shown, hold, can_hear);
                            ui::row(ui, "Threshold", |ui| {
                                slider(ui, &mut c.threshold_db, -50.0..=0.0, " dB");
                            });
                            ui::row(ui, "Ratio", |ui| {
                                ui.add(egui::Slider::new(&mut c.ratio, 1.0..=20.0).logarithmic(true).suffix(":1"));
                            });
                            more(ui, "comp", |ui| {
                                ui::row(ui, "Attack", |ui| {
                                    slider(ui, &mut c.attack_ms, 0.5..=50.0, " ms");
                                });
                                ui::row(ui, "Release", |ui| {
                                    slider(ui, &mut c.release_ms, 20.0..=1000.0, " ms");
                                });
                                ui::row(ui, "Makeup gain", |ui| {
                                    slider(ui, &mut c.makeup_db, 0.0..=24.0, " dB");
                                });
                            });
                        });
                    }
                }
            }            Node::Limiter => ui::group(ui, icon::VOLUME, Node::Limiter.title(), "Always on, so nothing you send can clip", None, |ui| {
                let red = Color32::from_rgb(232, 72, 72);
                let limiting = m.lim_db > 0.1;
                let ceil = 20.0 * dsp::LIMIT_CEIL.log10();
                let peak = format!("{:.1} dB", m.out_db);
                graphs::meter_row(ui, "Output peak", m.out_db, -60.0, 0.0, Some(ceil), if limiting { red } else { p.accent }, &peak);
                let amount = if limiting { format!("-{:.1} dB", m.lim_db) } else { "idle".into() };
                graphs::meter_row(ui, "Limiting", m.lim_db, 0.0, 12.0, None, red, &amount);
                ui.label(RichText::new("Working on every word sounds like clipping: turn Loudness down (Home or Output).").small().color(p.text2));
            }),
            Node::Output => {
                let desc = format!("Where the cleaned sound goes. For Discord or other apps, pick {}; they then use {} as their mic", os::cable::SEND, os::cable::MIC);
                ui::setting(ui, icon::SPEAKERS, "Send to", &desc, |ui| {
                    refresh |= refresh_button(ui);
                    pick(ui, "out", &mut d.output, outputs);
                });
                let system = if cfg!(windows) { "Windows" } else { "macOS" };
                let convert = &format!("{system} converts MicClean's 48000 Hz for it, which adds delay. {}", os::SET_48K);
                if let Some(r) = off_rate(1) {
                    rate_note(ui, &d.output, r, convert);
                }
                if audio::can_be_exclusive(&d.output) {
                    let desc = match self.engine.as_ref().and_then(|e| e.shared_output.as_deref()) {
                        Some(why) if d.exclusive_output => format!("Couldn't: {why}"),
                        _ => "Takes the cable for MicClean alone, so a Discord screen share doesn't send your voice a second time. Hear myself still plays through Windows, so a screen share includes it".into(),
                    };
                    ui::setting(ui, icon::SPEAKERS, "Hide from screen share", &desc, |ui| {
                        ui::toggle(ui, &mut d.exclusive_output);
                    });
                }
                ui::setting(ui, icon::HEADPHONE, "Monitor", "Hear yourself, e.g. on your headphones", |ui| {
                    pick_opt(ui, "mon", &mut d.monitor, outputs, "Off");
                });
                if let (Some(r), Some(mon)) = (off_rate(2), &d.monitor) {
                    rate_note(ui, mon, r, convert);
                }
                ui::group(ui, icon::SPEAKERS, "What gets sent", "After every effect and the clip guard. Setup has the steps for Discord", None, |ui| {
                    level_row(ui, "Out", m.out_db);
                    ui::row(ui, "Loudness (output gain)", |ui| {
                        slider(ui, &mut pr.output_gain_db, -20.0..=30.0, " dB");
                    });
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
        // Sounds are learned after noise removal, which Bypass skips.
        let busy = learning.is_some() || self.engine.is_none() || self.saved.params.bypass;
        let why = "Needs audio running and Bypass off";

        section(ui, "Sounds to remove");
        let mut learn_noise = false;
        let mut forget = None;
        ui::card(ui, |ui| {
            ui::header(ui, icon::LEARN, "Learn a noise", "Name it, then make the sound for 5 seconds. Quiet moments are left out", |ui| {
                learn_noise = ui::accent_button(ui, !busy, "Learn (5 s)").on_disabled_hover_text(why).clicked();
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
                    if n.profile.outdated() {
                        let tip = "Learned by an older version that took in the room too, so it matched almost anything. Learning it again under the same name replaces it";
                        ui.label(RichText::new("Learn it again").small().color(p.text2)).on_hover_text(tip);
                    } else if let Some(&d) = self.meters.match_dist.get(i) {
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
        let outdated = self.saved.profiles.keep.as_ref().is_some_and(|k| k.outdated());
        let desc = match (learned, outdated) {
            (_, true) => "Learned by an older version that took in the room too, so it matched almost anything. Learn it again",
            (true, _) => "Learned. Sounds like this are never muted",
            _ => "Talk and sing for 10 seconds so it never cuts you. Pauses are left out",
        };
        let (mut learn_voice, mut forget_voice) = (false, false);
        ui::card(ui, |ui| {
            ui::header(ui, icon::MUSIC, "Learn your voice", desc, |ui| {
                learn_voice = ui::accent_button(ui, !busy, "Learn (10 s)").on_disabled_hover_text(why).clicked();
                forget_voice = learned && ui.button("Forget").clicked();
            });
            if let Some((Kind::Keep, prog)) = learning {
                ui.add(egui::ProgressBar::new(prog).desired_height(4.0));
            }
            if learned && !outdated {
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

    /// Settings' backups: the automatic ones (newest first) to restore, one made now, and the
    /// folder to send one to a friend from. A settings file dropped on the window loads too.
    fn backups(&mut self, ui: &mut egui::Ui) {
        let p = ui::pal(ui);
        let (mut now, mut open) = (false, false);
        let mut restore = None;
        ui::card(ui, |ui| {
            let desc = "Kept from each start, the last 10. To load one a friend sent you, drag the file onto this window. Your devices stay as they are";
            ui::header(ui, icon::FOLDER, "Settings backups", desc, |ui| {
                open = ui.button("Open folder").clicked();
                now = ui.button("Back up now").clicked();
            });
            for b in backup::list().iter().take(10) {
                ui::row(ui, &backup::label(b), |ui| {
                    if ui.button("Restore").on_hover_text("Your current settings are backed up first").clicked() {
                        restore = Some(b.clone());
                    }
                });
            }
            if !self.backup_msg.is_empty() {
                ui.label(RichText::new(&self.backup_msg).small().color(p.text2));
            }
        });
        if now {
            self.backup_msg = match self.back_up() {
                Ok(()) => "Backed up.".into(),
                Err(e) => format!("Couldn't back up: {e}"),
            };
        }
        if open {
            let dir = backup::dir();
            let _ = std::fs::create_dir_all(&dir);
            os::open(dir);
        }
        if let Some(b) = restore {
            self.load_settings_file(&b);
        }
    }

    fn back_up(&self) -> Result<(), String> {
        backup::save(&ron::to_string(&self.saved).map_err(|e| e.to_string())?)
    }

    /// Loads a backup or a settings file someone sent, keeping this PC's devices. The current
    /// settings are backed up first, so a wrong file is one Restore away from undone.
    fn load_settings_file(&mut self, path: &std::path::Path) {
        let name = path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let Some(settings) = std::fs::read_to_string(path).ok().and_then(|t| backup::settings_in(&t)) else {
            self.backup_msg = format!("{name} isn't a MicClean settings file.");
            return;
        };
        let mut loaded = match parse_saved(Some(&settings)) {
            Ok(s) => s,
            Err(e) => {
                self.backup_msg = format!("Didn't load {name}: this version of MicClean can't read it ({e}).");
                return;
            }
        };
        if let Err(e) = self.back_up() {
            self.backup_msg = format!("Didn't load {name}: couldn't back up your current settings first ({e}).");
            return;
        }
        loaded.devices = self.saved.devices.clone();
        loaded.setup_done = true;
        self.saved = loaded;
        (self.smart_report, self.smart_undo, self.reset_undo) = (Vec::new(), None, None);
        self.backup_msg = format!("Loaded {name}. Your settings from before are the newest backup.");
        logfile::line(&format!("Loaded settings from {}", path.display()));
    }

    /// Shown in Setup and Settings.
    fn autostart_row(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let desc = format!("Opens hidden in the {} when you sign in, so Discord never listens to a silent cable", os::TRAY);
        ui::setting(ui, icon::POWER, os::AUTOSTART, &desc, |ui| {
            changed = ui::toggle(ui, &mut self.autostart).changed();
        });
        if changed && !autostart::set(self.autostart) {
            self.autostart = autostart::enabled();
            self.status = if cfg!(windows) || self.installed {
                "Couldn't change the startup entry.".into()
            } else {
                "Move MicClean into Applications first, then turn this on.".into()
            };
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        self.autostart_row(ui);
        self.plugin_settings(ui);
        let (mut arm, mut reset, mut cancel, mut undo) = (false, false, false, false);
        let desc = match (self.reset_armed, self.reset_undo.is_some()) {
            (true, _) => "Every switch and slider goes back to the default, Smart setup's choices too. Learned sounds and presets stay",
            (false, true) => "Done. Undo puts your settings back",
            (false, false) => "Back to the defaults. Learned sounds and presets stay",
        };
        ui::setting(ui, icon::REFRESH, "Reset all settings", desc, |ui| {
            if self.reset_armed {
                cancel = ui.button("Cancel").clicked();
                reset = ui::accent_button(ui, true, "Reset everything").clicked();
            } else {
                arm = ui.button("Reset").clicked();
                if self.reset_undo.is_some() {
                    undo = ui.button("Undo").clicked();
                }
            }
        });
        self.reset_armed = (self.reset_armed || arm) && !cancel && !reset;
        if reset {
            self.reset_undo = Some(std::mem::take(&mut self.saved.params));
        }
        if undo && let Some(old) = self.reset_undo.take() {
            self.saved.params = old;
        }
        self.backups(ui);
        self.updates(ui);
        let mut show_log = false;
        let desc = format!("Device errors, glitches, restarts and updates, for troubleshooting. It stays on {}", os::THIS_PC);
        ui::setting(ui, icon::FOLDER, "Log file", &desc, |ui| {
            show_log = ui.button("Show in folder").clicked();
        });
        if show_log {
            os::reveal(&logfile::path());
        }
        let quit = if cfg!(windows) { "Right-click the tray icon to quit" } else { "Click its menu bar icon to quit" };
        let desc = format!("Version {}. Closing the window keeps it running in the {}. {quit}", update::VERSION, os::TRAY);
        ui::setting(ui, icon::INFO, "About MicClean", &desc, |_| {});
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
            ui.label(RichText::new(format!("You have {}. {}", update::VERSION, update::HOW)).color(p.text2));
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
            self.start_update(r);
        }
    }

    fn start_update(&self, r: update::Release) {
        self.updater.install(r, self.hidden);
    }

    fn updates(&mut self, ui: &mut egui::Ui) {
        use update::State;
        let st = self.updater.state();
        let desc = match &st {
            _ if !self.installed => "This copy isn't installed, so it doesn't update itself".to_string(),
            State::Idle => format!("You have {}", update::VERSION),
            State::Checking => "Checking GitHub...".into(),
            State::UpToDate => format!("You're up to date ({})", update::VERSION),
            State::Available(r) => format!("Version {} is available. {}", r.version, update::HOW.trim_end_matches('.')),
            State::Installing => "Downloading and installing. MicClean restarts by itself".into(),
            State::Restarting => "Restarting into the new version".into(),
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
            State::Checking | State::Installing | State::Restarting => {
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
            self.start_update(r);
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
        // Every Tone that's on, added up.
        let mut gains = vec![0.0; freqs.len()];
        let mut any = false;
        for s in p.rack.iter().flatten().filter(|s| s.on) {
            if let Fx::Eq { bands } = &s.fx {
                any = true;
                gains.iter_mut().zip(dsp::eq_curve(bands, &freqs)).for_each(|(g, b)| *g += b);
            }
        }
        let amber = Color32::from_rgb(240, 190, 50);
        let col = if any { amber } else { amber.gamma_multiply(0.3) };
        let curve = freqs.iter().zip(&gains).map(|(&f, &g)| Pos2::new(x_of(f), y_eq(g))).collect();
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

/// The less used settings of a box, folded away.
fn more(ui: &mut egui::Ui, id: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new("More settings").id_salt(id).show(ui, body);
}

/// Which input of a multi-channel device to use, or all of them mixed.
fn channel_pick(ui: &mut egui::Ui, v: &mut Option<u16>, channels: u16) {
    let shown = v.map_or("All, mixed".into(), |c| format!("Input {}", c + 1));
    egui::ComboBox::from_id_salt("mic_ch").selected_text(shown).show_ui(ui, |ui| {
        ui.selectable_value(v, None, "All, mixed");
        for c in 0..channels {
            ui.selectable_value(v, Some(c), format!("Input {}", c + 1));
        }
    });
}

/// "2  Where it goes  ✓": a Setup step's heading, above its card.
fn step(ui: &mut egui::Ui, n: u32, title: &str, done: bool) {
    let p = ui::pal(ui);
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(n.to_string()).font(ui::semibold(14.0)).color(p.accent));
        ui.label(RichText::new(title).font(ui::semibold(14.0)));
        if done {
            ui.label(RichText::new(icon::CHECK).font(ui::icon_font(12.0)).color(p.good));
        }
    });
    ui.add_space(2.0);
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
    fn a_preset_changes_the_style_but_keeps_what_smart_setup_measured() {
        // Presets made on a first start, before Smart setup ran.
        let presets = starter_presets(&Params::default());
        let mut tuned = Params { input_gain_db: 18.0, gate_threshold_db: -38.0, output_gain_db: -4.0, ..Params::default() };
        tuned.comp_mut().unwrap().makeup_db = 2.0;
        tuned.bands_mut().unwrap()[1] = Some(Band { kind: BandKind::Bell, freq: 300.0, gain_db: -3.0, q: 1.0, on: true });
        let singing = with_preset(&tuned, &presets[1].params);
        assert_eq!((singing.input_gain_db, singing.gate_threshold_db, singing.output_gain_db), (18.0, -38.0, -4.0));
        assert_eq!((singing.bands(), singing.comp().unwrap().makeup_db), (tuned.bands(), 2.0));
        assert_eq!((singing.gate_hold_ms, singing.comp().unwrap().ratio), (900.0, 2.0), "the Singing style itself");
        assert!(same_preset(&presets[1].params, &singing) && !same_preset(&presets[2].params, &singing));
    }

    #[test]
    fn old_fixed_eq_save_loads_as_bands() {
        // A real save from before the parametric EQ.
        let old = r#"(devices:(input:"Test Microphone",output:"CABLE Input (VB-Audio Virtual Cable)",monitor:None,echo_ref:None),params:(bypass:false,input_gain_db:0.0,denoise:true,denoise_strength:1.0,sing_protect:0.3,gate:false,gate_threshold_db:-50.0,vad_threshold:0.5,gate_hold_ms:400.0,gate_floor_db:-60.0,learn_match:3.0,agc:true,agc_target_db:-14.0,agc_max_gain_db:15.0,eq:true,low_cut_hz:165.0,low_gain_db:0.0,mid_freq_hz:300.0,mid_gain_db:2.8,high_gain_db:3.0,deess:true,deess_threshold_db:-24.0,comp:true,comp_threshold_db:-24.0,comp_ratio:3.0,comp_attack_ms:5.0,comp_release_ms:120.0,comp_makeup_db:4.0,output_gain_db:6.0),profiles:(noises:[],keep:None))"#;
        let mut store = Mem(old.into());
        let s = load(&store);
        assert_eq!(s.devices.input, "Test Microphone", "settings were thrown away");
        assert_eq!(s.params.output_gain_db, 6.0);
        let b = *s.params.bands().unwrap();
        assert_eq!(b[0].map(|b| (b.kind, b.freq)), Some((BandKind::LowCut, 165.0)));
        assert!(b[1].is_none(), "0 dB low shelf should not become a band");
        assert_eq!(b[2].map(|b| (b.kind, b.freq, b.gain_db)), Some((BandKind::Bell, 300.0, 2.8)));
        assert_eq!(b[3].map(|b| (b.kind, b.gain_db)), Some((BandKind::HighShelf, 3.0)));
        // Starter presets are built from these settings, so the EQ carries into all of them.
        let names: Vec<&str> = s.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Talking", "Singing", "Gaming"]);
        assert!(same_preset(&s.presets[0].params, &s.params), "Talking should match the loaded settings");
        assert!(s.presets.iter().all(|p| p.params.bands() == s.params.bands()));

        // After one save the file has bands only, and editing them sticks.
        let mut edited = s;
        edited.params.bands_mut().unwrap()[4] = Some(Band { kind: BandKind::Notch, freq: 60.0, gain_db: 0.0, q: 8.0, on: true });
        eframe::set_value(&mut store, eframe::APP_KEY, &edited);
        assert!(!store.0.contains("low_cut_hz"));
        assert_eq!(load(&store).params.bands(), edited.params.bands());
    }

    #[test]
    fn a_0_8_save_keeps_every_setting() {
        // A real 0.8.0 save (presets cut to one). Any field that changes type must still read
        // this, or the whole file fails and every setting goes back to defaults.
        let old = r#"(devices:(input:"Loop-back 1/2 (Audient iD24)",input_channel:None,output:"CABLE Input (VB-Audio Virtual Cable)",monitor:None,echo_ref:Some("Analogue 1/2 (Audient iD24)"),exclusive_output:true),params:(bypass:false,input_gain_db:16.0,denoise:true,denoise_strength:1.0,sing_protect:0.75,gate:false,gate_threshold_db:-66.0,vad_threshold:0.5,gate_hold_ms:400.0,gate_floor_db:-60.0,gate_attack_ms:3.0,gate_release_ms:60.0,gate_hysteresis_db:6.0,learn_match:3.9,agc:true,agc_target_db:-22.0,agc_max_gain_db:15.0,eq:true,bands:(Some((kind:LowCut,freq:170.0,gain_db:0.0,q:0.71,on:true)),None,None,Some((kind:Bell,freq:3200.0,gain_db:1.0,q:1.0,on:true)),None),deess:true,deess_threshold_db:-39.0,comp:true,comp_threshold_db:-30.0,comp_ratio:3.0,comp_attack_ms:5.0,comp_release_ms:120.0,comp_makeup_db:4.0,output_gain_db:4.0),profiles:(noises:[],keep:None),update_check_off:false,update_popup_off:false,auto_update:true,presets:[(name:"Talking",params:(bypass:false,input_gain_db:16.0,denoise:true,denoise_strength:1.0,sing_protect:0.3,gate:true,gate_threshold_db:-67.0,vad_threshold:0.5,gate_hold_ms:400.0,gate_floor_db:-12.0,gate_attack_ms:2.0,gate_release_ms:60.0,gate_hysteresis_db:6.0,learn_match:3.0,agc:true,agc_target_db:-14.0,agc_max_gain_db:15.0,eq:true,bands:(Some((kind:LowCut,freq:165.0,gain_db:0.0,q:0.71,on:true)),None,Some((kind:Bell,freq:300.0,gain_db:2.8,q:1.0,on:true)),Some((kind:HighShelf,freq:4271.0,gain_db:3.0,q:0.71,on:true)),None),deess:true,deess_threshold_db:-24.0,comp:true,comp_threshold_db:-21.0,comp_ratio:3.0,comp_attack_ms:5.0,comp_release_ms:120.0,comp_makeup_db:0.0,output_gain_db:6.0))],presets_seeded:true,nav_compact:false,smart:Some((bypass:false,input_gain_db:16.0,denoise:true,denoise_strength:0.85,sing_protect:0.3,gate:true,gate_threshold_db:-66.0,vad_threshold:0.5,gate_hold_ms:400.0,gate_floor_db:-60.0,gate_attack_ms:3.0,gate_release_ms:60.0,gate_hysteresis_db:6.0,learn_match:3.9,agc:false,agc_target_db:-18.7,agc_max_gain_db:15.0,eq:true,bands:(Some((kind:LowCut,freq:170.0,gain_db:0.0,q:0.707,on:true)),None,None,None,Some((kind:HighShelf,freq:8000.0,gain_db:-2.0,q:0.707,on:true))),deess:true,deess_threshold_db:-39.0,comp:true,comp_threshold_db:-30.0,comp_ratio:3.0,comp_attack_ms:5.0,comp_release_ms:120.0,comp_makeup_db:4.0,output_gain_db:4.0)),setup_done:true)"#;
        let s = parse_saved(Some(old)).unwrap();
        assert_eq!(s.devices.echo_ref.as_deref(), Some("Analogue 1/2 (Audient iD24)"));
        assert_eq!((s.params.input_gain_db, s.params.learn_match, s.params.agc_target_db), (16.0, 3.9, -22.0));
        assert_eq!((s.presets.len(), s.presets[0].params.gate_floor_db), (1, -12.0));
        assert!(s.setup_done && s.auto_update && s.smart.is_some());
        // Its fixed effects became a rack, in their old order, with their settings: in the
        // settings, the preset and the Smart setup record alike.
        let kinds: Vec<(FxKind, bool)> = s.params.rack.iter().flatten().map(|f| (f.fx.kind(), f.on)).collect();
        assert_eq!(kinds, [(FxKind::Eq, true), (FxKind::Air, false), (FxKind::Deess, true), (FxKind::Warmth, false), (FxKind::Comp, true)]);
        assert_eq!(s.params.bands().unwrap()[3].map(|b| (b.freq, b.gain_db)), Some((3200.0, 1.0)));
        assert_eq!((s.params.deess_threshold(), s.params.comp().unwrap().threshold_db), (-39.0, -30.0));
        let talking = &s.presets[0].params;
        assert_eq!((talking.bands().unwrap()[2].map(|b| b.freq), talking.comp().unwrap().threshold_db), (Some(300.0), -21.0));
        assert_eq!(s.smart.as_ref().unwrap().bands().unwrap()[4].map(|b| b.gain_db), Some(-2.0));
        // Saved again, it has a rack and loads as it is.
        let again = ron::to_string(&s).unwrap();
        assert!(!again.contains("deess_threshold_db"));
        assert!(parse_saved(Some(&again)).unwrap().params == s.params);
        // One that can't be read starts from the defaults, starter presets included.
        let bad = load(&Mem("(setup_done:Some(true))".into()));
        assert_eq!((bad.devices.input.as_str(), bad.presets.len()), ("", 3));
    }
}
