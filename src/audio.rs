//! Device I/O. All processing runs inside the mic callback. The speaker loopback (echo
//! reference) and the output devices talk to it through lock-free rings.

use crate::dsp::{Chain, FRAME, Kind, Meters, Params, Profile, Profiles, SR};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use serde::{Deserialize, Serialize};
use sonora::config::{EchoCanceller, MaxProcessingRate, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// GUI <-> mic callback mailbox. The callback only ever try_locks it.
#[derive(Default)]
pub struct Shared {
    pub params: Params,
    pub params_ver: u64,
    pub profiles: Profiles,
    pub profiles_ver: u64,
    /// Learn request from the GUI: what, and for how many seconds.
    pub learn: Option<(Kind, f32)>,
    /// Finished learn run for the GUI to store (None profile = clip too quiet).
    pub learned: Option<(Kind, Option<Profile>)>,
    /// Zeroed buffer from the GUI to record the mic into (before the chain), returned when full.
    pub record: Option<Vec<f32>>,
    pub recorded: Option<Vec<f32>>,
    pub record_progress: Option<f32>,
    /// Silences the monitor output while a recording plays back on it.
    pub mute_monitor: bool,
    /// The window is on screen, so the display-only spectra are worth computing.
    pub viz: bool,
    pub latency: Arc<Latency>,
    pub meters: Meters,
    /// Dropped or late capture buffers (cpal Xrun). The stream keeps running, so these are
    /// only counted, with the device that reported the last one.
    pub glitches: u32,
    pub glitch_dev: String,
    pub error: Option<String>,
}

/// Device-side delays in microseconds, written by the callbacks (atomics: no locking there).
#[derive(Default)]
pub struct Latency {
    /// Mic: from capture to our callback.
    pub capture_us: AtomicU32,
    /// Discord feed: our output ring plus the device's buffer until playback.
    pub output_us: AtomicU32,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    pub input: String,
    pub output: String,
    pub monitor: Option<String>,
    /// Speakers to cancel out of the mic. None = headphones, no echo cancellation.
    pub echo_ref: Option<String>,
}

pub fn device_names(input: bool) -> Vec<String> {
    let host = cpal::default_host();
    let devs = if input { host.input_devices() } else { host.output_devices() };
    devs.map(|d| d.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect())
        .unwrap_or_default()
}

fn find(input: bool, name: &str) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let mut devs = if input { host.input_devices() } else { host.output_devices() }.map_err(es)?;
    devs.find(|d| d.description().is_ok_and(|x| x.name() == name))
        .ok_or_else(|| format!("device not found: {name}"))
}

fn es(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Stream error callback. Xruns and refused realtime priority leave the stream running, so
/// they must not trigger a reconnect; anything else asks the GUI to reopen the devices.
fn on_error(shared: Arc<Mutex<Shared>>, dev: String) -> impl FnMut(cpal::Error) + Send + 'static {
    move |e: cpal::Error| {
        let Ok(mut s) = shared.lock() else { return };
        match e.kind() {
            cpal::ErrorKind::Xrun => {
                s.glitches += 1;
                s.glitch_dev.clone_from(&dev);
            }
            cpal::ErrorKind::RealtimeDenied => {}
            _ => s.error = Some(e.to_string()),
        }
    }
}

/// Keeps the streams alive; dropping it stops everything.
pub struct Engine {
    _streams: Vec<cpal::Stream>,
    pub info: String,
    /// Optional devices (monitor, echo reference) that weren't plugged in at start.
    pub missing: Vec<String>,
}

