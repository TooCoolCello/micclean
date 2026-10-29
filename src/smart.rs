//! Smart setup: chooses each effect's settings from a short recording of the room and of you
//! reading. It works on numbers the chain measures anyway and runs once, so it costs nothing
//! while you talk. Every choice is a plain rule on top of those measurements.

use crate::dsp::{self, Band, BandKind, NB, Params};

/// One audio frame of you talking, as the meters saw it. During setup the gate is held open
/// and the de-esser and compressor are on, so all of these are live.
#[derive(Clone, Copy)]
pub struct Take {
    /// RMS level the gate judges (after input gain and noise removal).
    pub level: f32,
    /// Raw peak (after input gain, before anything else).
    pub peak: f32,
    /// Auto level's gain on this frame.
    pub agc: f32,
    /// De-esser detector level and compressor input peak.
    pub deess: f32,
    pub comp: f32,
    pub pitch: Option<f32>,
    /// Mean power in the analysis bands (dsp::band_edge), dB.
    pub bands: [f32; NB],
}

/// What Smart setup chose, plus anything worth telling you about the room or your voice.
pub struct Outcome {
    pub params: Params,
    pub notes: Vec<String>,
}

/// Long-term average spectrum of speech at normal effort: ANSI S3.5-1997 Table 3 spectrum
/// levels (dB/Hz) at the 1/3-octave centres. Only its shape is used, as the EQ target.
const SPEECH: [(f32, f32); 18] = [
    (160.0, 32.41),
    (200.0, 34.48),
    (250.0, 34.75),
    (315.0, 33.98),
    (400.0, 34.59),
    (500.0, 34.27),
    (630.0, 32.06),
    (800.0, 28.30),
    (1000.0, 25.01),
    (1250.0, 23.00),
    (1600.0, 20.15),
    (2000.0, 17.32),
    (2500.0, 13.18),
    (3150.0, 11.55),
    (4000.0, 9.33),
    (5000.0, 5.31),
    (6300.0, 2.59),
    (8000.0, 1.13),
];

/// The speech target at any frequency: straight lines on a log-frequency axis, flat below
/// 160 Hz and continuing the last slope above 8 kHz.
fn speech_at(f: f32) -> f32 {
    if f <= SPEECH[0].0 {
        return SPEECH[0].1;
    }
    let i = SPEECH.iter().position(|p| p.0 >= f).unwrap_or(SPEECH.len() - 1).max(1);
    let ((f0, y0), (f1, y1)) = (SPEECH[i - 1], SPEECH[i]);
    y0 + (y1 - y0) * (f / f0).log2() / (f1 / f0).log2()
}

fn centre(b: usize) -> f32 {
    (dsp::band_edge(b) * dsp::band_edge(b + 1)).sqrt()
}

/// The `q` quantile (0..1) of `v`, which it sorts.
fn pct(v: &mut [f32], q: f32) -> f32 {
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * q) as usize]
}

/// Half-dB steps, so the sliders show tidy numbers.
fn step(db: f32) -> f32 {
    (db * 2.0).round() / 2.0
}

