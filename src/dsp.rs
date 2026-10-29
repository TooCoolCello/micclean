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

/// Diagonal-Gaussian model of log band energies for one learned sound.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub mean: [f32; NB],
    pub var: [f32; NB],
}

impl Profile {
    /// Mean squared z-score: ~1 for a typical frame of this sound, large for others.
    fn dist(&self, f: &[f32; NB]) -> f32 {
        (0..NB).map(|i| (f[i] - self.mean[i]).powi(2) / self.var[i]).sum::<f32>() / NB as f32
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
    // ponytail: nearest-centroid classifier; swap for a small trained net if it misfires a lot.
    /// Also writes each sound's distance into `m` for the match bars (INFINITY = off or none).
    fn classify(&self, f: &[f32; NB], max_dist: f32, m: &mut Meters) -> Option<Match> {
        m.match_dist = [f32::INFINITY; MATCH_BARS];
        let mut noise = None::<(usize, f32)>;
        for (i, n) in self.noises.iter().enumerate().filter(|(_, n)| n.enabled) {
            let d = n.profile.dist(f);
            if let Some(slot) = m.match_dist.get_mut(i) {
                *slot = d;
            }
            if d < max_dist && noise.is_none_or(|(_, best)| d < best) {
                noise = Some((i, d));
            }
        }
        m.keep_dist = self.keep.as_ref().map_or(f32::INFINITY, |p| p.dist(f));
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
    /// only FFT left is the one the learned-sound classifier needs.
    pub viz: bool,
    learner: Option<Learner>,
    learned: Option<(Kind, Option<Profile>)>,
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

/// Runs a recorded clip through a fresh chain, for the record-and-compare player.
pub fn render(raw: &[f32], p: &Params, prof: &Profiles) -> Vec<f32> {
    let mut chain = Chain::default();
    let mut out = Vec::with_capacity(raw.len());
    for c in raw.chunks(FRAME) {
        let mut f = [0.0; FRAME];
        f[..c.len()].copy_from_slice(c);
        chain.process(p, prof, &mut f);
        out.extend_from_slice(&f[..c.len()]);
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
    pub fn start_learning(&mut self, kind: Kind, seconds: f32) {
        self.learner = Some(Learner::new(kind, (seconds * 100.0) as u32));
    }

    /// Finished learn run: the profile, or None if the clip was too quiet to learn from.
    pub fn take_learned(&mut self) -> Option<(Kind, Option<Profile>)> {
        self.learned.take()
    }

    pub fn process(&mut self, p: &Params, prof: &Profiles, x: &mut [f32]) {
        assert_eq!(x.len(), FRAME);
        self.meters.frames += 1;
        let g = db2lin(p.input_gain_db);
        x.iter_mut().for_each(|s| *s *= g);
        self.meters.in_db = peak_db(x);

        // Analysis runs on the raw frame so learning still works while bypassed.
        self.an_in.push(x);
        if self.viz {
            self.an_in.display(&mut self.meters.spec_in);
        }
        let bands = self.an_in.features();
        self.meters.bands_db = bands;
        if let Some(l) = &mut self.learner {
            l.add(&bands, rms_db(x));
            self.meters.learning = Some((l.kind, l.progress()));
            if l.left == 0 {
                self.learned = Some((l.kind, l.finish()));
                self.learner = None;
                self.meters.learning = None;
            }
        }
        let f0 = self.pitch.push(x);
        let class = prof.classify(&bands, p.learn_match, &mut self.meters);
        self.meters.pitch_hz = f0;
        self.meters.noise_match = match class {
            Some(Match::Noise(i)) => Some(i),
            _ => None,
        };

        if p.bypass {
            self.meters.out_db = peak_db(x);
            self.meters.gr_db = 0.0;
            self.meters.reason = "bypassed";
            self.meters.spec_out = self.meters.spec_in;
            return;
        }

        let mut vad = 1.0;
        if p.denoise {
            let mut inp = [0.0; FRAME];
            let mut wet = [0.0; FRAME];
            for (i, s) in x.iter().enumerate() {
                inp[i] = s * 32768.0; // RNNoise wants i16-scaled floats
            }
            vad = self.rnn.process_frame(&mut wet, &inp);
            // Pitch decides before the denoised frame comes out, so this is a free 10 ms lookahead.
            let singing = self.pitch.track(f0, vad >= p.vad_threshold);
            let protect = if singing || class == Some(Match::Keep) { p.sing_protect } else { 0.0 };
            let target = p.denoise_strength * (1.0 - protect);
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
            if self.viz {
                self.an_clean.push(x);
                self.an_clean.display(&mut self.meters.spec_clean);
            }
        } else {
            self.pitch.track(f0, false);
            self.meters.denoise_db = 0.0;
        }
        self.meters.vad = vad;

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
        } else {
            self.agc_db = 0.0;
        }
        self.meters.agc_db = self.agc_db;

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

        // Safety limiter: instant attack, so output never exceeds the ceiling.
        let out_g = db2lin(p.output_gain_db);
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

struct Learner {
    kind: Kind,
    total: u32,
    left: u32,
    n: u32,
    sum: [f64; NB],
    sumsq: [f64; NB],
}

impl Learner {
    fn new(kind: Kind, frames: u32) -> Self {
        Self { kind, total: frames, left: frames, n: 0, sum: [0.0; NB], sumsq: [0.0; NB] }
    }

    fn add(&mut self, f: &[f32; NB], level_db: f32) {
        self.left = self.left.saturating_sub(1);
        if level_db < -70.0 {
            return; // digital silence teaches nothing
        }
        self.n += 1;
        for i in 0..NB {
            self.sum[i] += f[i] as f64;
            self.sumsq[i] += (f[i] as f64).powi(2);
        }
    }

    fn progress(&self) -> f32 {
        1.0 - self.left as f32 / self.total as f32
    }

    fn finish(&self) -> Option<Profile> {
        if self.n < 20 {
            return None;
        }
        let n = self.n as f64;
        let mean = std::array::from_fn(|i| (self.sum[i] / n) as f32);
        // 4 dB^2 floor so a perfectly steady hum doesn't make the match razor thin.
        let var = std::array::from_fn(|i| ((self.sumsq[i] / n - (self.sum[i] / n).powi(2)) as f32).max(4.0));
        Some(Profile { mean, var })
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

    /// Only the named stage on, gate open, so a test sees one effect at a time.
    fn only(stage: &str) -> Params {
        Params {
            denoise: false,
            gate: false,
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
        chain.start_learning(Kind::Noise, 2.0);
        for c in hiss.chunks(FRAME).take(200) {
            chain.process(&p, &Profiles::default(), &mut c.to_vec());
        }
        let (kind, prof) = chain.take_learned().expect("finished");
        assert_eq!(kind, Kind::Noise);
        let other = Profile { mean: [0.0; NB], var: [4.0; NB] }; // a sound nothing like hiss
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