pub fn start(dev: &Devices, shared: Arc<Mutex<Shared>>) -> Result<Engine, String> {
    let mic = find(true, &dev.input)?;
    let mic_cfg = mic.default_input_config().map_err(es)?;
    let rate = mic_cfg.sample_rate();
    if rate % 100 != 0 {
        return Err(format!("{} runs at {rate} Hz; set it to 48000 Hz in Windows sound settings", dev.input));
    }
    let mut streams = Vec::new();
    let mut info = format!("mic {} Hz {} ch", rate, mic_cfg.channels());

    let mut missing = Vec::new();
    let mut outs = Vec::new(); // (ring, is_monitor)
    let latency = shared.lock().map_err(es)?.latency.clone();
    let (stream, prod) = open_output(&dev.output, shared.clone(), Some(latency.clone()))?;
    streams.push(stream);
    outs.push((prod, false));
    // Headphones unplugged shouldn't take the Discord feed down with them.
    if let Some(name) = &dev.monitor {
        match open_output(name, shared.clone(), None) {
            Ok((stream, prod)) => {
                streams.push(stream);
                outs.push((prod, true));
            }
            Err(_) => missing.push(name.clone()),
        }
    }

    let mut echo = None;
    if let Some(name) = &dev.echo_ref {
        match open_echo_ref(name, shared.clone()) {
            Ok((stream, cons, ref_rate)) => {
                streams.push(stream);
                info += &format!(", echo ref {ref_rate} Hz");
                echo = Some((cons, ref_rate));
            }
            Err(_) => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        info += &format!(" (not plugged in: {})", missing.join(", "));
    }

    let mut apm = AudioProcessing::builder()
        .config(Config {
            pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
            echo_canceller: echo.as_ref().map(|_| EchoCanceller::default()),
            ..Default::default()
        })
        .capture_config(StreamConfig::new(rate, 1))
        .render_config(StreamConfig::new(echo.as_ref().map_or(rate, |e| e.1), 1))
        .build();
    // The APM also resamples the mic to 48 kHz when it runs at e.g. 44.1 kHz.
    let in_cfg = StreamConfig::new(rate, 1);
    let out_cfg = StreamConfig::new(SR as u32, 1);
    let mut chain = Chain::default();
    let frame_in = rate as usize / 100;
    let mut acc = Vec::with_capacity(frame_in);
    let mut frame = [0.0f32; FRAME];
    let (mut params, mut params_ver) = (Params::default(), u64::MAX);
    let (mut profiles, mut profiles_ver) = (Profiles::default(), u64::MAX);
    let ref_n = echo.as_ref().map_or(0, |e| e.1 as usize / 100);
    let (mut ref_in, mut ref_out) = (vec![0.0f32; ref_n], vec![0.0f32; ref_n]);
    let mut starved = 0;
    let (mut rec, mut rec_pos) = (None::<Vec<f32>>, 0);
    let mut mute_monitor = false;
    let sh = shared.clone();

    streams.push(open_input(&mic, &mic_cfg, shared, move |mono, lag| {
        latency.capture_us.store(lag.as_micros() as u32, Relaxed);
        for &s in mono {
            acc.push(s);
            if acc.len() < frame_in {
                continue;
            }
            if let Ok(mut sh) = sh.try_lock() {
                if sh.params_ver != params_ver {
                    params = sh.params.clone();
                    params_ver = sh.params_ver;
                }
                if sh.profiles_ver != profiles_ver {
                    profiles = sh.profiles.clone();
                    profiles_ver = sh.profiles_ver;
                }
                if let Some((kind, secs)) = sh.learn.take() {
                    chain.start_learning(kind, secs);
                }
                if let Some(r) = chain.take_learned() {
                    sh.learned = Some(r);
                }
                if let Some(buf) = sh.record.take() {
                    rec = Some(buf);
                    rec_pos = 0;
                }
                sh.record_progress = rec.as_ref().map(|r| rec_pos as f32 / r.len() as f32);
                if rec.as_ref().is_some_and(|r| rec_pos == r.len()) {
                    sh.recorded = rec.take();
                }
                mute_monitor = sh.mute_monitor;
                chain.viz = sh.viz;
                sh.meters = chain.meters.clone();
            }

            if let Some((cons, ref_rate)) = &mut echo {
                let rc = StreamConfig::new(*ref_rate, 1);
                let backlog = cons.occupied_len();
                if backlog > ref_n * 20 {
                    cons.skip(backlog - ref_n * 4); // fell 200 ms behind: catch up
                }
                let mut fed = false;
                let mut peak = 1e-6f32;
                while cons.occupied_len() >= ref_n {
                    cons.pop_slice(&mut ref_in);
                    peak = ref_in.iter().fold(peak, |m, s| m.max(s.abs()));
                    let _ = apm.process_render_f32_with_config(&[&ref_in], &rc, &rc, &mut [&mut ref_out]);
                    fed = true;
                }
                chain.meters.echo_ref_db = 20.0 * peak.log10();
                // WASAPI loopback goes quiet when nothing plays. After 30 ms of nothing, feed
                // silence so AEC3 keeps its timeline; shorter gaps are just callback jitter.
                starved = if fed { 0 } else { starved + 1 };
                if starved > 3 {
                    ref_in.fill(0.0);
                    let _ = apm.process_render_f32_with_config(&[&ref_in], &rc, &rc, &mut [&mut ref_out]);
                }
            }

            let _ = apm.process_capture_f32_with_config(&[&acc], &in_cfg, &out_cfg, &mut [&mut frame]);
            acc.clear();
            if echo.is_some() {
                let st = apm.statistics(); // a borrowed struct, nothing computed here
                chain.meters.echo_erle_db = st.echo_return_loss_enhancement.map(|v| v as f32);
                chain.meters.echo_delay_ms = st.delay_ms;
            }
            if let Some(r) = &mut rec {
                let k = (r.len() - rec_pos).min(FRAME);
                r[rec_pos..rec_pos + k].copy_from_slice(&frame[..k]);
                rec_pos += k;
            }
            chain.process(&params, &profiles, &mut frame);
            for (o, is_monitor) in &mut outs {
                if !(*is_monitor && mute_monitor) {
                    o.push_slice(&frame);
                }
            }
        }
    })?);

    Ok(Engine { _streams: streams, info, missing })
}

fn open_echo_ref(
    name: &str,
    shared: Arc<Mutex<Shared>>,
) -> Result<(cpal::Stream, ringbuf::HeapCons<f32>, u32), String> {
    let spk = find(false, name)?;
    let cfg = spk.default_output_config().map_err(es)?;
    let ref_rate = cfg.sample_rate();
    if ref_rate % 100 != 0 {
        return Err(format!("{name} runs at {ref_rate} Hz; set it to 48000 Hz in Windows sound settings"));
    }
    let (mut prod, cons) = HeapRb::<f32>::new(ref_rate as usize).split();
    // Input stream on an output device = WASAPI loopback of what it plays.
    let stream = open_input(&spk, &cfg, shared, move |mono, _| {
        prod.push_slice(mono);
    })?;
    Ok((stream, cons, ref_rate))
}

/// Plays a 48 kHz mono clip once on the named output (or the default one).
pub fn play(name: Option<&str>, clip: Vec<f32>) -> Result<cpal::Stream, String> {
    let dev = match name {
        Some(n) => find(false, n)?,
        None => cpal::default_host().default_output_device().ok_or("no output device")?,
    };
    let ch = dev.default_output_config().map_err(es)?.channels() as usize;
    let mut pos = 0;
    let stream = dev
        .build_output_stream(
            cpal::StreamConfig { channels: ch as u16, sample_rate: SR as u32, buffer_size: cpal::BufferSize::Default },
            move |d: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for fr in d.chunks_mut(ch) {
                    fr.fill(clip.get(pos).copied().unwrap_or(0.0));
                    pos += 1;
                }
            },
            |_| {},
            None,
        )
        .map_err(es)?;
    stream.play().map_err(es)?;
    Ok(stream)
}