/// `quiet`: (gate level, raw peak) per frame of room noise. `talk`: frames the voice detector
/// was sure about.
pub fn analyse(p: &Params, quiet: &[(f32, f32)], talk: &[Take]) -> Result<Outcome, String> {
    const TOO_LITTLE: &str = "Didn't hear enough talking. Try again and keep reading until the bar is full.";
    if quiet.len() < 20 {
        return Err(TOO_LITTLE.into());
    }
    let mut notes = Vec::new();
    let (mut room, mut room_peak): (Vec<f32>, Vec<f32>) = quiet.iter().copied().unzip();
    let (noise, room_peak) = (pct(&mut room, 0.9), pct(&mut room_peak, 0.9));
    // Without noise removal the voice detector says "voice" to everything: drop the pauses.
    let talk: Vec<Take> = talk.iter().filter(|t| t.level >= noise + 10.0).copied().collect();
    if talk.len() < 60 {
        return Err(TOO_LITTLE.into());
    }
    let col = |f: fn(&Take) -> f32| talk.iter().map(f).collect::<Vec<f32>>();
    let voice = pct(&mut col(|t| t.level), 0.5);
    let mut s = p.clone();

    // Input gain puts your normal voice at the Auto level target, so auto level only evens out.
    s.input_gain_db = step((p.input_gain_db + p.agc_target_db - voice).clamp(-20.0, 30.0));
    let shift = s.input_gain_db - p.input_gain_db;
    let (noise_s, voice_s) = (noise + shift, voice + shift);

    // The gate closes (threshold minus hysteresis) 3 dB above the room, but stays 20 dB under
    // your voice; in a loud room, halfway between the two.
    let (lo, hi) = (noise_s + p.gate_hysteresis_db + 3.0, voice_s - 20.0);
    s.gate_threshold_db = step(if lo <= hi { lo } else { (lo + hi) / 2.0 }.clamp(-80.0, -10.0));
    if voice_s - noise_s < 15.0 {
        notes.push("Your voice is close to the room noise: noise removal and getting closer to the mic help.".into());
    }

    // Noise removal: a quiet room needs less, and less means fewer watery artifacts. Both are
    // raw peaks, measured before noise removal.
    let snr = pct(&mut col(|t| t.peak), 0.5) - room_peak;
    s.denoise_strength = if snr > 45.0 {
        0.7
    } else if snr > 35.0 {
        0.85
    } else {
        1.0
    };
    if snr < 20.0 {
        notes.push("The room is loud next to your voice. Teaching MicClean the noise (Learn sounds) will help more than any setting.".into());
    }

    // Your average spectrum while talking, in power.
    let avg: [f32; NB] = std::array::from_fn(|b| {
        let sum: f32 = talk.iter().map(|t| 10f32.powf(t.bands[b] / 10.0)).sum();
        10.0 * (sum / talk.len() as f32).log10()
    });
    let mut pitch: Vec<f32> = talk.iter().filter_map(|t| t.pitch).collect();
    let notes_hz = (pitch.len() >= 30).then(|| (pct(&mut pitch, 0.1), pct(&mut pitch, 0.5)));
    s.bands = fit_eq(p, &avg, notes_hz);

    // The de-esser and compressor sit after auto level and the EQ, so their levels move by
    // however much the new gain and EQ change the level there.
    let mut post: Vec<f32> = talk.iter().map(|t| t.level + if p.agc { t.agc } else { 0.0 }).collect();
    let post = pct(&mut post, 0.5);
    let post_new = if s.agc { s.agc_target_db } else { voice_s };
    let level_change = |bands: std::ops::RangeInclusive<usize>| eq_change(p, &s, &avg, bands) + post_new - post;
    let (mid, high) = (level_change(0..=12), level_change(13..=NB - 1));

    // De-esser: only your strongest S sounds (top 10% of the 5 kHz+ level) get trimmed.
    s.deess_threshold_db = (pct(&mut col(|t| t.deess), 0.9) + high - 3.0).clamp(-50.0, 0.0).round();

    // Compressor: your typical peaks sit at the threshold; the wider the gap to your loud
    // peaks, the higher the ratio. Makeup gives back about half of what loud peaks lose.
    let mut peaks = col(|t| t.comp);
    let (c50, c90, c95) = (pct(&mut peaks, 0.5) + mid, pct(&mut peaks, 0.9) + mid, pct(&mut peaks, 0.95) + mid);
    s.comp_ratio = match c95 - c50 {
        x if x > 12.0 => 4.0,
        x if x > 8.0 => 3.0,
        _ => 2.0,
    };
    s.comp_threshold_db = c50.clamp(-50.0, 0.0).round();
    s.comp_makeup_db = step(0.5 * (c90 - s.comp_threshold_db).max(0.0) * (1.0 - 1.0 / s.comp_ratio)).clamp(0.0, 12.0);
    Ok(Outcome { params: s, notes })
}

