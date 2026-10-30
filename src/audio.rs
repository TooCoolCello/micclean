//! Device I/O. All processing runs inside the mic callback. The speaker loopback (echo
//! reference) and the output devices talk to it through lock-free rings. Most streams are
//! cpal's; the two it can't open (exclusive output, every app but this one as the echo
//! source) run on their own threads with the wasapi crate.

use crate::dsp::{Chain, FRAME, Learner, Meters, Params, Profiles, SR};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use serde::{Deserialize, Serialize};
use sonora::config::{EchoCanceller, MaxProcessingRate, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Input channels with their own level meter.
pub const MAX_CH: usize = 8;

/// GUI <-> mic callback mailbox. The callback only ever try_locks it, and never allocates or
/// frees through it: whatever it lets go of goes back here for the GUI to drop.
#[derive(Default)]
pub struct Shared {
    pub params: Params,
    pub params_ver: u64,
    /// The learned sounds the GUI last handed over.
    pub profiles: Profiles,
    /// A new copy of them for the callback to swap in; it hands its old one back in `old_profiles`.
    pub new_profiles: Option<Profiles>,
    pub old_profiles: Option<Profiles>,
    /// Learn run from the GUI, and the same run back with all its frames.
    pub learn: Option<Learner>,
    pub learned: Option<Learner>,
    /// Zeroed buffer from the GUI to record the mic into (before the chain), returned when full.
    pub record: Option<Vec<f32>>,
    pub recorded: Option<Vec<f32>>,
    /// A recording a new `record` cut short (an empty one stops it), for the GUI to drop.
    pub discarded: Option<Vec<f32>>,
    pub record_progress: Option<f32>,
    /// Silences the monitor output while a recording plays back on it.
    pub mute_monitor: bool,
    /// Silences what's sent (not Hear myself) while Smart setup listens to the room and to you
    /// reading, which friends shouldn't have to hear.
    pub mute_sent: bool,
    /// The window is on screen, so the display-only spectra are worth computing.
    pub viz: bool,
    pub latency: Arc<Latency>,
    pub meters: Meters,
    /// Peak (linear) of each input channel since the GUI last looked, before one is picked,
    /// so Setup can show which input your voice is on.
    pub channel_peak: [f32; MAX_CH],
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

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    pub input: String,
    /// The one input channel to use (0 = input 1), for interfaces Windows shows as a single
    /// stereo device like "Analogue 1 + 2". None = all channels mixed together.
    pub input_channel: Option<u16>,
    pub output: String,
    pub monitor: Option<String>,
    /// What you listen on (speakers, or open-back headphones), cancelled out of the mic:
    /// a device, or EVERYTHING. None = no echo cancellation.
    pub echo_ref: Option<String>,
    /// Take the output for MicClean alone (exclusive mode) when it's a virtual cable, so a
    /// screen share doesn't send your voice a second time.
    #[serde(default = "yes")]
    pub exclusive_output: bool,
}

fn yes() -> bool {
    true
}

impl Default for Devices {
    fn default() -> Self {
        Self { input: String::new(), input_channel: None, output: String::new(), monitor: None, echo_ref: None, exclusive_output: true }
    }
}

/// The echo source that hears every app except MicClean itself (Windows' per-app loopback,
/// Windows 10 2004 and later), so your own voice from Hear myself never lands in it.
pub const EVERYTHING: &str = "Everything you hear (except MicClean)";

/// Only virtual cables are taken in exclusive mode: on speakers it would silence every other app.
// ponytail: VB-Cable's names (CABLE Input, CABLE-A Input, Hi-Fi CABLE Input); add others when asked.
pub fn can_be_exclusive(name: &str) -> bool {
    name.contains("CABLE")
}

/// Windows' default playback device: usually what you listen on.
pub fn default_output_name() -> Option<String> {
    let d = cpal::default_host().default_output_device()?;
    d.description().ok().map(|d| d.name().to_string())
}

/// VB-Cable's ends, also when renamed in Windows: as a mic, MicClean would hear itself.
pub fn is_cable(name: &str) -> bool {
    name.contains("CABLE") || name.contains("VB-Audio")
}

/// The mic to start with: Windows' default one, unless that's a cable or Stereo Mix (what the
/// PC plays), then the first real one.
pub fn first_mic(inputs: &[String]) -> Option<String> {
    let real = |n: &String| !is_cable(n) && !n.contains("Stereo Mix");
    let default = cpal::default_host().default_input_device().and_then(|d| d.description().ok()).map(|d| d.name().to_string());
    default.filter(real).or_else(|| inputs.iter().find(|n| real(n)).cloned())
}

pub fn device_names(input: bool) -> Vec<String> {
    let host = cpal::default_host();
    let devs = if input { host.input_devices() } else { host.output_devices() };
    devs.map(|d| d.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect())
        .unwrap_or_default()
}

/// The rate and channel count Windows runs a device at (its shared-mode mix format).
pub fn device_format(input: bool, name: &str) -> Option<(u32, u16)> {
    let d = find(input, name).ok()?;
    let cfg = if input { d.default_input_config() } else { d.default_output_config() };
    cfg.ok().map(|c| (c.sample_rate(), c.channels()))
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
        // cpal calls this on the capture thread for every glitch: those never wait for the GUI
        // (one missed count is fine). Real errors are rare enough to wait.
        let s = if matches!(e.kind(), cpal::ErrorKind::Xrun) { shared.try_lock().ok() } else { shared.lock().ok() };
        let Some(mut s) = s else { return };
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
    _workers: Vec<Worker>,
    pub info: String,
    /// Why the output couldn't be taken in exclusive mode (and so plays through Windows'
    /// mixer, where a screen share can pick it up), when it was meant to be.
    pub shared_output: Option<String>,
    /// Optional devices (monitor, echo reference) that weren't plugged in at start. The GUI
    /// restarts audio when one comes back.
    pub missing: Vec<String>,
    /// Optional devices that are plugged in but wouldn't open (held in exclusive mode by
    /// another app, an odd rate), and why. Not retried on their own: they'd fail the same way
    /// every time and take the Discord feed down with them.
    pub failed: Vec<(String, String)>,
}

/// Sorts an optional device that didn't open into unplugged or failed.
fn skipped(name: &str, err: String, missing: &mut Vec<String>, failed: &mut Vec<(String, String)>) {
    if find(false, name).is_err() {
        missing.push(name.to_string());
    } else {
        failed.push((name.to_string(), err));
    }
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
    if let Some(c) = dev.input_channel {
        info += &format!(" (input {})", c + 1);
    }

    let (mut missing, mut failed) = (Vec::new(), Vec::new());
    let (mut workers, mut shared_output) = (Vec::new(), None);
    let mut outs = Vec::new(); // (ring, is_monitor)
    let latency = shared.lock().map_err(es)?.latency.clone();
    // No output yet (Setup, before VB-Cable is in): the mic still runs, for the meters.
    if dev.output.is_empty() {
        info += ", nothing sent yet";
    } else {
        let exclusive = if dev.exclusive_output && can_be_exclusive(&dev.output) {
            open_output_exclusive(&dev.output, shared.clone(), latency.clone()).map_err(|e| shared_output = Some(e)).ok()
        } else {
            None
        };
        let prod = match exclusive {
            Some((worker, prod)) => {
                workers.push(worker);
                info += ", output exclusive";
                prod
            }
            None => {
                let (stream, prod) = open_output(&dev.output, shared.clone(), Some(latency.clone()))?;
                streams.push(stream);
                prod
            }
        };
        outs.push((prod, false));
    }
    // Headphones unplugged shouldn't take the Discord feed down with them.
    if let Some(name) = &dev.monitor {
        match open_output(name, shared.clone(), None) {
            Ok((stream, prod)) => {
                streams.push(stream);
                outs.push((prod, true));
            }
            Err(e) => skipped(name, e, &mut missing, &mut failed),
        }
    }

    let mut echo = None;
    if let Some(name) = &dev.echo_ref {
        let opened = if name == EVERYTHING {
            open_echo_everything(shared.clone()).map(|(w, cons, rate)| {
                workers.push(w);
                (cons, rate)
            })
        } else {
            open_echo_ref(name, shared.clone()).map(|(s, cons, rate)| {
                streams.push(s);
                (cons, rate)
            })
        };
        match opened {
            Ok((cons, ref_rate)) => {
                info += &format!(", echo ref {ref_rate} Hz");
                echo = Some((cons, ref_rate));
            }
            Err(e) if name == EVERYTHING => failed.push((name.clone(), e)),
            Err(e) => skipped(name, e, &mut missing, &mut failed),
        }
    }
    if !missing.is_empty() {
        info += &format!(" (not plugged in: {})", missing.join(", "));
    }
    for (name, why) in &failed {
        info += &format!(" (couldn't open {name}: {why})");
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
    let mut profiles = Profiles::default();
    {
        let mut sh = shared.lock().map_err(es)?;
        // Auto level carries on where the last engine left off instead of starting at 0 dB.
        // ponytail: with Auto level off the meter reads 0, so it starts there; it climbs back
        // within a few seconds once turned on.
        chain.set_agc(sh.meters.agc_db);
        sh.new_profiles = Some(sh.profiles.clone());
    }
    let frame_in = rate as usize / 100;
    let mut acc = Vec::with_capacity(frame_in);
    let mut frame = [0.0f32; FRAME];
    let (mut params, mut params_ver) = (Params::default(), u64::MAX);
    let ref_n = echo.as_ref().map_or(0, |e| e.1 as usize / 100);
    let (mut ref_in, mut ref_out) = (vec![0.0f32; ref_n], vec![0.0f32; ref_n]);
    let mut starved = 0;
    let (mut rec, mut rec_pos) = (None::<Vec<f32>>, 0);
    let (mut mute_monitor, mut mute_sent) = (false, false);
    let mut ch_peak = [0.0f32; MAX_CH];
    let sh = shared.clone();

    streams.push(open_input(&mic, &mic_cfg, dev.input_channel, shared, move |mono, peaks, lag| {
        latency.capture_us.store(lag.as_micros() as u32, Relaxed);
        ch_peak.iter_mut().zip(peaks).for_each(|(a, p)| *a = a.max(*p));
        for &s in mono {
            acc.push(s);
            if acc.len() < frame_in {
                continue;
            }
            let t0 = Instant::now();
            if let Ok(mut sh) = sh.try_lock() {
                if sh.params_ver != params_ver {
                    params = sh.params.clone();
                    params_ver = sh.params_ver;
                }
                if let Some(mut p) = sh.new_profiles.take() {
                    std::mem::swap(&mut profiles, &mut p);
                    sh.old_profiles = Some(p);
                }
                if let Some(l) = sh.learn.take() {
                    chain.start_learning(l);
                }
                if let Some(l) = chain.take_learned() {
                    sh.learned = Some(l);
                }
                if let Some(buf) = sh.record.take() {
                    sh.discarded = rec.replace(buf);
                    rec_pos = 0;
                }
                // The GUI empties these when it reads them.
                sh.channel_peak.iter_mut().zip(std::mem::take(&mut ch_peak)).for_each(|(a, p)| *a = a.max(p));
                sh.record_progress = rec.as_ref().map(|r| rec_pos as f32 / r.len() as f32);
                if rec.as_ref().is_some_and(|r| rec_pos == r.len()) {
                    sh.recorded = rec.take();
                }
                (mute_monitor, mute_sent) = (sh.mute_monitor, sh.mute_sent);
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
                match (*is_monitor, mute_monitor, mute_sent) {
                    (true, true, _) => {}
                    (false, _, true) => {
                        o.push_slice(&[0.0; FRAME]);
                    }
                    _ => {
                        o.push_slice(&frame);
                    }
                }
            }
            let load = t0.elapsed().as_secs_f32() / 0.01;
            chain.meters.dsp_load += (load - chain.meters.dsp_load) * 0.02;
        }
    })?);

    Ok(Engine { _streams: streams, _workers: workers, info, shared_output, missing, failed })
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
    let stream = open_input(&spk, &cfg, None, shared, move |mono, _, _| {
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

/// Opens a capture stream in the device's native format and hands the callback mono f32 (one
/// `channel`, or all of them mixed), each channel's peak, and how long ago the device
/// captured it.
fn open_input(
    dev: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    channel: Option<u16>,
    shared: Arc<Mutex<Shared>>,
    f: impl FnMut(&[f32], &[f32; MAX_CH], Duration) + Send + 'static,
) -> Result<cpal::Stream, String> {
    fn build<T>(
        dev: &cpal::Device,
        sc: cpal::StreamConfig,
        channel: Option<u16>,
        mut f: impl FnMut(&[f32], &[f32; MAX_CH], Duration) + Send + 'static,
        err: impl FnMut(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let ch = sc.channels as usize;
        // A channel the device no longer has (set to fewer in Windows) mixes them all.
        let channel = channel.map(usize::from).filter(|&c| c < ch);
        let mut mono = Vec::new();
        dev.build_input_stream(
            sc,
            move |d: &[T], info: &cpal::InputCallbackInfo| {
                let mut peaks = [0.0f32; MAX_CH];
                mono.clear();
                mono.extend(d.chunks(ch).map(|fr| {
                    peaks.iter_mut().zip(fr).for_each(|(p, s)| *p = p.max(s.to_sample::<f32>().abs()));
                    match channel {
                        Some(c) => fr[c].to_sample::<f32>(),
                        None => fr.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32,
                    }
                }));
                let ts = info.timestamp();
                f(&mono, &peaks, ts.callback.duration_since(ts.capture));
            },
            err,
            None,
        )
    }
    let err = on_error(shared, dev.description().map(|d| d.name().to_string()).unwrap_or_default());
    let sc = cfg.config();
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(dev, sc, channel, f, err),
        cpal::SampleFormat::I16 => build::<i16>(dev, sc, channel, f, err),
        cpal::SampleFormat::I32 => build::<i32>(dev, sc, channel, f, err),
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
            let mut r = DriftReader { period: 480, ..Default::default() };
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

    /// VB-Cable's measured pattern: a 1056-sample first callback (the whole buffer), then 480s
    /// with an occasional late wake that asks for 960 at once. The cushion stays at 30 ms.
    #[test]
    fn whole_buffer_first_callback_does_not_grow_the_cushion() {
        let (mut prod, mut cons) = HeapRb::<f32>::new(SR as usize).split();
        let mut r = DriftReader { period: 480, ..Default::default() };
        let mut out = [0.0f32; 1056];
        r.read(&mut cons, &mut out);
        for tick in 1..3000 {
            prod.push_slice(&[0.25; 480]);
            match tick % 37 {
                0 => continue, // a late wake...
                1 => r.read(&mut cons, &mut out[..960]), // ...catches up with two periods
                _ => r.read(&mut cons, &mut out[..480]),
            }
            if tick > 10 {
                assert!(r.primed, "ran dry at tick {tick}");
            }
        }
        assert!((r.fill - 1440.0).abs() < 150.0, "fill {:.0}, want 1440", r.fill);
    }

    #[test]
    fn encode_writes_every_sample_width() {
        let mut out = [0u8; 2 * 2 * 2];
        encode(&[1.0, -0.5], 2, 2, false, &mut out);
        assert_eq!(&out[..4], &[0xFF, 0x7F, 0xFF, 0x7F], "16-bit full scale, both channels");
        assert_eq!(i16::from_le_bytes([out[4], out[5]]), -16384);
        let mut out = [0u8; 3];
        encode(&[0.5], 1, 3, false, &mut out);
        assert_eq!(i32::from_le_bytes([0, out[0], out[1], out[2]]) >> 8, 0x3F_FFFF, "24-bit half scale");
        let mut out = [0u8; 4];
        encode(&[0.25], 1, 4, true, &mut out);
        assert_eq!(f32::from_le_bytes(out), 0.25);
    }

    /// Needs real devices: `MC_EXCL=<output name> cargo test -- --ignored wasapi_streams --nocapture`.
    /// Opens the echo source (every app but this one) and, if named, an output in exclusive mode
    /// playing silence, for 2 s.
    #[test]
    #[ignore]
    fn wasapi_streams() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (_echo, cons, rate) = open_echo_everything(shared.clone()).expect("echo source");
        let excl = std::env::var("MC_EXCL").ok().map(|name| {
            let lat = Arc::new(Latency::default());
            let r = open_output_exclusive(&name, shared.clone(), lat.clone());
            println!("exclusive on {name}: {}", r.as_ref().map_or_else(|e| e.clone(), |_| "open".into()));
            (r.map(|(w, mut prod)| {
                prod.push_slice(&vec![0.0; SR as usize * 2]);
                w
            }), lat)
        });
        std::thread::sleep(Duration::from_secs(2));
        println!("echo source: {} samples at {rate} Hz in 2 s", cons.occupied_len());
        if let Some((Ok(_), lat)) = &excl {
            println!("exclusive delay {:.1} ms", lat.output_us.load(Relaxed) as f32 / 1000.0);
        }
        assert!(shared.lock().unwrap().error.is_none(), "{:?}", shared.lock().unwrap().error);
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
        let dev = Devices { input, output: output.clone(), echo_ref: Some(output), ..Devices::default() };
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
    let cfg = dev.default_output_config().map_err(es)?;
    let ch = cfg.channels();
    // cpal reports the shared-mode engine period here, in device frames.
    let period = match cfg.buffer_size() {
        cpal::SupportedBufferSize::Range { min, .. } if *min > 0 => *min as usize * SR as usize / cfg.sample_rate() as usize,
        _ => SR as usize / 100, // Windows' engine runs 10 ms periods
    };
    let (prod, mut cons) = HeapRb::<f32>::new(SR as usize).split();
    let mut mono = Vec::new();
    let mut reader = DriftReader { period, ..Default::default() };
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
    /// The device's steady callback size, in samples. Not the largest callback seen: the first
    /// one is the whole buffer (1056 on VB-Cable) and late wakes ask for two periods at once,
    /// but the mic keeps filling the ring through both, so neither needs a bigger cushion.
    period: usize,
    primed: bool,
}

impl DriftReader {
    fn read(&mut self, ring: &mut ringbuf::HeapCons<f32>, out: &mut [f32]) {
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

/// A WASAPI stream on its own thread, for what cpal can't open. Dropping it stops the thread.
pub struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type Ready = std::sync::mpsc::SyncSender<Result<(), String>>;

/// Runs `body` on its own thread; it sends Ok on `ready` once its stream runs, then loops until
/// `stop`. An error before that is returned from here (so `start` can fall back); one after it
/// asks the GUI to reconnect, like a cpal stream error.
fn spawn_worker(
    name: &str,
    shared: Arc<Mutex<Shared>>,
    body: impl FnOnce(&AtomicBool, &Ready) -> Result<(), String> + Send + 'static,
) -> Result<Worker, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let st = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = wasapi::initialize_mta();
            if let Err(e) = body(&st, &tx)
                && tx.try_send(Err(e.clone())).is_err()
                && let Ok(mut s) = shared.lock()
            {
                s.error = Some(e);
            }
        })
        .map_err(es)?;
    let worker = Worker { stop, thread: Some(thread) };
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(worker),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("it didn't start".into()),
    }
}

/// wasapi errors, with the ones people can fix in words.
fn wasapi_err(e: wasapi::WasapiError) -> String {
    let code = match &e {
        wasapi::WasapiError::Windows(w) => w.code().0 as u32,
        _ => 0,
    };
    match code {
        0x8889_000A => "another app is using it".into(), // AUDCLNT_E_DEVICE_IN_USE
        0x8889_000E => "exclusive control is turned off for it in Sound settings".into(), // AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED
        0x8889_0008 => "it doesn't take MicClean's format in exclusive mode".into(), // AUDCLNT_E_UNSUPPORTED_FORMAT
        _ => e.to_string(),
    }
}

/// What cpal does for its own audio threads: MMCSS "Pro Audio", so other work can't starve it.
fn pro_audio_priority() {
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut index = 0u32;
    // SAFETY: a NUL-terminated name that outlives the call; Windows drops the registration
    // when the thread ends.
    unsafe { windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
}

/// Mono f32 out as `ch` interleaved channels of `width`-byte samples: float, or integers
/// left-justified, so 24 valid bits in a 32-bit slot come out right too.
fn encode(mono: &[f32], ch: usize, width: usize, float: bool, out: &mut [u8]) {
    for (s, frame) in mono.iter().zip(out.chunks_exact_mut(ch * width)) {
        let le = if float { s.to_le_bytes() } else { ((s.clamp(-1.0, 1.0) as f64 * i32::MAX as f64) as i32).to_le_bytes() };
        for slot in frame.chunks_exact_mut(width) {
            slot.copy_from_slice(&le[4 - width..]);
        }
    }
}

/// The Discord feed in exclusive mode. Windows' mixer never sees it, so a screen share, which
/// records that mixer minus Discord itself, can't send your voice a second time; it also skips
/// the mixer's buffer. There's no Windows rate conversion in exclusive mode, so the device
/// has to run at 48 kHz.
fn open_output_exclusive(
    name: &str,
    shared: Arc<Mutex<Shared>>,
    latency: Arc<Latency>,
) -> Result<(Worker, ringbuf::HeapProd<f32>), String> {
    use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
    let (prod, mut cons) = HeapRb::<f32>::new(SR as usize).split();
    let name = name.to_string();
    let worker = spawn_worker("micclean exclusive output", shared, move |stop, ready| {
        let dev = wasapi::DeviceEnumerator::new()
            .and_then(|e| e.get_device_collection(&Direction::Render))
            .and_then(|c| c.get_device_with_name(&name))
            .map_err(wasapi_err)?;
        let mut client = dev.get_iaudioclient().map_err(wasapi_err)?;
        let mix = client.get_mixformat().map_err(wasapi_err)?;
        if mix.get_samplespersec() != SR as u32 {
            return Err(format!("it runs at {} Hz; set it to 48000 Hz in Sound settings", mix.get_samplespersec()));
        }
        let kinds = [(32, 32, SampleType::Float), (32, 32, SampleType::Int), (32, 24, SampleType::Int), (24, 24, SampleType::Int), (16, 16, SampleType::Int)];
        let fmt = kinds
            .iter()
            .find_map(|(store, valid, kind)| {
                let want = WaveFormat::new(*store, *valid, kind, SR as usize, mix.get_nchannels() as usize, None);
                client.is_supported_exclusive_with_quirks(&want).ok()
            })
            .ok_or_else(|| "it doesn't take a 48000 Hz format in exclusive mode".to_string())?;
        let (period, _) = client.get_device_period().map_err(wasapi_err)?;
        if let Err(e) = client.initialize_client(&fmt, &Direction::Render, &StreamMode::EventsExclusive { period_hns: period }) {
            // AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: Windows names the size it wants; start over with it.
            if !matches!(&e, wasapi::WasapiError::Windows(w) if w.code().0 as u32 == 0x8889_0019) {
                return Err(wasapi_err(e));
            }
            let period = wasapi::calculate_period_100ns(client.get_buffer_size().map_err(wasapi_err)? as i64, SR as i64);
            client = dev.get_iaudioclient().map_err(wasapi_err)?;
            client.initialize_client(&fmt, &Direction::Render, &StreamMode::EventsExclusive { period_hns: period }).map_err(wasapi_err)?;
        }
        let event = client.set_get_eventhandle().map_err(wasapi_err)?;
        let render = client.get_audiorenderclient().map_err(wasapi_err)?;
        // Exclusive and event driven: each wake fills the whole buffer.
        let frames = client.get_buffer_size().map_err(wasapi_err)? as usize;
        let (ch, width) = (fmt.get_nchannels() as usize, fmt.get_bitspersample() as usize / 8);
        let float = matches!(fmt.get_subformat(), Ok(SampleType::Float));
        let mut mono = vec![0.0f32; frames];
        let mut bytes = vec![0u8; frames * ch * width];
        let mut reader = DriftReader { period: frames, ..Default::default() };
        render.write_to_device(frames, &bytes, None).map_err(wasapi_err)?; // silence until the ring fills
        client.start_stream().map_err(wasapi_err)?;
        let _ = ready.send(Ok(()));
        pro_audio_priority();
        let mut waits = 0;
        while !stop.load(Relaxed) {
            if event.wait_for_event(100).is_err() {
                waits += 1;
                if waits > 20 {
                    return Err(format!("{name} stopped asking for sound"));
                }
                continue;
            }
            waits = 0;
            reader.read(&mut cons, &mut mono);
            encode(&mono, ch, width, float, &mut bytes);
            render.write_to_device(frames, &bytes, None).map_err(wasapi_err)?;
            latency.output_us.store(((frames as f64 + reader.fill) / SR as f64 * 1e6) as u32, Relaxed);
        }
        let _ = client.stop_stream();
        Ok(())
    })?;
    Ok((worker, prod))
}

/// The echo source for EVERYTHING: what every app plays except this one (Windows' per-app
/// loopback with this process excluded), as 48 kHz mono.
fn open_echo_everything(shared: Arc<Mutex<Shared>>) -> Result<(Worker, ringbuf::HeapCons<f32>, u32), String> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
    let (mut prod, cons) = HeapRb::<f32>::new(SR as usize).split();
    let worker = spawn_worker("micclean echo source", shared, move |stop, ready| {
        // false: every process except this one and the ones it started.
        let mut client = AudioClient::new_application_loopback_client(std::process::id(), false).map_err(wasapi_err)?;
        let fmt = WaveFormat::new(32, 32, &SampleType::Float, SR as usize, 1, None);
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
        client.initialize_client(&fmt, &Direction::Capture, &mode).map_err(wasapi_err)?;
        let event = client.set_get_eventhandle().map_err(wasapi_err)?;
        let capture = client.get_audiocaptureclient().map_err(wasapi_err)?;
        client.start_stream().map_err(wasapi_err)?;
        let _ = ready.send(Ok(()));
        let (mut bytes, mut mono) = (vec![0u8; SR as usize * 4], vec![0.0f32; SR as usize]);
        while !stop.load(Relaxed) {
            if event.wait_for_event(100).is_err() {
                continue; // nothing playing; the echo canceller fills in silence itself
            }
            while let Some(n) = capture.get_next_packet_size().map_err(wasapi_err)?.filter(|&n| n > 0) {
                let n = (n as usize).min(mono.len());
                let (got, info) = capture.read_from_device(&mut bytes[..n * 4]).map_err(wasapi_err)?;
                let got = got as usize;
                for (m, b) in mono[..got].iter_mut().zip(bytes.chunks_exact(4)) {
                    *m = if info.flags.silent { 0.0 } else { f32::from_le_bytes([b[0], b[1], b[2], b[3]]) };
                }
                prod.push_slice(&mono[..got]);
            }
        }
        let _ = client.stop_stream();
        Ok(())
    })?;
    Ok((worker, cons, SR as u32))
}