/// Opens a capture stream in the device's native format and hands the callback mono f32,
/// plus how long ago the device captured it.
fn open_input(
    dev: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    shared: Arc<Mutex<Shared>>,
    f: impl FnMut(&[f32], Duration) + Send + 'static,
) -> Result<cpal::Stream, String> {
    fn build<T>(
        dev: &cpal::Device,
        sc: cpal::StreamConfig,
        mut f: impl FnMut(&[f32], Duration) + Send + 'static,
        err: impl FnMut(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let ch = sc.channels as usize;
        let mut mono = Vec::new();
        dev.build_input_stream(
            sc,
            move |d: &[T], info: &cpal::InputCallbackInfo| {
                mono.clear();
                mono.extend(d.chunks(ch).map(|fr| fr.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32));
                let ts = info.timestamp();
                f(&mono, ts.callback.duration_since(ts.capture));
            },
            err,
            None,
        )
    }
    let err = on_error(shared, dev.description().map(|d| d.name().to_string()).unwrap_or_default());
    let sc = cfg.config();
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(dev, sc, f, err),
        cpal::SampleFormat::I16 => build::<i16>(dev, sc, f, err),
        cpal::SampleFormat::I32 => build::<i32>(dev, sc, f, err),
        other => return Err(format!("unsupported sample format {other}")),
    }
    .map_err(es)?;
    stream.play().map_err(es)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apm_resamples_44k1_mic_to_48k() {
        let (i, o) = (StreamConfig::new(44_100, 1), StreamConfig::new(48_000, 1));
        let mut apm = AudioProcessing::builder().capture_config(i).build();
        let mut out = [0.0f32; FRAME];
        for k in 0..30 {
            let x: Vec<f32> = (0..441)
                .map(|n| (std::f32::consts::TAU * 440.0 * (k * 441 + n) as f32 / 44_100.0).sin() * 0.5)
                .collect();
            apm.process_capture_f32_with_config(&[&x], &i, &o, &mut [&mut out]).unwrap();
        }
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.05, "peak {peak}");
    }

    #[test]
    fn drift_reader_rides_out_clock_drift() {
        for ppm in [-400.0, 400.0] {
            let (mut prod, mut cons) = HeapRb::<f32>::new(SR as usize).split();
            let mut r = DriftReader::default();
            let (mut out, mut prev) = ([0.0f32; 480], 0.0f32);
            let (mut owed, mut n, mut jump) = (0.0f64, 0u64, 0.0f32);
            // 10 minutes: the mic pushes 10 ms bursts on its clock, the output pulls 10 ms on its own.
            for tick in 0..60_000 {
                owed += 480.0 * (1.0 + ppm * 1e-6);
                let burst: Vec<f32> = (0..owed as u64)
                    .map(|k| (std::f32::consts::TAU * ((n + k) % 48) as f32 / 48.0).sin() * 0.5) // 1 kHz
                    .collect();
                n += burst.len() as u64;
                owed -= burst.len() as f64;
                prod.push_slice(&burst);
                r.read(&mut cons, &mut out);
                if tick > 10 {
                    assert!(r.primed, "{ppm} ppm: ran dry at tick {tick}");
                    for &s in &out {
                        jump = jump.max((s - prev).abs());
                        prev = s;
                    }
                }
                prev = out[479];
            }
            // A 1 kHz sine at 0.5 moves at most 0.065 per sample; a skip or gap jumps far more.
            assert!(jump < 0.07, "{ppm} ppm: jump {jump}");
            let off = r.fill - 1440.0;
            assert!(off.abs() < 600.0, "{ppm} ppm: fill {off:+.0} off target");
            println!("{ppm:+} ppm: fill {off:+.0} samples off target");
        }
    }

    /// Needs real devices: `cargo test -- --ignored`. Runs default mic -> default output with
    /// echo cancellation against that same output for 2 s and checks frames came through.
    #[test]
    #[ignore]
    fn engine_runs_on_default_devices() {
        let host = cpal::default_host();
        let name = |d: Option<cpal::Device>| d.and_then(|d| d.description().ok()).map(|d| d.name().to_string());
        let input = name(host.default_input_device()).expect("no input device");
        let output = name(host.default_output_device()).expect("no output device");
        println!("in: {input}\nout: {output}");
        let dev = Devices { input, output: output.clone(), monitor: None, echo_ref: Some(output) };
        let shared = Arc::new(Mutex::new(Shared::default()));
        let engine = start(&dev, shared.clone()).expect("start");
        println!("{}", engine.info);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let sh = shared.lock().unwrap();
        assert!(sh.error.is_none(), "{:?}", sh.error);
        assert!(!sh.meters.reason.is_empty(), "no frames processed");
        println!("in {:.0} dB, gate {}", sh.meters.in_db, sh.meters.reason);
    }
}

