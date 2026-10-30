//! Voice chain on 10 ms mono frames at 48 kHz (RNNoise's native frame size):
//! denoise -> voice gate -> auto level -> EQ -> de-esser -> compressor -> limiter.
//! Echo cancellation runs before this, in audio.rs, because it needs the speaker signal.

use nnnoiseless::DenoiseState;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const SR: f32 = 48_000.0;
pub const FRAME: usize = DenoiseState::FRAME_SIZE; // 480 samples = 10 ms
pub const NB: usize = 16; // band count for the learned profiles and Smart setup

/// Lower edge of analysis band `i` (log-spaced, 100 Hz..12 kHz; `i = NB` is the top edge).
pub fn band_edge(i: usize) -> f32 {
    100.0 * 120f32.powf(i as f32 / NB as f32)
}
pub const SPEC_BINS: usize = 64;
/// Spectrum display covers SPEC_LO..SPEC_LO*SPEC_SPAN Hz on a log axis.
pub const SPEC_LO: f32 = 40.0;
pub const SPEC_SPAN: f32 = 500.0;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    pub bypass: bool,
    pub input_gain_db: f32,
    pub denoise: bool,
    /// 0..1, how much of the denoised signal replaces the raw one.
    pub denoise_strength: f32,
    /// 0..1, how much denoising backs off while singing is detected.
    pub sing_protect: f32,
    pub gate: bool,
    pub gate_threshold_db: f32,
    pub vad_threshold: f32,
    pub gate_hold_ms: f32,
    pub gate_floor_db: f32,
    pub gate_attack_ms: f32,
    pub gate_release_ms: f32,
    /// Once open, the level has to drop this far below the threshold to count as quiet.
    pub gate_hysteresis_db: f32,
    /// Max normalised distance for a frame to count as a learned sound. Higher = matches more.
    pub learn_match: f32,
    pub agc: bool,
    pub agc_target_db: f32,
    pub agc_max_gain_db: f32,
    pub eq: bool,
    /// Fixed slots (None = unused) so cloning Params on the audio thread never allocates.
    pub bands: [Option<Band>; BANDS],
    pub deess: bool,
    pub deess_threshold_db: f32,
    pub comp: bool,
    pub comp_threshold_db: f32,
    pub comp_ratio: f32,
    pub comp_attack_ms: f32,
    pub comp_release_ms: f32,
    pub comp_makeup_db: f32,
    pub output_gain_db: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            bypass: false,
            input_gain_db: 0.0,
            denoise: true,
            denoise_strength: 1.0,
            sing_protect: 0.3,
            gate: true,
            gate_threshold_db: -50.0,
            vad_threshold: 0.5,
            gate_hold_ms: 400.0,
            gate_floor_db: -60.0,
            gate_attack_ms: 3.0,
            gate_release_ms: 60.0,
            gate_hysteresis_db: 6.0,
            learn_match: 3.0,
            agc: true,
            agc_target_db: -22.0,
            agc_max_gain_db: 15.0,
            eq: true,
            bands: [Some(Band { kind: BandKind::LowCut, freq: 80.0, gain_db: 0.0, q: 0.707, on: true }), None, None, None, None],
            deess: true,
            deess_threshold_db: -24.0,
            comp: true,
            comp_threshold_db: -24.0,
            comp_ratio: 3.0,
            comp_attack_ms: 5.0,
            comp_release_ms: 120.0,
            comp_makeup_db: 4.0,
            output_gain_db: 0.0,
        }
    }
}

impl Params {
    /// Every value inside what its slider allows, and NaN back to the default: a hand-edited or
    /// damaged settings file can't crash the audio thread (a clamp with min > max panics) or
    /// blow it up.
    pub fn sanitize(&mut self) {
        fn fix(v: &mut f32, lo: f32, hi: f32, def: f32) {
            *v = if v.is_finite() { v.clamp(lo, hi) } else { def };
        }
        let d = Params::default();
        fix(&mut self.input_gain_db, -20.0, 30.0, d.input_gain_db);
        fix(&mut self.denoise_strength, 0.0, 1.0, d.denoise_strength);
        fix(&mut self.sing_protect, 0.0, 1.0, d.sing_protect);
        fix(&mut self.gate_threshold_db, -80.0, -10.0, d.gate_threshold_db);
        fix(&mut self.vad_threshold, 0.0, 1.0, d.vad_threshold);
        fix(&mut self.gate_hold_ms, 0.0, 2000.0, d.gate_hold_ms);
        fix(&mut self.gate_floor_db, -90.0, 0.0, d.gate_floor_db);
        fix(&mut self.gate_attack_ms, 0.5, 50.0, d.gate_attack_ms);
        fix(&mut self.gate_release_ms, 10.0, 500.0, d.gate_release_ms);
        fix(&mut self.gate_hysteresis_db, 0.0, 20.0, d.gate_hysteresis_db);
        fix(&mut self.learn_match, 1.0, 8.0, d.learn_match);
        fix(&mut self.agc_target_db, -36.0, -10.0, d.agc_target_db);
        fix(&mut self.agc_max_gain_db, 0.0, 30.0, d.agc_max_gain_db);
        fix(&mut self.deess_threshold_db, -50.0, 0.0, d.deess_threshold_db);
        fix(&mut self.comp_threshold_db, -50.0, 0.0, d.comp_threshold_db);
        fix(&mut self.comp_ratio, 1.0, 20.0, d.comp_ratio);
        fix(&mut self.comp_attack_ms, 0.5, 50.0, d.comp_attack_ms);
        fix(&mut self.comp_release_ms, 20.0, 1000.0, d.comp_release_ms);
        fix(&mut self.comp_makeup_db, 0.0, 24.0, d.comp_makeup_db);
        fix(&mut self.output_gain_db, -20.0, 30.0, d.output_gain_db);
        for b in self.bands.iter_mut().flatten() {
            fix(&mut b.freq, 20.0, 20_000.0, 1000.0);
            fix(&mut b.q, 0.1, 18.0, 0.707);
            fix(&mut b.gain_db, -24.0, 24.0, 0.0);
        }
    }
}

pub const BANDS: usize = 5;
/// The safety limiter's ceiling (linear, about -0.26 dBFS).
pub const LIMIT_CEIL: f32 = 0.97;

/// Filter shape of an EQ band (RBJ cookbook biquads). Also used inside the de-esser.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BandKind {
    Bell,
    LowShelf,
    HighShelf,
    LowCut,
    HighCut,
    Notch,
}

impl BandKind {
    pub const ALL: [BandKind; 6] = [Self::Bell, Self::LowShelf, Self::HighShelf, Self::LowCut, Self::HighCut, Self::Notch];