/// Five gentle bands from how your spectrum differs from the speech target, compared at
/// 0.9..2.7 kHz: low cut under your lowest notes, then boom, mud, presence and air. Cuts go
/// to 4 dB, boosts to 3 dB, and anything under 1 dB is left out. `notes_hz`: your lowest
/// (10th percentile) and typical pitch.
fn fit_eq(p: &Params, avg: &[f32; NB], notes_hz: Option<(f32, f32)>) -> [Option<Band>; dsp::BANDS] {
    let diff: [f32; NB] = std::array::from_fn(|b| avg[b] - speech_at(centre(b)));
    let mean = |a: usize, b: usize| diff[a..=b].iter().sum::<f32>() / (b - a + 1) as f32;
    let reference = mean(7, 10);
    let over = |a: usize, b: usize| mean(a, b) - reference;
    let band = |kind, freq: f32, q, gain_db: f32| {
        let gain_db = step(gain_db);
        (gain_db.abs() >= 1.0).then_some(Band { kind, freq: freq.round(), q, gain_db, on: true })
    };
    // Too much: cut what's past the allowance. Too little (presence and air only): boost.
    let fix = |x: f32, allow: f32, boost: bool| {
        if x > allow {
            -(x - allow).min(4.0)
        } else if boost && x < -allow {
            (-x - allow).min(3.0)
        } else {
            0.0
        }
    };
    let low_cut = match notes_hz {
        // 3/4 of your lowest notes: the rumble and plosive thumps go, your voice stays.
        Some((lowest, _)) => (0.75 * lowest / 5.0).round() * 5.0,
        None => p.bands.iter().flatten().find(|b| b.kind == BandKind::LowCut).map_or(80.0, |b| b.freq),
    }
    .clamp(60.0, 150.0);
    // The target averages men and women: a voice whose fundamental sits in the boom range
    // (135..330 Hz, most women) has more there by nature, not from mic boom.
    let boom_allow = match notes_hz {
        Some((_, typical)) if (dsp::band_edge(1)..dsp::band_edge(3)).contains(&typical) => 6.0,
        _ => 2.0,
    };
    let (mud_band, mud) = (3..=5).map(|b| (b, over(b, b))).max_by(|a, b| a.1.total_cmp(&b.1)).expect("non-empty range");
    let air = over(14, 15);
    [
        Some(Band { kind: BandKind::LowCut, freq: low_cut, q: 0.707, gain_db: 0.0, on: true }),
        band(BandKind::LowShelf, 200.0, 0.707, fix(over(1, 2), boom_allow, false)),
        band(BandKind::Bell, centre(mud_band), 1.4, fix(mud, 2.0, false)),
        band(BandKind::Bell, 3200.0, 1.0, fix(over(11, 12), 1.5, true)),
        // Over 12 dB short means the mic (or the call) cuts off there: lifting it adds only hiss.
        band(BandKind::HighShelf, 8000.0, 0.707, fix(air, 2.0, air > -12.0)),
    ]
}

/// How much louder (dB) the new EQ makes your voice than the old one, over some bands,
/// weighted by how much of your voice is in each.
fn eq_change(old: &Params, new: &Params, avg: &[f32; NB], bands: std::ops::RangeInclusive<usize>) -> f32 {
    let freqs: Vec<f32> = (0..NB).map(centre).collect();
    let curve = |p: &Params| if p.eq { dsp::eq_curve(p, &freqs) } else { vec![0.0; NB] };
    let (a, b) = (curve(old), curve(new));
    let (mut num, mut den) = (0.0, 0.0);
    for i in bands {
        let w = 10f32.powf(avg[i] / 10.0);
        num += w * 10f32.powf((b[i] - a[i]) / 10.0);
        den += w;
    }
    10.0 * (num / den).log10()
}

fn hz(f: f32) -> String {
    if f >= 1000.0 { format!("{:.1} kHz", f / 1000.0) } else { format!("{f:.0} Hz") }
}