/// Opens a 48 kHz f32 output (WASAPI converts to the device format) fed by a mono ring.
/// `latency`: report this output's delay there (the Discord feed only, not the monitor).
fn open_output(
    name: &str,
    shared: Arc<Mutex<Shared>>,
    latency: Option<Arc<Latency>>,
) -> Result<(cpal::Stream, ringbuf::HeapProd<f32>), String> {
    let dev = find(false, name)?;
    let ch = dev.default_output_config().map_err(es)?.channels();
    let (prod, mut cons) = HeapRb::<f32>::new(SR as usize).split();
    let mut mono = Vec::new();
    let mut reader = DriftReader::default();
    let stream = dev
        .build_output_stream(
            cpal::StreamConfig { channels: ch, sample_rate: SR as u32, buffer_size: cpal::BufferSize::Default },
            move |d: &mut [f32], info: &cpal::OutputCallbackInfo| {
                mono.resize(d.len() / ch as usize, 0.0);
                reader.read(&mut cons, &mut mono);
                if let Some(l) = &latency {
                    let ts = info.timestamp();
                    let ring = Duration::from_secs_f64(reader.fill / SR as f64);
                    l.output_us.store((ts.playback.duration_since(ts.callback) + ring).as_micros() as u32, Relaxed);
                }
                for (fr, s) in d.chunks_mut(ch as usize).zip(&mono) {
                    fr.fill(*s);
                }
            },
            on_error(shared, name.to_string()),
            None,
        )
        .map_err(es)?;
    stream.play().map_err(es)?;
    Ok((stream, prod))
}