    pub fn name(self) -> &'static str {
        match self {
            Self::Bell => "Bell",
            Self::LowShelf => "Low shelf",
            Self::HighShelf => "High shelf",
            Self::LowCut => "Low cut",
            Self::HighCut => "High cut",
            Self::Notch => "Notch",
        }
    }

    /// Cuts and notches have no gain, only frequency and Q.
    pub fn has_gain(self) -> bool {
        matches!(self, Self::Bell | Self::LowShelf | Self::HighShelf)
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Band {
    pub kind: BandKind,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
    pub on: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Kind {
    Noise,
    Keep,
}

/// Diagonal-Gaussian model of one learned sound's spectral shape (log band energies minus
/// their mean, so the same sound louder or quieter still matches), plus how quiet it gets.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub mean: [f32; NB],
    pub var: [f32; NB],
    /// Frames quieter than this (band level in dB, before input gain, so turning the gain up
    /// or down doesn't move it) aren't this sound: the room around it. None for profiles from
    /// before 0.6.2, which learned the room too and so matched everything.
    #[serde(default)]
    pub floor_raw_db: Option<f32>,
    /// 0.6.2 to 0.8: the same floor after input gain. Read once, turned into `floor_raw_db`.
    #[serde(default, skip_serializing)]
    pub floor_db: Option<f32>,
}

/// A frame's spectral shape and level. Bands over 40 dB under its loudest one count as empty
/// and sit at that floor: how empty they are is just noise. The level is the bands' mean and
/// the shape is each band minus it.
fn shape(f: &[f32; NB]) -> ([f32; NB], f32) {
    let top = f.iter().fold(f32::MIN, |a, &b| a.max(b));
    let g: [f32; NB] = std::array::from_fn(|i| f[i].max(top - 40.0));
    let level = g.iter().sum::<f32>() / NB as f32;
    (std::array::from_fn(|i| g[i] - level), level)
}

impl Profile {
    /// Mean squared z-score of the frame's shape: ~1 for a typical frame of this sound, large
    /// for others, INFINITY when it's quieter than the sound ever was. `gain_db` is the input
    /// gain the frame went through.
    fn dist(&self, f: &[f32; NB], gain_db: f32) -> f32 {
        let (s, level) = shape(f);
        match self.floor_raw_db {
            Some(floor) if level - gain_db >= floor => (0..NB).map(|i| (s[i] - self.mean[i]).powi(2) / self.var[i]).sum::<f32>() / NB as f32,
            _ => f32::INFINITY,
        }
    }

    /// Learned before 0.6.2: it never matches, and the app asks for it to be learned again.
    pub fn outdated(&self) -> bool {
        self.floor_raw_db.is_none()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct NoiseProfile {
    pub name: String,
    pub enabled: bool,
    pub profile: Profile,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profiles {
    pub noises: Vec<NoiseProfile>,
    pub keep: Option<Profile>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Match {
    Noise(usize),
    Keep,
}

impl Profiles {
    /// Floors saved by 0.6.2 to 0.8 were after input gain; they become floors before it,
    /// taking `gain_db` (the gain in the same save) as the gain they were learned at.
    pub fn migrate_floors(&mut self, gain_db: f32) {
        for p in self.noises.iter_mut().map(|n| &mut n.profile).chain(self.keep.as_mut()) {
            if let Some(floor) = p.floor_db.take()
                && p.floor_raw_db.is_none()
            {
                p.floor_raw_db = Some(floor - gain_db);
            }
        }
    }

    // ponytail: nearest-centroid classifier; swap for a small trained net if it misfires a lot.
    /// Also writes each sound's distance into `m` for the match bars (INFINITY = off or none).
    fn classify(&self, f: &[f32; NB], gain_db: f32, max_dist: f32, m: &mut Meters) -> Option<Match> {
        m.match_dist = [f32::INFINITY; MATCH_BARS];
        let mut noise = None::<(usize, f32)>;
        for (i, n) in self.noises.iter().enumerate().filter(|(_, n)| n.enabled) {
            let d = n.profile.dist(f, gain_db);
            if let Some(slot) = m.match_dist.get_mut(i) {
                *slot = d;
            }
            if d < max_dist && noise.is_none_or(|(_, best)| d < best) {
                noise = Some((i, d));
            }
        }
        m.keep_dist = self.keep.as_ref().map_or(f32::INFINITY, |p| p.dist(f, gain_db));
        let keep = Some(m.keep_dist).filter(|&d| d < max_dist);
        match (noise, keep) {
            (Some((i, dn)), Some(dk)) => Some(if dk <= dn { Match::Keep } else { Match::Noise(i) }),
            (Some((i, _)), None) => Some(Match::Noise(i)),
            (None, Some(_)) => Some(Match::Keep),
            (None, None) => None,
        }
    }
}

/// Peak-hold spectrum in dBFS, SPEC_BINS log-spaced bins.
#[derive(Clone, Copy)]
pub struct Spec(pub [f32; SPEC_BINS]);

impl Default for Spec {
    fn default() -> Self {
        Self([-120.0; SPEC_BINS])
    }
}

#[derive(Clone, Default)]
pub struct Meters {
    /// Processed frame count; the GUI watches it to spot a dead stream.
    pub frames: u64,
    pub in_db: f32,
    pub out_db: f32,
    pub gr_db: f32,
    /// Peak level going into the compressor, for its graph.
    pub comp_in_db: f32,
    /// RMS level the gate judged, and the gain it applied, for its graph.
    pub gate_level_db: f32,
    pub gate_gain_db: f32,
    pub agc_db: f32,
    pub deess_db: f32,
    pub vad: f32,
    pub pitch_hz: Option<f32>,
    pub gate_open: bool,
    pub reason: &'static str,
    /// Index into Profiles::noises of the learned noise being muted.
    pub noise_match: Option<usize>,
    pub learning: Option<(Kind, f32)>,
    pub spec_in: Spec,
    pub spec_out: Spec,
    /// After noise removal, before the gate. Only updated while `Chain::viz` is set.
    pub spec_clean: Spec,
    /// How much noise removal took out of the last frame, smoothed.
    pub denoise_db: f32,
    /// De-esser detector (energy above 5 kHz), compared against its threshold.
    pub deess_level_db: f32,
    /// Limiter gain reduction over the last frame.
    pub lim_db: f32,
    /// Distance from each learned sound (first MATCH_BARS) and from the learned voice.
    pub match_dist: [f32; MATCH_BARS],
    pub keep_dist: f32,
    /// Echo canceller: speaker level it hears, echo it removes (ERLE) and the delay it found.
    /// Written by audio.rs, which owns the canceller.
    pub echo_ref_db: f32,
    pub echo_erle_db: Option<f32>,
    pub echo_delay_ms: Option<i32>,
    /// Time spent on one 10 ms frame (echo cancellation and the whole chain) as a share of
    /// those 10 ms, smoothed over about half a second. Written by audio.rs.
    pub dsp_load: f32,
    /// The input's mean power in the NB analysis bands (after input gain), for Smart setup.
    pub bands_db: [f32; NB],
}

/// Learned sounds that get a live match bar. More can be learned; they just have no bar.
pub const MATCH_BARS: usize = 16;

pub struct Chain {
    rnn: Box<DenoiseState<'static>>,
    dry: [f32; FRAME], // raw frame delayed by RNNoise's one-frame latency
    mix: f32,
    pitch: Pitch,
    an_in: Analyzer,
    an_out: Analyzer,
    an_clean: Analyzer,
    /// The window is on screen: compute the display-only spectra. Hidden in the tray, the
    /// FFTs left are the input bands (Smart setup) and the cleaned ones (learned sounds).
    pub viz: bool,
    learner: Option<Learner>,
    /// Last frame's learned-sound match.
    class: Option<Match>,
    /// A learn run that has all its frames, for the GUI to finish.
    learned: Option<Learner>,
    hold: u32,
    gate_gain: f32,
    agc_db: f32,
    eq: Eq,
    deess: DeEsser,
    gr_db: f32,
    lim_env: f32,
    pub meters: Meters,
}

impl Default for Chain {
    fn default() -> Self {
        Self {
            rnn: DenoiseState::new(),
            dry: [0.0; FRAME],
            mix: 1.0,
            pitch: Pitch::default(),
            an_in: Analyzer::new(),
            an_out: Analyzer::new(),
            an_clean: Analyzer::new(),
            viz: false,
            learner: None,
            class: None,
            learned: None,
            hold: 0,
            gate_gain: 1.0,
            agc_db: 0.0,
            eq: Eq::default(),
            deess: DeEsser::default(),
            gr_db: 0.0,
            lim_env: 0.0,
            meters: Meters::default(),
        }
    }
}

/// Runs a recorded clip through the chain, for the record-and-compare player. It goes through
/// twice and the second pass is kept: live, auto level and noise removal have long settled,
/// and a cold chain would fade the first seconds in.
pub fn render(raw: &[f32], p: &Params, prof: &Profiles) -> Vec<f32> {
    let mut chain = Chain::default();
    let mut out = Vec::with_capacity(raw.len());
    for pass in 0..2 {
        out.clear();
        for c in raw.chunks(FRAME) {
            let mut f = [0.0; FRAME];
            f[..c.len()].copy_from_slice(c);
            chain.process(p, prof, &mut f);
            if pass == 1 {
                out.extend_from_slice(&f[..c.len()]);
            }
        }
    }
    out
}

/// One band's magnitude response in dB at each frequency, for drawing.
pub fn band_response(b: &Band, freqs: &[f32]) -> Vec<f32> {
    let mut f = Biquad::default();
    f.set(b.kind, b.freq, b.q, b.gain_db);
    freqs.iter().map(|&x| f.gain_db(x)).collect()
}

/// Whole-EQ magnitude response in dB at each frequency, for drawing the curve.
pub fn eq_curve(p: &Params, freqs: &[f32]) -> Vec<f32> {
    let mut sum = vec![0.0; freqs.len()];
    for b in p.bands.iter().flatten().filter(|b| b.on) {
        sum.iter_mut().zip(band_response(b, freqs)).for_each(|(s, g)| *s += g);
    }
    sum
}

/// Compressor output level for a steady input level, both dBFS, makeup included.
pub fn comp_curve_db(p: &Params, in_db: f32) -> f32 {
    in_db - comp_gr(in_db - p.comp_threshold_db, comp_slope(p)) + p.comp_makeup_db
}

fn comp_slope(p: &Params) -> f32 {
    1.0 - 1.0 / p.comp_ratio.max(1.0)
}

/// Gain reduction in dB for a level `over` dB above the threshold, with a 6 dB soft knee.
fn comp_gr(over: f32, slope: f32) -> f32 {
    const KNEE: f32 = 6.0;
    if over <= -KNEE / 2.0 {
        0.0
    } else if over >= KNEE / 2.0 {
        over * slope
    } else {
        slope * (over + KNEE / 2.0).powi(2) / (2.0 * KNEE)
    }
}

impl Chain {
    /// Starts a learn run, made (and allocated) by the GUI.
    pub fn start_learning(&mut self, l: Learner) {
        self.learner = Some(l);
    }

    /// A learn run with all its frames; `Learner::finish` turns it into a profile, off the
    /// audio thread.
    pub fn take_learned(&mut self) -> Option<Learner> {
        self.learned.take()
    }

    /// Auto level's gain, so a new chain (audio restarted) picks up where the last one was.
    pub fn set_agc(&mut self, db: f32) {
        self.agc_db = if db.is_finite() { db.clamp(-12.0, 30.0) } else { 0.0 };
    }

    fn learn_step(&mut self, f: &[f32; NB]) {
        if let Some(l) = &mut self.learner {
            l.add(f);
            self.meters.learning = Some((l.kind, l.progress()));
            if l.left == 0 {
                self.learned = self.learner.take();
                self.meters.learning = None;
            }
        }
    }

    pub fn process(&mut self, p: &Params, prof: &Profiles, x: &mut [f32]) {
        assert_eq!(x.len(), FRAME);
        self.meters.frames += 1;
        let g = db2lin(p.input_gain_db);
        x.iter_mut().for_each(|s| *s *= g);
        self.meters.in_db = peak_db(x);

        self.an_in.push(x);
        if self.viz {
            self.an_in.display(&mut self.meters.spec_in);
        }
        self.meters.bands_db = self.an_in.features();
        let f0 = self.pitch.push(x);
        self.meters.pitch_hz = f0;

        // RNNoise always runs: its voice detector drives the gate with noise removal off too,
        // and its one-frame delay never comes and goes, so switching noise removal or Bypass
        // can't replay a stale frame or skip one. Off just mixes in none of its output.
        let mut inp = [0.0; FRAME];
        let mut wet = [0.0; FRAME];
        for (i, s) in x.iter().enumerate() {
            inp[i] = s * 32768.0; // RNNoise wants i16-scaled floats
        }
        let vad = self.rnn.process_frame(&mut wet, &inp);
        // Pitch decides before the denoised frame comes out, so this is a free 10 ms lookahead.
        // Learned sounds are matched after noise removal, so it goes by last frame's match.
        let singing = self.pitch.track(f0, vad >= p.vad_threshold);
        let protect = if singing || self.class == Some(Match::Keep) { p.sing_protect } else { 0.0 };
        let target = if p.denoise && !p.bypass { p.denoise_strength * (1.0 - protect) } else { 0.0 };
        // Energy in vs out over the same (delayed) frame: what noise removal took away.
        let (mut e_in, mut e_out) = (1e-12f32, 1e-12f32);
        for (i, s) in x.iter_mut().enumerate() {
            let m = self.mix + (target - self.mix) * i as f32 / FRAME as f32;
            let dry = std::mem::replace(&mut self.dry[i], *s);
            *s = dry * (1.0 - m) + wet[i] / 32768.0 * m;
            e_in += dry * dry;
            e_out += *s * *s;
        }
        self.mix = target;
        let cut = (10.0 * (e_in / e_out).log10()).max(0.0);
        self.meters.denoise_db += (cut - self.meters.denoise_db) * 0.1;
        self.meters.vad = vad;

        // Learned sounds are learned and told apart after noise removal: the room it takes out
        // is neither part of a sound nor mistaken for one.
        self.an_clean.push(x);
        if self.viz && p.denoise {
            self.an_clean.display(&mut self.meters.spec_clean);
        }
        let clean = self.an_clean.features();
        self.learn_step(&clean);

        if p.bypass {
            // Your mic at your input gain, nothing cleaned, but still through the clip guard:
            // a quiet mic turned up 30 dB mustn't clip hard into Discord.
            (self.class, self.meters.noise_match, self.hold) = (None, None, 0);
            (self.meters.gate_open, self.meters.reason) = (true, "bypassed");
            (self.meters.gr_db, self.meters.deess_db) = (0.0, 0.0);
            self.limit(x, 0.0);
            return;
        }

        let class = prof.classify(&clean, p.input_gain_db, p.learn_match, &mut self.meters);
        self.class = class;
        self.meters.noise_match = match class {
            Some(Match::Noise(i)) => Some(i),
            _ => None,
        };

        let singing = self.pitch.singing();
        let level = rms_db(x);
        // Hysteresis: a note fading out stays open until it's clearly below the threshold.
        // meters.gate_open still holds last frame's state here.
        let thresh = p.gate_threshold_db - if self.meters.gate_open { p.gate_hysteresis_db } else { 0.0 };
        let (want, reason) = match class {
            _ if level <= thresh => (false, "quiet"),
            Some(Match::Noise(_)) => (false, "learned noise"),
            Some(Match::Keep) => (true, "learned voice"),
            None if vad >= p.vad_threshold => (true, "voice"),
            None if singing => (true, "singing"),
            None => (false, "not voice"),
        };
        if want {
            self.hold = (p.gate_hold_ms / 10.0) as u32;
        } else if matches!(class, Some(Match::Noise(_))) && vad < p.vad_threshold {
            // A learned noise right after you stop talking (typing, say) doesn't ride the hold.
            self.hold = 0;
        } else {
            self.hold = self.hold.saturating_sub(1);
        }
        let open = want || self.hold > 0;
        self.meters.gate_open = open || !p.gate;
        self.meters.reason = if !p.gate { "gate off" } else if !want && open { "hold" } else { reason };
        let target = if self.meters.gate_open { 1.0 } else { db2lin(p.gate_floor_db) };
        let (open_c, close_c) = (coef(p.gate_attack_ms), coef(p.gate_release_ms));
        for s in x.iter_mut() {
            let c = if target > self.gate_gain { open_c } else { close_c };
            self.gate_gain = target + c * (self.gate_gain - target);
            *s *= self.gate_gain;
        }
        self.meters.gate_level_db = level;
        self.meters.gate_gain_db = 20.0 * self.gate_gain.max(1e-6).log10();

        // Auto level: own leveler rather than sonora's AGC2, because that one sits before the
        // denoiser and would pump the noise up. Adapts only on frames the gate opened for.
        if p.agc {
            let before = self.agc_db;
            if want {
                let err = p.agc_target_db - (level + self.agc_db);
                self.agc_db = (self.agc_db + (err * 0.01).clamp(-0.05, 0.05)).clamp(-12.0, p.agc_max_gain_db);
            }
            for (i, s) in x.iter_mut().enumerate() {
                *s *= db2lin(before + (self.agc_db - before) * i as f32 / FRAME as f32);
            }
        }
        // Off keeps its gain for later: holding "hear it off" mustn't make it climb again from 0.
        // ponytail: switching back on jumps straight to that gain; ramp it if anyone hears a step.
        self.meters.agc_db = if p.agc { self.agc_db } else { 0.0 };

        if p.eq {
            self.eq.update(&p.bands);
            self.eq.run(x);
        }

        self.meters.deess_db = 0.0;
        if p.deess {
            x.iter_mut().for_each(|s| *s = self.deess.run(*s, p.deess_threshold_db));
            self.meters.deess_db = self.deess.gr_db;
            self.meters.deess_level_db = 20.0 * self.deess.env.max(1e-6).log10();
        }

        self.meters.gr_db = 0.0;
        if p.comp {
            let (att, rel) = (coef(p.comp_attack_ms), coef(p.comp_release_ms));
            let slope = comp_slope(p);
            let mut peak = 1e-6f32;
            for s in x.iter_mut() {
                peak = peak.max(s.abs());
                let target = comp_gr(20.0 * s.abs().max(1e-6).log10() - p.comp_threshold_db, slope);
                let c = if target > self.gr_db { att } else { rel };
                self.gr_db = target + c * (self.gr_db - target);
                *s *= db2lin(p.comp_makeup_db - self.gr_db);
            }
            self.meters.gr_db = self.gr_db;
            self.meters.comp_in_db = 20.0 * peak.log10();
        }

        self.limit(x, p.output_gain_db);
    }

    /// Output gain, then the safety limiter: instant attack, so output never exceeds the ceiling.
    fn limit(&mut self, x: &mut [f32], gain_db: f32) {
        let out_g = db2lin(gain_db);
        let rel = coef(50.0);
        let mut worst = 1.0f32;
        for s in x.iter_mut() {
            *s *= out_g;
            self.lim_env = s.abs().max(self.lim_env * rel);
            if self.lim_env > LIMIT_CEIL {
                worst = worst.max(self.lim_env / LIMIT_CEIL);
                *s *= LIMIT_CEIL / self.lim_env;
            }
        }
        self.meters.lim_db = 20.0 * worst.log10();
        self.meters.out_db = peak_db(x);
        if self.viz {
            self.an_out.push(x);
            self.an_out.display(&mut self.meters.spec_out);
        }
    }
}

/// YIN pitch detector on a 40 ms history decimated to 16 kHz, plus a note tracker.
struct Pitch {
    hist: Vec<f32>,
    last_f0: Option<f32>,
    streak: u32,
    miss: u32,
    latched: bool,
}

impl Default for Pitch {
    fn default() -> Self {
        Self { hist: vec![0.0; 640], last_f0: None, streak: 0, miss: 0, latched: false }
    }
}

impl Pitch {
    fn push(&mut self, x: &[f32]) -> Option<f32> {
        // ponytail: 3-sample boxcar decimation; its aliasing doesn't matter for picking a period.
        self.hist.drain(..FRAME / 3);
        self.hist.extend(x.chunks_exact(3).map(|c| (c[0] + c[1] + c[2]) / 3.0));
        yin(&self.hist, 16_000.0)
    }

    /// A held note counts as singing once it has lasted 30 ms and RNNoise called it voice at
    /// least once since it started. That latch is what keeps fan hum and whine from passing:
    /// they are pitched too, but never start out as voice.
    fn track(&mut self, f0: Option<f32>, voice: bool) -> bool {
        match f0 {
            Some(f) => {
                // Octave jumps are YIN errors, not a new note.
                let same_note = self.last_f0.is_some_and(|l| {
                    let r = f / l;
                    [0.5, 1.0, 2.0].iter().any(|m| (r / m - 1.0).abs() < 0.12)
                });
                if !same_note {
                    self.streak = 0;
                    self.latched = false;
                }
                self.last_f0 = Some(f);
                self.miss = 0;
                self.streak += 1;
            }
            None => {
                self.miss += 1;
                if self.miss > 5 {
                    self.last_f0 = None;
                    self.streak = 0;
                    self.latched = false;
                }
            }
        }
        if voice && self.last_f0.is_some() {
            self.latched = true;
        }
        self.singing()
    }

    fn singing(&self) -> bool {
        self.miss == 0 && self.streak >= 3 && self.latched
    }
}

/// Returns the fundamental in Hz (70..1000) or None if the window isn't periodic.
fn yin(x: &[f32], sr: f32) -> Option<f32> {
    const W: usize = 320;
    const TMIN: usize = 16;
    const TMAX: usize = 228;
    const THRESH: f32 = 0.15;
    let x = &x[x.len() - (W + TMAX)..];
    let mut cmnd = [1.0f32; TMAX + 1];
    let mut sum = 0.0;
    for tau in 1..=TMAX {
        let d: f32 = (0..W).map(|j| (x[j] - x[j + tau]).powi(2)).sum();
        sum += d;
        if sum > 0.0 {
            cmnd[tau] = d * tau as f32 / sum;
        }
    }
    let mut tau = (TMIN..=TMAX).find(|&t| cmnd[t] < THRESH)?;
    while tau < TMAX && cmnd[tau + 1] < cmnd[tau] {
        tau += 1;
    }
    Some(sr / tau as f32)
}

const NFFT: usize = 2048;

/// Rolling 2048-point (43 ms) Hann FFT. Bin powers are scaled so a full-scale sine reads 0 dB.
struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    hist: Vec<f32>,
    win: Vec<f32>,
    buf: Vec<f32>,
    spec: Vec<Complex<f32>>,
    power: Vec<f32>,
}

impl Analyzer {
    fn new() -> Self {
        let fft = RealFftPlanner::new().plan_fft_forward(NFFT);
        let win = (0..NFFT)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / NFFT as f32).cos())
            .collect();
        let spec = fft.make_output_vec();
        Self { power: vec![0.0; spec.len()], spec, buf: fft.make_input_vec(), fft, hist: vec![0.0; NFFT], win }
    }

    fn push(&mut self, x: &[f32]) {
        self.hist.drain(..x.len());
        self.hist.extend_from_slice(x);
        for i in 0..NFFT {
            self.buf[i] = self.hist[i] * self.win[i];
        }
        self.fft.process(&mut self.buf, &mut self.spec).expect("fft sizes are fixed");
        let norm = (NFFT as f32 / 4.0).powi(2);
        for (p, c) in self.power.iter_mut().zip(&self.spec) {
            *p = c.norm_sqr() / norm;
        }
    }

    /// Mean (or peak) bin power between two frequencies, in dB.
    fn band_db(&self, lo: f32, hi: f32, peak: bool) -> f32 {
        let df = SR / NFFT as f32;
        let a = ((lo / df) as usize).min(self.power.len() - 1);
        let b = ((hi / df).ceil() as usize).clamp(a + 1, self.power.len());
        let bins = &self.power[a..b];
        let v = if peak {
            bins.iter().fold(0.0f32, |m, &p| m.max(p))
        } else {
            bins.iter().sum::<f32>() / bins.len() as f32
        };
        10.0 * (v + 1e-12).log10()
    }

    /// Classifier features: mean log power in 16 log-spaced bands, 100 Hz..12 kHz.
    fn features(&self) -> [f32; NB] {
        std::array::from_fn(|i| self.band_db(band_edge(i), band_edge(i + 1), false))
    }

    /// Updates a peak-hold display spectrum that falls at 60 dB/s.
    fn display(&self, spec: &mut Spec) {
        for (i, v) in spec.0.iter_mut().enumerate() {
            let lo = SPEC_LO * SPEC_SPAN.powf(i as f32 / SPEC_BINS as f32);
            let hi = SPEC_LO * SPEC_SPAN.powf((i + 1) as f32 / SPEC_BINS as f32);
            *v = self.band_db(lo, hi, true).max(*v - 0.6);
        }
    }
}

pub struct Learner {
    pub kind: Kind,
    total: u32,
    left: u32,
    /// Every frame's band levels, kept so the quiet room around the sound can be left out at
    /// the end. Allocated here, on the GUI thread.
    frames: Vec<[f32; NB]>,
    /// Input gain while learning, so the profile's floor is stored before it.
    gain_db: f32,
}

impl Learner {
    pub fn new(kind: Kind, seconds: f32, gain_db: f32) -> Self {
        let n = (seconds * 100.0) as u32;
        Self { kind, total: n, left: n, frames: Vec::with_capacity(n as usize), gain_db }
    }

    fn add(&mut self, f: &[f32; NB]) {
        self.left = self.left.saturating_sub(1);
        self.frames.push(*f);
    }

    fn progress(&self) -> f32 {
        1.0 - self.left as f32 / self.total as f32
    }

    /// The learned profile, or why there's none, in words for the app to show.
    pub fn finish(&self) -> Result<Profile, &'static str> {
        const QUIET: &str = "That clip was too quiet, nothing saved.";
        const SHORT: &str = "That sound was too short to learn. Keep it going, or make it a few times over.";
        let pct = |mut v: Vec<f32>, q: f32| {
            v.sort_by(f32::total_cmp);
            v[((v.len() - 1) as f32 * q) as usize]
        };
        let frames: Vec<([f32; NB], f32)> = self.frames.iter().map(shape).collect();
        if frames.is_empty() {
            return Err(QUIET);
        }
        // The sound is the louder half between the room (quietest 10%) and its loud parts. A
        // steady sound like a fan has no quiet part, so all of it counts. Its loud parts are
        // the loudest 1%, so a single bark or clap of 0.2 s still stands out from the room.
        let levels = || frames.iter().map(|f| f.1).collect::<Vec<f32>>();
        let (lo, hi, top) = (pct(levels(), 0.1), pct(levels(), 0.99), pct(levels(), 1.0));
        if hi - lo < 10.0 && top - lo >= 10.0 {
            // Under 1% of the clip: learning it would learn the room.
            return Err(SHORT);
        }
        let cut = if hi - lo < 10.0 { f32::NEG_INFINITY } else { (lo + hi) / 2.0 };
        // Below -110 dB is digital silence, which teaches nothing.
        let kept: Vec<&([f32; NB], f32)> = frames.iter().filter(|f| f.1 >= cut && f.1 > -110.0).collect();
        if kept.len() < 20 {
            return Err(if cut.is_finite() { SHORT } else { QUIET });
        }
        let n = kept.len() as f32;
        let mean: [f32; NB] = std::array::from_fn(|i| kept.iter().map(|f| f.0[i]).sum::<f32>() / n);
        // 4 dB^2 floor so a perfectly steady hum doesn't make the match razor thin.
        let var = std::array::from_fn(|i| (kept.iter().map(|f| (f.0[i] - mean[i]).powi(2)).sum::<f32>() / n).max(4.0));
        // 6 dB under its quietest tenth, so it still matches as it fades, but never down in the
        // room it was left out of.
        let floor = (pct(kept.iter().map(|f| f.1).collect(), 0.1) - 6.0).max(cut);
        Ok(Profile { mean, var, floor_raw_db: Some(floor - self.gain_db), floor_db: None })
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    /// RBJ cookbook coefficients. Keeps filter state so slider moves don't click.
    fn set(&mut self, shape: BandKind, f: f32, q: f32, gain_db: f32) {
        // Just under Nyquist: the bilinear transform has nothing left above it.
        let w = 2.0 * std::f32::consts::PI * f.min(SR * 0.49) / SR;
        let (sn, cs) = w.sin_cos();
        let al = sn / (2.0 * q.max(0.05));
        let a = 10f32.powf(gain_db / 40.0);
        let sa = 2.0 * a.sqrt() * al;
        let (b0, b1, b2, a0, a1, a2) = match shape {
            BandKind::LowCut => ((1.0 + cs) / 2.0, -(1.0 + cs), (1.0 + cs) / 2.0, 1.0 + al, -2.0 * cs, 1.0 - al),
            BandKind::HighCut => ((1.0 - cs) / 2.0, 1.0 - cs, (1.0 - cs) / 2.0, 1.0 + al, -2.0 * cs, 1.0 - al),
            BandKind::Notch => (1.0, -2.0 * cs, 1.0, 1.0 + al, -2.0 * cs, 1.0 - al),
            BandKind::Bell => (1.0 + al * a, -2.0 * cs, 1.0 - al * a, 1.0 + al / a, -2.0 * cs, 1.0 - al / a),
            BandKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cs + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
                a * ((a + 1.0) - (a - 1.0) * cs - sa),
                (a + 1.0) + (a - 1.0) * cs + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cs),
                (a + 1.0) + (a - 1.0) * cs - sa,
            ),
            BandKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cs + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
                a * ((a + 1.0) + (a - 1.0) * cs - sa),
                (a + 1.0) - (a - 1.0) * cs + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cs),
                (a + 1.0) - (a - 1.0) * cs - sa,
            ),
        };
        self.b = [b0 / a0, b1 / a0, b2 / a0];
        self.a = [a1 / a0, a2 / a0];
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    fn gain_db(&self, f: f32) -> f32 {
        let z1 = Complex::from_polar(1.0, -2.0 * std::f32::consts::PI * f / SR);
        let z2 = z1 * z1;
        let num = self.b[0] + z1 * self.b[1] + z2 * self.b[2];
        let den = Complex::new(1.0, 0.0) + z1 * self.a[0] + z2 * self.a[1];
        20.0 * (num.norm() / den.norm()).log10()
    }
}