/// One line per effect whose Smart setup settings differ, for the summary and Suggest.
pub fn changes(old: &Params, new: &Params) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = |name: &str, parts: &[Option<String>]| {
        let parts: Vec<&str> = parts.iter().flatten().map(String::as_str).collect();
        if !parts.is_empty() {
            out.push(format!("{name}: {}", parts.join(" · ")));
        }
    };
    let db = |what: &str, a: f32, b: f32| ((a - b).abs() >= 0.05).then(|| format!("{what}{a:+.1} → {b:+.1} dB"));
    line("Input gain", &[db("", old.input_gain_db, new.input_gain_db)]);
    let strength = (old.denoise_strength != new.denoise_strength)
        .then(|| format!("strength {:.0}% → {:.0}%", old.denoise_strength * 100.0, new.denoise_strength * 100.0));
    line("Noise removal", &[strength]);
    line("Voice gate", &[db("minimum level ", old.gate_threshold_db, new.gate_threshold_db)]);
    if old.bands != new.bands {
        let bands: Vec<String> = new
            .bands
            .iter()
            .flatten()
            .map(|b| match b.kind {
                BandKind::LowCut => format!("low cut {}", hz(b.freq)),
                BandKind::HighCut => format!("high cut {}", hz(b.freq)),
                BandKind::LowShelf => format!("{:+.1} dB below {}", b.gain_db, hz(b.freq)),
                BandKind::HighShelf => format!("{:+.1} dB above {}", b.gain_db, hz(b.freq)),
                BandKind::Bell => format!("{:+.1} dB at {}", b.gain_db, hz(b.freq)),
                BandKind::Notch => format!("notch at {}", hz(b.freq)),
            })
            .collect();
        line("Equalizer", &[Some(bands.join(", "))]);
    }
    line("De-esser", &[db("threshold ", old.deess_threshold_db, new.deess_threshold_db)]);
    let ratio = (old.comp_ratio != new.comp_ratio).then(|| format!("ratio {:.0}:1 → {:.0}:1", old.comp_ratio, new.comp_ratio));
    line(
        "Compressor",
        &[db("threshold ", old.comp_threshold_db, new.comp_threshold_db), ratio, db("makeup ", old.comp_makeup_db, new.comp_makeup_db)],
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of a voice that matches the speech target exactly, at `level` dB.
    fn take(level: f32, comp: f32, deess: f32) -> Take {
        let bands = std::array::from_fn(|b| speech_at(centre(b)) - 60.0);
        Take { level, peak: level + 12.0, agc: 0.0, deess, comp, pitch: Some(120.0), bands }
    }

    fn quiet(level: f32) -> Vec<(f32, f32)> {
        vec![(level, level + 10.0); 60]
    }

    #[test]
    fn gain_puts_the_voice_at_the_auto_level_target_and_the_gate_above_the_room() {
        // Defaults: auto level target -22 dB, gate hysteresis 6 dB.
        let o = analyse(&Params::default(), &quiet(-70.0), &vec![take(-34.0, -20.0, -40.0); 200]).unwrap();
        assert_eq!(o.params.input_gain_db, 12.0, "voice -34 lifted to -22");
        assert_eq!(o.params.gate_threshold_db, -49.0, "room now -58: closes at -55, 3 dB above it");

        // Noisy room: halfway between "above the room" (-23) and "20 dB under the voice" (-42).
        let o = analyse(&Params::default(), &quiet(-40.0), &vec![take(-30.0, -20.0, -40.0); 200]).unwrap();
        assert_eq!(o.params.gate_threshold_db, -32.5);
        assert!(o.notes.iter().any(|n| n.contains("close to the room noise")));

        assert!(analyse(&Params::default(), &quiet(-70.0), &vec![take(-34.0, -20.0, -40.0); 20]).is_err());
    }

    #[test]
    fn a_voice_like_the_target_gets_only_a_low_cut_under_its_lowest_notes() {
        let o = analyse(&Params::default(), &quiet(-70.0), &vec![take(-22.0, -20.0, -40.0); 200]).unwrap();
        assert_eq!(o.params.bands[0].map(|b| (b.kind, b.freq)), Some((BandKind::LowCut, 90.0)), "3/4 of 120 Hz");
        assert!(o.params.bands[1..].iter().all(Option::is_none), "{:?}", changes(&Params::default(), &o.params));
    }

    #[test]
    fn boom_and_dull_highs_get_gentle_capped_corrections() {
        let mut t = take(-22.0, -20.0, -40.0);
        t.bands[1] += 8.0; // proximity-effect boom around 150-200 Hz
        t.bands[2] += 8.0;
        for b in 11..=15 {
            t.bands[b] -= 5.0; // a dull mic
        }
        let o = analyse(&Params::default(), &quiet(-70.0), &vec![t; 200]).unwrap();
        let gain = |o: &Outcome, i: usize| o.params.bands[i].map_or(0.0, |b| b.gain_db);
        assert_eq!(gain(&o, 1), -4.0, "boom cut, capped at 4 dB");
        assert_eq!(gain(&o, 3), 3.0, "presence lifted, capped at 3 dB");
        assert_eq!(gain(&o, 4), 3.0, "air lifted");

        // A higher voice has its fundamental in the boom range, and a mic that stops at 8 kHz
        // has no air to lift.
        t.pitch = Some(200.0);
        t.bands[14] -= 15.0;
        t.bands[15] -= 15.0;
        let o = analyse(&Params::default(), &quiet(-70.0), &vec![t; 200]).unwrap();
        assert_eq!(gain(&o, 1), -2.0, "8 dB over, 6 dB of it allowed");
        assert_eq!(gain(&o, 4), 0.0, "nothing up there to lift");
    }

    /// Smart setup on a recording, through the real chain:
    /// `MC_WAV=voice.wav cargo test smart_on_wav -- --ignored --nocapture` (16-bit PCM, 48 kHz).
    /// 3 s of faint noise go first as the room, and the same noise stays under the voice.
    #[test]
    #[ignore]
    fn smart_on_wav() {
        let bytes = std::fs::read(std::env::var("MC_WAV").expect("set MC_WAV")).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48_000, "needs 48 kHz");
        let ch = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
        let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
        let voice = bytes[data..].chunks_exact(2 * ch).map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0);
        let mut seed = 1u32;
        let mut x: Vec<f32> = std::iter::repeat_n(0.0, 3 * 48_000)
            .chain(voice)
            .map(|s| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                s + ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002 // about -65 dBFS
            })
            .collect();
        // What the app runs during setup: the gate open, the de-esser and compressor on.
        let p = Params { gate: false, deess: true, comp: true, ..Params::default() };
        let (mut chain, prof) = (dsp::Chain::default(), dsp::Profiles::default());
        let (mut quiet, mut talk) = (Vec::new(), Vec::new());
        for (i, f) in x.chunks_exact_mut(dsp::FRAME).enumerate() {
            chain.process(&p, &prof, f);
            let m = &chain.meters;
            if (50..300).contains(&i) {
                quiet.push((m.gate_level_db, m.in_db));
            } else if i >= 300 && m.vad >= 0.5 {
                let (level, peak, agc, deess, comp) = (m.gate_level_db, m.in_db, m.agc_db, m.deess_level_db, m.comp_in_db);
                talk.push(Take { level, peak, agc, deess, comp, pitch: m.pitch_hz, bands: m.bands_db });
            }
        }
        let base = Params::default();
        let o = analyse(&base, &quiet, &talk).unwrap();
        println!("{} quiet frames, {} talking frames", quiet.len(), talk.len());
        for line in changes(&base, &o.params).iter().chain(&o.notes) {
            println!("{line}");
        }
    }

    #[test]
    fn compressor_and_de_esser_follow_your_peaks() {
        // Typical peaks at -20 dB and the loudest 15% 10 dB over them; S levels -45..-27 dB.
        let mut talk: Vec<Take> = (0..200).map(|i| take(-22.0, -20.0, -45.0 + (i % 10) as f32 * 2.0)).collect();
        for t in talk.iter_mut().skip(170) {
            t.comp = -10.0;
        }
        let o = analyse(&Params::default(), &quiet(-70.0), &talk).unwrap();
        // Voice already at the target and only a low cut added: levels there don't move.
        assert_eq!(o.params.comp_threshold_db, -20.0);
        assert_eq!(o.params.comp_ratio, 3.0, "10 dB from typical to loud peaks");
        assert_eq!(o.params.comp_makeup_db, 3.5, "half of the 6.7 dB loud peaks lose");
        assert_eq!(o.params.deess_threshold_db, -32.0, "3 dB under where the top 10% of S levels start (-29)");
    }
}