/// Plays the mic ring out on the output device's clock. The two clocks never tick at exactly
/// the same rate (tens of ppm apart), so a plain buffer slowly fills up or runs dry and clicks.
/// Instead the read speed is nudged, by at most 0.5%, to hold the ring at its target fill.
#[derive(Default)]
struct DriftReader {
    /// 4-point Hermite taps; the read point sits between taps[1] and taps[2].
    taps: [f32; 4],
    t: f64,
    /// Ring fill smoothed over ~1 s, in samples.
    fill: f64,
    /// Largest callback seen, in samples.
    period: usize,
    primed: bool,
}

impl DriftReader {
    fn read(&mut self, ring: &mut ringbuf::HeapCons<f32>, out: &mut [f32]) {
        self.period = self.period.max(out.len());
        // Enough to ride out one whole callback on top of the mic's 10 ms bursts.
        let target = 720 + self.period * 3 / 2;
        let have = ring.occupied_len();
        if have > target + SR as usize / 10 {
            // 100 ms over can't be drift; the output stalled. Jump back instead of racing.
            ring.skip(have - target);
            self.fill = target as f64;
        }
        if !self.primed {
            // Starting, or ran dry: wait for the cushion so jitter doesn't click every period.
            self.primed = have >= target;
            self.fill = target as f64;
            if !self.primed {
                out.fill(0.0);
                return;
            }
        }
        self.fill += (ring.occupied_len() as f64 - self.fill) * out.len() as f64 / SR as f64;
        // ponytail: proportional only. Settles in ~20 s and sits (ppm) samples off target,
        // e.g. 8 ms at 400 ppm. Add an integral term if latency ever matters that much.
        let step = 1.0 + ((self.fill - target as f64) * 1e-6).clamp(-0.005, 0.005);
        for i in 0..out.len() {
            let [a, b, c, d] = self.taps;
            let t = self.t as f32;
            let c1 = 0.5 * (c - a);
            let c2 = a - 2.5 * b + 2.0 * c - 0.5 * d;
            let c3 = 0.5 * (d - a) + 1.5 * (b - c);
            out[i] = ((c3 * t + c2) * t + c1) * t + b;
            self.t += step;
            while self.t >= 1.0 {
                self.t -= 1.0;
                let Some(x) = ring.try_pop() else {
                    self.primed = false;
                    out[i + 1..].fill(0.0);
                    return;
                };
                self.taps.rotate_left(1);
                self.taps[3] = x;
            }
        }
    }
}