/// Up to BANDS parametric bands in series. Unused and switched-off bands cost nothing.
#[derive(Default)]
struct Eq {
    f: [Biquad; BANDS],
    key: [Option<Band>; BANDS],
}

impl Eq {
    /// Coefficients are only recomputed when a band changes, not every frame.
    fn update(&mut self, bands: &[Option<Band>; BANDS]) {
        if *bands == self.key {
            return;
        }
        for (i, b) in bands.iter().enumerate() {
            let Some(b) = b.filter(|b| b.on) else { continue };
            if !self.key[i].is_some_and(|o| o.on) {
                self.f[i].z = [0.0; 2]; // new band: don't replay a removed one's state
            }
            self.f[i].set(b.kind, b.freq, b.q, b.gain_db);
        }
        self.key = *bands;
    }

    /// Band by band over the frame: one branch per band instead of per sample.
    fn run(&mut self, x: &mut [f32]) {
        for (f, b) in self.f.iter_mut().zip(&self.key) {
            if b.is_some_and(|b| b.on) {
                x.iter_mut().for_each(|s| *s = f.run(*s));
            }
        }
    }
}

/// Dynamic high shelf: energy above 5 kHz over the threshold pulls a 4.5 kHz shelf down,
/// 4:1 up to 12 dB. (Splitting into x - HP(x) and HP(x) doesn't work: the HP's phase shift
/// leaves most of an 8 kHz "s" in the "low" part.)
struct DeEsser {
    det: Biquad,
    shelf: Biquad,
    env: f32,
    gr_db: f32,
    shelf_db: f32,
    n: u32,
}

impl Default for DeEsser {
    fn default() -> Self {
        let mut det = Biquad::default();
        det.set(BandKind::LowCut, 5000.0, 0.707, 0.0); // high-pass: only the "s" range
        let mut shelf = Biquad::default();
        shelf.set(BandKind::HighShelf, 4500.0, 0.707, 0.0);
        Self { det, shelf, env: 0.0, gr_db: 0.0, shelf_db: 0.0, n: 0 }
    }
}

impl DeEsser {
    fn run(&mut self, x: f32, thresh_db: f32) -> f32 {
        let hi = self.det.run(x).abs();
        let c = if hi > self.env { coef(1.0) } else { coef(60.0) };
        self.env = hi + c * (self.env - hi);
        self.gr_db = ((20.0 * self.env.max(1e-6).log10() - thresh_db) * 0.75).clamp(0.0, 12.0);
        // Recomputing coefficients every 32 samples (0.7 ms) is smooth enough and cheap.
        self.n = self.n.wrapping_add(1);
        if self.n % 32 == 0 && (self.gr_db - self.shelf_db).abs() > 0.05 {
            self.shelf_db = self.gr_db;
            self.shelf.set(BandKind::HighShelf, 4500.0, 0.707, -self.shelf_db);
        }
        self.shelf.run(x)
    }
}

/// One-pole smoothing coefficient for a time constant in ms.
fn coef(ms: f32) -> f32 {
    (-1.0 / (ms.max(0.1) * 0.001 * SR)).exp()
}

pub fn db2lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn peak_db(x: &[f32]) -> f32 {
    20.0 * x.iter().fold(1e-6f32, |m, s| m.max(s.abs())).log10()
}

fn rms_db(x: &[f32]) -> f32 {
    let ms = x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32;
    10.0 * (ms + 1e-12).log10()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(f0: f32, n: usize, amp: f32) -> Vec<f32> {
        // Five harmonics with 5 Hz vibrato: a crude held sung vowel.
        (0..n)
            .map(|i| {
                let t = i as f32 / SR;
                let ph = TAU * f0 * t + 0.03 * f0 / 5.0 * (TAU * 5.0 * t).sin();
                amp * (1..=5).map(|h| (ph * h as f32).sin() / h as f32).sum::<f32>() / 2.3
            })
            .collect()
    }

    fn sine(f: f32, n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| (TAU * f * i as f32 / SR).sin() * amp).collect()
    }

    fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 17;
                *seed ^= *seed << 5;
                amp * (*seed as f32 / u32::MAX as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    /// Only the named stage on, gate open, so a test sees one effect at a time. Every frame
    /// counts as voice: test tones don't sound like it to RNNoise.
    fn only(stage: &str) -> Params {
        Params {
            denoise: false,
            gate: false,
            vad_threshold: 0.0,
            agc: stage == "agc",
            eq: stage == "eq",
            deess: stage == "deess",
            comp: stage == "comp",
            ..Params::default()
        }
    }

    fn run(p: &Params, prof: &Profiles, x: &[f32]) -> (Vec<f32>, Chain) {
        let mut chain = Chain { viz: true, ..Chain::default() };
        let mut y = Vec::new();
        for c in x.chunks(FRAME) {
            let mut f = c.to_vec();
            chain.process(p, prof, &mut f);
            y.extend(f);
        }
        (y, chain)
    }

    #[test]
    fn yin_finds_220() {
        let x = tone(220.0, 2 * FRAME * 3, 0.5);
        let mut p = Pitch::default();
        let mut f = None;
        for c in x.chunks(FRAME) {
            f = p.push(c);
        }
        let f = f.expect("voiced");
        assert!((f / 220.0 - 1.0).abs() < 0.03, "got {f}");
    }

    #[test]
    fn singing_needs_a_voiced_start() {
        let mut hum = Pitch::default();
        for _ in 0..50 {
            hum.track(Some(120.0), false);
        }
        assert!(!hum.singing(), "fan hum counted as singing");

        let mut note = Pitch::default();
        note.track(Some(260.0), true);
        for _ in 0..300 {
            note.track(Some(261.0), false); // RNNoise lost it, the note goes on for 3 s
        }
        assert!(note.singing(), "held note dropped");
    }

    #[test]
    fn rnnoise_latency_is_one_frame() {
        let x = tone(220.0, FRAME * 40, 0.3);
        let mut rnn = DenoiseState::new();
        let mut y = Vec::new();
        for c in x.chunks(FRAME) {
            let inp: Vec<f32> = c.iter().map(|s| s * 32768.0).collect();
            let mut out = [0.0; FRAME];
            rnn.process_frame(&mut out, &inp);
            y.extend_from_slice(&out);
        }
        let corr = |lag: usize| (FRAME * 10..FRAME * 38).map(|i| x[i] * y[i + lag]).sum::<f32>();
        let best = (0..FRAME * 2).max_by(|&a, &b| corr(a).total_cmp(&corr(b))).unwrap();
        // The tone repeats every ~218 samples, so accept the nearest lag to 480 within half a period.
        assert!((best as i32 - FRAME as i32).abs() < 100, "lag {best}");
    }

    #[test]
    fn held_note_stays_open_and_noise_alone_is_gated() {
        let mut seed = 1;
        // 3 s of a held note over -40 dB noise, then 1.5 s of the noise alone.
        let mut x = tone(260.0, (SR * 3.0) as usize, 0.3);
        x.extend(vec![0.0; (SR * 1.5) as usize]);
        let n = noise(x.len(), 0.01, &mut seed);
        x.iter_mut().zip(n).for_each(|(s, n)| *s += n);

        // Gate behaviour only: auto level would (rightly) pull the note toward its target.
        let p = Params { comp: false, eq: false, agc: false, deess: false, ..Params::default() };
        let (y, _) = run(&p, &Profiles::default(), &x);
        let seg = |a: f32, b: f32| rms_db(&y[(SR * a) as usize..(SR * b) as usize]);
        let (note, tail) = (seg(1.0, 2.9), seg(3.8, 4.4));
        assert!(note > -20.0, "held note was cut: {note} dB");
        assert!(tail < -70.0, "noise not gated: {tail} dB");
    }

    #[test]
    fn learned_noise_closes_gate_and_names_it() {
        let mut seed = 7;
        let p = Params { gate: true, ..only("none") };
        let mut chain = Chain::default();
        let hiss = noise(FRAME * 300, 0.1, &mut seed);
        chain.start_learning(Learner::new(Kind::Noise, 2.0, 0.0));
        for c in hiss.chunks(FRAME).take(200) {
            chain.process(&p, &Profiles::default(), &mut c.to_vec());
        }
        let (kind, prof) = chain.take_learned().map(|l| (l.kind, l.finish())).expect("finished");
        assert_eq!(kind, Kind::Noise);
        // A sound nothing like hiss: all lows, no highs.
        let other = Profile { mean: std::array::from_fn(|i| if i < NB / 2 { 15.0 } else { -15.0 }), var: [4.0; NB], floor_raw_db: Some(-200.0), floor_db: None };
        let noises = vec![
            NoiseProfile { name: "dog".into(), enabled: true, profile: other },
            NoiseProfile { name: "fan".into(), enabled: true, profile: prof.expect("loud enough") },
        ];
        let mut profiles = Profiles { noises, keep: None };
        for c in hiss.chunks(FRAME).skip(200) {
            chain.process(&p, &profiles, &mut c.to_vec());
        }
        assert_eq!(chain.meters.reason, "learned noise");
        assert_eq!(chain.meters.noise_match, Some(1));
        let d = chain.meters.match_dist;
        assert!(d[1] < p.learn_match && d[0] > p.learn_match, "match bars {:?}", &d[..2]);
        assert!(d[2].is_infinite() && chain.meters.keep_dist.is_infinite(), "unused bars should be empty");
        assert!(!chain.meters.gate_open);

        profiles.noises[1].enabled = false;
        chain.process(&p, &profiles, &mut hiss[..FRAME].to_vec());
        assert_eq!(chain.meters.noise_match, None, "disabled profile still matched");
    }

    #[test]
    fn a_learned_sound_matches_itself_but_not_the_quiet_room_around_it() {
        // What "stay quiet, then make the sound" records: 1 s of a quiet room, then a hum that
        // starts and stops every 0.3 s over it.
        let mut seed = 3;
        let p = Params { gate: true, ..only("none") };
        let n = (SR * 5.0) as usize;
        let hum = tone(150.0, n, 0.3);
        let on = |i: usize| i >= SR as usize && (i - SR as usize) / (SR * 0.3) as usize % 2 == 0;
        let mut clip = noise(n, 0.003, &mut seed);
        clip.iter_mut().enumerate().filter(|(i, _)| on(*i)).for_each(|(i, s)| *s += hum[i]);
        let mut chain = Chain::default();
        chain.start_learning(Learner::new(Kind::Noise, 5.0, 0.0));
        for c in clip.chunks(FRAME) {
            chain.process(&p, &Profiles::default(), &mut c.to_vec());
        }
        let (_, prof) = chain.take_learned().map(|l| (l.kind, l.finish())).expect("finished");
        let profile = prof.expect("loud enough");
        let profiles = Profiles { noises: vec![NoiseProfile { name: "hum".into(), enabled: true, profile }], keep: None };

        // Share of frames that count as the hum.
        let mut matched = |x: &[f32]| {
            let frames = x.chunks(FRAME).filter(|c| {
                chain.process(&p, &profiles, &mut c.to_vec());
                chain.meters.noise_match.is_some()
            });
            frames.count() as f32 / (x.len() / FRAME) as f32
        };
        let room = matched(&noise(SR as usize, 0.003, &mut seed));
        let louder = matched(&tone(150.0, SR as usize, 0.6));
        assert!(room < 0.05, "the quiet room counts as the hum {:.0}% of the time", room * 100.0);
        assert!(louder > 0.9, "the hum itself, 6 dB louder, matched only {:.0}% of the time", louder * 100.0);
    }

    #[test]
    fn a_learned_sound_still_matches_after_the_input_gain_changes() {
        let p = Params { gate: true, ..only("none") };
        let hum = tone(150.0, SR as usize * 2, 0.1);
        let mut chain = Chain::default();
        chain.start_learning(Learner::new(Kind::Noise, 2.0, p.input_gain_db));
        for c in hum.chunks(FRAME) {
            chain.process(&p, &Profiles::default(), &mut c.to_vec());
        }
        let profile = chain.take_learned().expect("finished").finish().expect("learned");
        let profiles = Profiles { noises: vec![NoiseProfile { name: "hum".into(), enabled: true, profile }], keep: None };
        // Smart setup turned the gain down 14 dB: the same hum, quieter after the gain.
        let quieter = Params { input_gain_db: -14.0, ..p.clone() };
        let mut chain = Chain::default();
        let hits = hum.chunks(FRAME).filter(|c| {
            chain.process(&quieter, &profiles, &mut c.to_vec());
            chain.meters.noise_match.is_some()
        });
        assert!(hits.count() > 180, "the hum stopped matching after a gain change");
    }

    #[test]
    fn old_floors_after_gain_become_floors_before_it() {
        let old = Profile { mean: [0.0; NB], var: [4.0; NB], floor_raw_db: None, floor_db: Some(-60.0) };
        let mut profiles = Profiles { noises: vec![], keep: Some(old) };
        profiles.migrate_floors(10.0);
        let p = profiles.keep.unwrap();
        assert_eq!((p.floor_raw_db, p.floor_db), (Some(-70.0), None));
    }

    #[test]
    fn a_brief_sound_learns_the_sound_or_says_too_short_never_the_room() {
        let mut seed = 4;
        let p = Params { gate: true, ..only("none") };
        let n = (SR * 5.0) as usize;
        // 5 s of a quiet room with one burst of hum `secs` long in the middle.
        let mut learn = |secs: f32| {
            let mut clip = noise(n, 0.003, &mut seed);
            let start = (SR * 2.0) as usize;
            let hum = tone(150.0, (SR * secs) as usize, 0.3);
            clip[start..start + hum.len()].iter_mut().zip(&hum).for_each(|(s, h)| *s += h);
            let mut chain = Chain::default();
            chain.start_learning(Learner::new(Kind::Noise, 5.0, 0.0));
            for c in clip.chunks(FRAME) {
                chain.process(&p, &Profiles::default(), &mut c.to_vec());
            }
            chain.take_learned().map(|l| (l.kind, l.finish())).expect("finished").1
        };
        let profile = learn(0.25).expect("a quarter second is enough");
        let profiles = Profiles { noises: vec![NoiseProfile { name: "bark".into(), enabled: true, profile }], keep: None };
        let mut chain = Chain::default();
        let room = noise(SR as usize, 0.003, &mut 77);
        let hits = room.chunks(FRAME).filter(|c| {
            chain.process(&p, &profiles, &mut c.to_vec());
            chain.meters.noise_match.is_some()
        });
        assert!(hits.count() < 5, "the room counts as the burst");
        assert!(learn(0.04).unwrap_err().contains("too short"));
    }

    #[test]
    fn a_learned_noise_right_after_you_stop_does_not_ride_the_hold() {
        let mut seed = 9;
        let p = Params { gate: true, gate_hold_ms: 400.0, vad_threshold: 0.99, ..only("none") };
        let learn = |x: &[f32]| {
            let mut c = Chain::default();
            c.start_learning(Learner::new(Kind::Noise, x.len() as f32 / SR, 0.0));
            for f in x.chunks(FRAME) {
                c.process(&p, &Profiles::default(), &mut f.to_vec());
            }
            c.take_learned().map(|l| (l.kind, l.finish())).expect("finished").1.expect("learned")
        };
        let (hiss, note) = (noise(FRAME * 200, 0.1, &mut seed), tone(200.0, FRAME * 200, 0.3));
        let keys = NoiseProfile { name: "keys".into(), enabled: true, profile: learn(&hiss) };
        let profiles = Profiles { noises: vec![keys], keep: Some(learn(&note)) };
        let mut chain = Chain::default();
        for f in note.chunks(FRAME) {
            chain.process(&p, &profiles, &mut f.to_vec());
        }
        assert!(chain.meters.gate_open, "the learned voice didn't open it: {}", chain.meters.reason);
        // Without the fix all 40 frames (the 400 ms hold) pass. The analysis window still
        // holds some of the note for the first few.
        let open = hiss.chunks(FRAME).take(40).filter(|f| {
            chain.process(&p, &profiles, &mut f.to_vec());
            chain.meters.gate_open
        });
        assert!(open.count() < 10);
    }

    #[test]
    fn bypass_keeps_the_clip_guard_and_switching_never_replays_old_sound() {
        // A quiet mic turned up 30 dB, then a shout.
        let p = Params { bypass: true, input_gain_db: 30.0, ..Params::default() };
        let (y, chain) = run(&p, &Profiles::default(), &sine(300.0, FRAME * 20, 0.5));
        assert!(y.iter().all(|s| s.abs() <= LIMIT_CEIL + 1e-4), "Bypass clipped");
        assert!(chain.meters.gate_open && chain.meters.reason == "bypassed");

        // Talk with noise removal on, switch it off through a pause, then on again in silence:
        // nothing from before the pause may come out.
        let (on, off) = (Params { denoise: true, ..only("none") }, only("none"));
        let mut chain = Chain::default();
        let mut feed = |p: &Params, x: &[f32]| {
            let mut peak = 0.0f32;
            for c in x.chunks(FRAME) {
                let mut f = c.to_vec();
                chain.process(p, &Profiles::default(), &mut f);
                peak = f.iter().fold(peak, |m, s| m.max(s.abs()));
            }
            peak
        };
        feed(&on, &tone(220.0, FRAME * 50, 0.3));
        feed(&off, &tone(220.0, FRAME * 5, 0.3));
        feed(&off, &vec![0.0; FRAME * 2]);
        let back_on = feed(&on, &vec![0.0; FRAME * 2]);
        assert!(back_on < 1e-3, "an old frame came back at {:.1} dB", 20.0 * back_on.log10());
    }

    /// Learns your voice from a recording through the real chain (noise removal on): 2 s of a
    /// room, then the recording over it, the way people use "Learn your voice". Then counts
    /// how often the room alone and the talking match it:
    /// `MC_WAV=voice.wav cargo test learning_on_wav -- --ignored --nocapture` (16-bit, 48 kHz).
    #[test]
    #[ignore]
    fn learning_on_wav() {
        let bytes = std::fs::read(std::env::var("MC_WAV").expect("set MC_WAV")).unwrap();
        let ch = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
        let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
        let voice = bytes[data..].chunks_exact(2 * ch).map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0);
        let mut seed = 5;
        let mut clip: Vec<f32> = std::iter::repeat_n(0.0, 2 * SR as usize).chain(voice).collect();
        clip.truncate(clip.len() / FRAME * FRAME);
        let room = noise(clip.len(), 0.004, &mut seed);
        clip.iter_mut().zip(room).for_each(|(s, n)| *s += n);
        let p = Params::default();
        let mut chain = Chain::default();
        chain.start_learning(Learner::new(Kind::Keep, clip.len() as f32 / SR, 0.0));
        for c in clip.chunks(FRAME) {
            chain.process(&p, &Profiles::default(), &mut c.to_vec());
        }
        let keep = chain.take_learned().and_then(|l| l.finish().ok()).expect("learned");
        let profiles = Profiles { noises: vec![], keep: Some(keep) };
        let (mut room, mut talk) = ((0, 0), (0, 0));
        for (i, c) in clip.chunks(FRAME).enumerate() {
            chain.process(&p, &profiles, &mut c.to_vec());
            let hit = (chain.meters.keep_dist < p.learn_match) as u32;
            let slot = if i < 200 { &mut room } else if chain.meters.vad >= 0.5 { &mut talk } else { continue };
            *slot = (slot.0 + hit, slot.1 + 1);
        }
        let share = |(hit, all): (u32, u32)| 100.0 * hit as f32 / all.max(1) as f32;
        println!("room alone matched {:.0}% of {} frames, talking {:.0}% of {}", share(room), room.1, share(talk), talk.1);
        assert!(share(room) < 5.0 && share(talk) > 60.0);
    }

    #[test]
    fn compressor_hits_ratio() {
        let p = Params { comp_threshold_db: -20.0, comp_ratio: 4.0, comp_makeup_db: 0.0, ..only("comp") };
        let (y, _) = run(&p, &Profiles::default(), &sine(1000.0, FRAME * 100, 0.9));
        // -0.9 dB peak in, 19.1 dB over a -20 dB threshold at 4:1 -> about -15.2 dB out.
        let out = peak_db(&y[FRAME * 80..]);
        assert!((out + 15.2).abs() < 2.0, "got {out}");
    }

    #[test]
    fn flat_eq_is_transparent() {
        let band = |kind, freq| Some(Band { kind, freq, gain_db: 0.0, q: 0.707, on: true });
        // A 0 dB bell and a disabled band must not touch 1 kHz either.
        let off = Some(Band { kind: BandKind::Bell, freq: 1000.0, gain_db: 12.0, q: 1.0, on: false });
        let bands = [band(BandKind::LowCut, 20.0), band(BandKind::Bell, 1000.0), off, None, None];
        let mut eq = Eq::default();
        eq.update(&bands);
        let mut y = sine(1000.0, FRAME * 20, 0.5);
        let x = y.clone();
        eq.run(&mut y);
        assert!((rms_db(&y[FRAME * 10..]) - rms_db(&x[FRAME * 10..])).abs() < 0.1);
    }

    #[test]
    fn eq_curve_matches_settings() {
        let mut p = Params::default();
        p.bands[0] = Some(Band { kind: BandKind::LowCut, freq: 100.0, gain_db: 0.0, q: 0.707, on: true });
        p.bands[3] = Some(Band { kind: BandKind::Bell, freq: 2000.0, gain_db: 6.0, q: 1.0, on: true });
        // 450 Hz sits between the low cut and the Q=1 bell's skirt.
        let c = eq_curve(&p, &[100.0, 450.0, 2000.0]);
        assert!((c[0] + 3.0).abs() < 0.3, "low cut corner {}", c[0]);
        assert!(c[1].abs() < 0.5, "passband {}", c[1]);
        assert!((c[2] - 6.0).abs() < 0.2, "bell peak {}", c[2]);
    }

    #[test]
    fn band_kinds_have_their_shapes() {
        let at = |kind, f| {
            band_response(&Band { kind, freq: 1000.0, gain_db: 9.0, q: 0.707, on: true }, &[f])[0]
        };
        assert!((at(BandKind::HighCut, 1000.0) + 3.0).abs() < 0.3, "high cut corner");
        assert!(at(BandKind::HighCut, 8000.0) < -30.0, "high cut stopband");
        assert!(at(BandKind::Notch, 1000.0) < -40.0, "notch centre");
        assert!(at(BandKind::Notch, 100.0).abs() < 0.2, "notch away from centre");
        assert!((at(BandKind::LowShelf, 50.0) - 9.0).abs() < 0.3, "low shelf plateau");
        assert!((at(BandKind::HighShelf, 15_000.0) - 9.0).abs() < 0.5, "high shelf plateau");
    }

    #[test]
    fn gate_hysteresis_keeps_a_fading_note_open() {
        let p = Params { gate: true, gate_threshold_db: -40.0, gate_hysteresis_db: 6.0, ..only("none") };
        // Loud, then settling at -44 dB RMS: between the -46 close and -40 open thresholds.
        let mut x = sine(300.0, FRAME * 50, 0.3);
        x.extend(sine(300.0, FRAME * 50, 0.0089));
        let (_, chain) = run(&p, &Profiles::default(), &x);
        assert!(chain.meters.gate_open, "faded note closed: {}", chain.meters.reason);
        // The same level from cold never opens it.
        let (_, chain) = run(&p, &Profiles::default(), &sine(300.0, FRAME * 50, 0.0089));
        assert_eq!(chain.meters.reason, "quiet");
    }

    #[test]
    fn deesser_cuts_hiss_not_voice() {
        let p = Params { deess_threshold_db: -30.0, ..only("deess") };
        let s = sine(8000.0, FRAME * 50, 0.5);
        let (y, _) = run(&p, &Profiles::default(), &s);
        let cut = rms_db(&s[FRAME * 20..]) - rms_db(&y[FRAME * 20..]);
        assert!((9.0..=12.5).contains(&cut), "8 kHz cut {cut} dB");

        let v = sine(300.0, FRAME * 50, 0.5);
        let (y, _) = run(&p, &Profiles::default(), &v);
        let cut = rms_db(&v[FRAME * 20..]) - rms_db(&y[FRAME * 20..]);
        assert!(cut.abs() < 0.5, "300 Hz changed by {cut} dB");
    }

    #[test]
    fn auto_level_reaches_target() {
        let p = Params { agc_target_db: -22.0, ..only("agc") };
        // Sine RMS is peak - 3 dB, so 0.045 peak = -30 dB RMS: needs +8 dB.
        let (y, chain) = run(&p, &Profiles::default(), &sine(1000.0, FRAME * 500, 0.045));
        let out = rms_db(&y[FRAME * 400..]);
        assert!((out + 22.0).abs() < 1.0, "got {out} dB, gain {}", chain.meters.agc_db);
    }

    #[test]
    fn visual_meters_track_their_stages() {
        let mut seed = 3;
        // Noise removal on plain hiss takes a lot out.
        let p = Params { denoise: true, ..only("none") };
        let (_, chain) = run(&p, &Profiles::default(), &noise(FRAME * 100, 0.05, &mut seed));
        assert!(chain.meters.denoise_db > 6.0, "denoise {} dB", chain.meters.denoise_db);
        assert!(chain.meters.spec_clean.0.iter().any(|&d| d > -100.0), "clean spectrum not drawn");

        // +20 dB into the limiter: about 19 dB of limiting, nothing above the ceiling.
        let p = Params { output_gain_db: 20.0, ..only("none") };
        let (y, chain) = run(&p, &Profiles::default(), &sine(1000.0, FRAME * 20, 0.9));
        assert!((chain.meters.lim_db - 19.3).abs() < 1.0, "limiting {} dB", chain.meters.lim_db);
        assert!(y.iter().all(|s| s.abs() <= LIMIT_CEIL + 1e-4));

        // An "s" well over the de-esser threshold shows up on its detector.
        let p = Params { deess_threshold_db: -30.0, ..only("deess") };
        let (_, chain) = run(&p, &Profiles::default(), &sine(8000.0, FRAME * 20, 0.5));
        assert!(chain.meters.deess_level_db > -30.0, "detector {} dB", chain.meters.deess_level_db);
    }

    #[test]
    fn spectrum_peaks_at_the_tone() {
        let (_, chain) = run(&only("none"), &Profiles::default(), &sine(1000.0, FRAME * 10, 0.5));
        let s = chain.meters.spec_out.0;
        let peak = (0..SPEC_BINS).max_by(|&a, &b| s[a].total_cmp(&s[b])).unwrap();
        let lo = SPEC_LO * SPEC_SPAN.powf(peak as f32 / SPEC_BINS as f32);
        let hi = SPEC_LO * SPEC_SPAN.powf((peak + 1) as f32 / SPEC_BINS as f32);
        assert!(lo <= 1000.0 && 1000.0 <= hi * 1.05, "peak bin {lo}-{hi} Hz");
        assert!((s[peak] + 6.0).abs() < 1.5, "0.5 amplitude should read -6 dB, got {}", s[peak]);
    }
}
