//! Audio input. One dedicated thread owns the cpal stream (not `Send` on macOS),
//! drains the lock-free ring the callback fills, keeps a pre-roll, writes the
//! spool file while recording and produces meter / waveform data.
//!
//! The audio callback never allocates, locks or touches the disk.

use crate::config::Preset;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use crossbeam_channel::{Receiver, Sender};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Waveform resolution: bins per second.
pub const BINS_PER_SEC: u32 = 100;

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    pub rates: Vec<u32>,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct StreamSpec {
    pub preset: Preset,
}

pub enum AudioCmd {
    Configure(StreamSpec),
    StartRec { raw_path: PathBuf },
    StopRec,
    Rescan,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AudioState {
    Opening,
    Ready,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioStatus {
    pub state: AudioState,
    pub message: String,
    pub device: String,
    pub sample_rate: u32,
    pub device_channels: u16,
    /// 0-based device channels feeding the tracks.
    pub selected: Vec<usize>,
    pub dropouts: u64,
}

pub enum AudioEvent {
    Status(AudioStatus),
    Devices(Vec<DeviceInfo>),
    Live {
        /// Peak per device channel since the last event (linear 0..1).
        meters: Vec<f32>,
        /// Per selected channel: new waveform bins (peak, rms), 0..255.
        bins: Vec<Vec<[u8; 2]>>,
        rec_frames: u64,
    },
    RecStarted { preroll_frames: u64 },
    RecStopped { raw_path: PathBuf, frames: u64, interrupted: bool, error: Option<String> },
}

pub fn spawn(events: Sender<AudioEvent>) -> Sender<AudioCmd> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("psr-audio".into())
        .spawn(move || Manager::new(events, rx).run())
        .expect("audio thread");
    tx
}

pub fn list_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok()).unwrap_or_default();
    let mut out = Vec::new();
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            let Ok(name) = d.name() else { continue };
            let mut channels = 0u16;
            let mut rates = Vec::new();
            if let Ok(cfgs) = d.supported_input_configs() {
                for c in cfgs {
                    channels = channels.max(c.channels());
                    for r in [44100u32, 48000, 88200, 96000, 176400, 192000] {
                        if c.min_sample_rate().0 <= r && r <= c.max_sample_rate().0 && !rates.contains(&r) {
                            rates.push(r);
                        }
                    }
                }
            }
            if channels == 0 {
                continue;
            }
            rates.sort_unstable();
            out.push(DeviceInfo { is_default: name == default_name, name, channels, rates });
        }
    }
    out
}

struct Live {
    _stream: cpal::Stream,
    cons: rtrb::Consumer<f32>,
    failed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    channels: usize,
    rate: u32,
    selected: Vec<usize>,
    device: String,
}

struct Rec {
    raw_path: PathBuf,
    out: BufWriter<File>,
    frames: u64,
    error: Option<String>,
}

struct Manager {
    ev: Sender<AudioEvent>,
    rx: Receiver<AudioCmd>,
    spec: Option<StreamSpec>,
    live: Option<Live>,
    rec: Option<Rec>,
    preroll: VecDeque<f32>,
    preroll_cap: usize,
    // metering / bins
    meters: Vec<f32>,
    bin_pk: Vec<f32>,
    bin_sq: Vec<f64>,
    bin_n: u32,
    bin_len: u32,
    bins: Vec<Vec<[u8; 2]>>,
    frame: Vec<f32>,
    last_emit: Instant,
    last_data: Instant,
    next_retry: Instant,
    last_scan: Instant,
    status_msg: String,
}

impl Manager {
    fn new(ev: Sender<AudioEvent>, rx: Receiver<AudioCmd>) -> Self {
        let now = Instant::now();
        Manager {
            ev,
            rx,
            spec: None,
            live: None,
            rec: None,
            preroll: VecDeque::new(),
            preroll_cap: 0,
            meters: vec![],
            bin_pk: vec![],
            bin_sq: vec![],
            bin_n: 0,
            bin_len: 480,
            bins: vec![],
            frame: vec![],
            last_emit: now,
            last_data: now,
            next_retry: now,
            last_scan: now - Duration::from_secs(60),
            status_msg: String::new(),
        }
    }

    fn run(mut self) {
        loop {
            while let Ok(cmd) = self.rx.try_recv() {
                self.handle(cmd);
            }
            let now = Instant::now();
            if self.rec.is_none() && now.duration_since(self.last_scan) > Duration::from_secs(5) {
                self.scan();
            }
            if self.live.is_none() && self.spec.is_some() && now >= self.next_retry {
                self.open();
            }
            let got = self.drain();
            if let Some(l) = &self.live {
                let dead = l.failed.load(Ordering::Relaxed) || (!got && now.duration_since(self.last_data) > Duration::from_secs(3));
                if dead {
                    let msg = if l.failed.load(Ordering::Relaxed) { "Input device disconnected" } else { "Input device stopped delivering audio" };
                    self.fail(msg);
                }
            }
            if now.duration_since(self.last_emit) >= Duration::from_millis(33) {
                self.emit_live();
            }
            if !got {
                std::thread::sleep(Duration::from_millis(4));
            }
        }
    }

    fn handle(&mut self, cmd: AudioCmd) {
        match cmd {
            AudioCmd::Configure(spec) => {
                let same = self.spec.as_ref().map(|s| stream_key(&s.preset) == stream_key(&spec.preset)).unwrap_or(false);
                self.spec = Some(spec);
                if !same && self.rec.is_none() {
                    self.live = None;
                    self.next_retry = Instant::now();
                } else if let Some(l) = &mut self.live {
                    // same device/rate: just re-route channels (+ pre-roll length)
                    let sel = self.spec.as_ref().unwrap().preset.selected_channels(l.channels as u16);
                    if self.rec.is_none() && sel != l.selected {
                        l.selected = sel;
                        self.reset_dsp();
                        self.send_status();
                    }
                }
                self.update_preroll_cap();
            }
            AudioCmd::StartRec { raw_path } => self.start_rec(raw_path),
            AudioCmd::StopRec => self.stop_rec(false),
            AudioCmd::Rescan => self.scan(),
        }
    }

    fn scan(&mut self) {
        self.last_scan = Instant::now();
        let _ = self.ev.send(AudioEvent::Devices(list_devices()));
    }

    fn update_preroll_cap(&mut self) {
        if let (Some(l), Some(s)) = (&self.live, &self.spec) {
            self.preroll_cap = (s.preset.preroll_secs.clamp(0.0, 30.0) * l.rate as f32) as usize * l.selected.len();
            while self.preroll.len() > self.preroll_cap {
                self.preroll.pop_front();
            }
        }
    }

    fn reset_dsp(&mut self) {
        let Some(l) = &self.live else { return };
        let n = l.selected.len();
        self.meters = vec![0.0; l.channels];
        self.bin_pk = vec![0.0; n];
        self.bin_sq = vec![0.0; n];
        self.bins = vec![Vec::new(); n];
        self.bin_n = 0;
        self.bin_len = (l.rate / BINS_PER_SEC).max(1);
        self.frame = vec![0.0; n];
        self.preroll.clear();
        self.update_preroll_cap();
    }

    fn status(&self, state: AudioState) -> AudioStatus {
        match &self.live {
            Some(l) => AudioStatus {
                state,
                message: self.status_msg.clone(),
                device: l.device.clone(),
                sample_rate: l.rate,
                device_channels: l.channels as u16,
                selected: l.selected.clone(),
                dropouts: l.dropped.load(Ordering::Relaxed),
            },
            None => AudioStatus {
                state,
                message: self.status_msg.clone(),
                device: self.spec.as_ref().map(|s| s.preset.device.clone()).unwrap_or_default(),
                sample_rate: 0,
                device_channels: 0,
                selected: vec![],
                dropouts: 0,
            },
        }
    }

    fn send_status(&self) {
        let st = if self.live.is_some() { AudioState::Ready } else if self.spec.is_some() { AudioState::Error } else { AudioState::Opening };
        let _ = self.ev.send(AudioEvent::Status(self.status(st)));
    }

    fn fail(&mut self, msg: &str) {
        log::warn!("audio: {msg}");
        if self.rec.is_some() {
            self.stop_rec(true);
        }
        self.live = None;
        self.status_msg = msg.to_string();
        self.next_retry = Instant::now() + Duration::from_secs(2);
        self.send_status();
    }

    fn open(&mut self) {
        let spec = self.spec.clone().unwrap();
        let _ = self.ev.send(AudioEvent::Status(AudioStatus { state: AudioState::Opening, ..self.status(AudioState::Opening) }));
        match open_stream(&spec.preset) {
            Ok(live) => {
                log::info!("audio: opened '{}' {} ch @ {} Hz", live.device, live.channels, live.rate);
                self.status_msg = if live.rate != spec.preset.sample_rate {
                    format!("Device runs at {} Hz (preset wants {})", live.rate, spec.preset.sample_rate)
                } else {
                    String::new()
                };
                self.live = Some(live);
                self.last_data = Instant::now();
                self.reset_dsp();
                self.send_status();
            }
            Err(e) => {
                self.status_msg = e;
                self.next_retry = Instant::now() + Duration::from_secs(2);
                self.send_status();
            }
        }
    }

    /// Drain the ring. Returns true if any audio arrived.
    fn drain(&mut self) -> bool {
        let Some(l) = &mut self.live else { return false };
        let ch = l.channels;
        let avail = (l.cons.slots() / ch) * ch;
        if avail == 0 {
            return false;
        }
        let chunk = match l.cons.read_chunk(avail) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let (a, b) = chunk.as_slices();
        let mut data = Vec::with_capacity(avail);
        data.extend_from_slice(a);
        data.extend_from_slice(b);
        chunk.commit_all();
        self.last_data = Instant::now();
        let selected = l.selected.clone();
        self.process(&data, ch, &selected);
        true
    }

    fn process(&mut self, data: &[f32], ch: usize, selected: &[usize]) {
        let n = selected.len();
        let mut rec_buf: Vec<u8> = if self.rec.is_some() { Vec::with_capacity(data.len() / ch * n * 4) } else { Vec::new() };
        for fr in data.chunks_exact(ch) {
            for (m, &s) in self.meters.iter_mut().zip(fr) {
                let a = s.abs();
                if a > *m {
                    *m = a;
                }
            }
            for (i, &c) in selected.iter().enumerate() {
                let s = fr[c];
                self.frame[i] = s;
                let a = s.abs();
                if a > self.bin_pk[i] {
                    self.bin_pk[i] = a;
                }
                self.bin_sq[i] += (s * s) as f64;
            }
            self.bin_n += 1;
            if self.bin_n >= self.bin_len {
                for i in 0..n {
                    let rms = (self.bin_sq[i] / self.bin_n as f64).sqrt() as f32;
                    self.bins[i].push([to_u8(self.bin_pk[i]), to_u8(rms)]);
                    self.bin_pk[i] = 0.0;
                    self.bin_sq[i] = 0.0;
                }
                self.bin_n = 0;
            }
            if self.rec.is_some() {
                for &s in &self.frame {
                    rec_buf.extend_from_slice(&s.to_le_bytes());
                }
            } else if self.preroll_cap > 0 {
                for &s in &self.frame {
                    if self.preroll.len() >= self.preroll_cap {
                        self.preroll.pop_front();
                    }
                    self.preroll.push_back(s);
                }
            }
        }
        if let Some(r) = &mut self.rec {
            if r.error.is_none() {
                match r.out.write_all(&rec_buf) {
                    Ok(()) => r.frames += (rec_buf.len() / 4 / n.max(1)) as u64,
                    Err(e) => r.error = Some(format!("Spool write failed: {e}")),
                }
            }
        }
    }

    fn emit_live(&mut self) {
        self.last_emit = Instant::now();
        if self.live.is_none() {
            return;
        }
        let n = self.meters.len();
        let meters = std::mem::replace(&mut self.meters, vec![0.0; n]);
        let bins = self.bins.iter_mut().map(std::mem::take).collect();
        let rec_frames = self.rec.as_ref().map(|r| r.frames).unwrap_or(0);
        let _ = self.ev.send(AudioEvent::Live { meters, bins, rec_frames });
        if let Some(l) = &self.live {
            static LAST: AtomicU64 = AtomicU64::new(0);
            let d = l.dropped.load(Ordering::Relaxed);
            if LAST.swap(d, Ordering::Relaxed) != d {
                self.send_status();
            }
        }
    }

    fn start_rec(&mut self, raw_path: PathBuf) {
        if self.rec.is_some() {
            return;
        }
        if self.live.is_none() {
            let _ = self.ev.send(AudioEvent::RecStopped { raw_path, frames: 0, interrupted: true, error: Some("No input device".into()) });
            return;
        }
        let n = self.live.as_ref().unwrap().selected.len();
        let file = std::fs::create_dir_all(raw_path.parent().unwrap()).and_then(|_| File::create(&raw_path));
        match file {
            Ok(f) => {
                let mut out = BufWriter::with_capacity(1 << 20, f);
                let pre: Vec<u8> = self.preroll.iter().flat_map(|s| s.to_le_bytes()).collect();
                let preroll_frames = (self.preroll.len() / n.max(1)) as u64;
                let error = out.write_all(&pre).err().map(|e| e.to_string());
                self.preroll.clear();
                self.rec = Some(Rec { raw_path, out, frames: preroll_frames, error });
                let _ = self.ev.send(AudioEvent::RecStarted { preroll_frames });
            }
            Err(e) => {
                let _ = self.ev.send(AudioEvent::RecStopped { raw_path, frames: 0, interrupted: true, error: Some(format!("Cannot create spool file: {e}")) });
            }
        }
    }

    fn stop_rec(&mut self, interrupted: bool) {
        let Some(mut r) = self.rec.take() else { return };
        // pull whatever the callback already delivered
        if !interrupted {
            self.rec = Some(r);
            self.drain();
            r = self.rec.take().unwrap();
        }
        let mut error = r.error.take();
        if let Err(e) = r.out.flush().and_then(|_| r.out.get_ref().sync_all()) {
            error.get_or_insert(format!("Spool flush failed: {e}"));
        }
        let _ = self.ev.send(AudioEvent::RecStopped { raw_path: r.raw_path, frames: r.frames, interrupted, error });
    }
}

fn stream_key(p: &Preset) -> (String, u32) {
    (p.device.clone(), p.sample_rate)
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn open_stream(p: &Preset) -> Result<Live, String> {
    let host = cpal::default_host();
    let device = if p.device.is_empty() {
        host.default_input_device().ok_or("No input device available")?
    } else {
        host.input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| n == p.device).unwrap_or(false))
            .ok_or_else(|| format!("'{}' not connected", p.device))?
    };
    let name = device.name().unwrap_or_else(|_| "Unknown device".into());
    let cfgs: Vec<_> = device.supported_input_configs().map_err(|e| e.to_string())?.collect();
    if cfgs.is_empty() {
        return Err(format!("'{name}' has no input configurations"));
    }
    let fmt_rank = |f: cpal::SampleFormat| match f {
        cpal::SampleFormat::F32 => 0,
        cpal::SampleFormat::I32 => 1,
        cpal::SampleFormat::I16 => 2,
        _ => 3,
    };
    let want = p.sample_rate;
    let best = cfgs
        .iter()
        .filter(|c| c.min_sample_rate().0 <= want && want <= c.max_sample_rate().0)
        .max_by_key(|c| (c.channels(), -fmt_rank(c.sample_format())))
        .map(|c| (*c).with_sample_rate(cpal::SampleRate(want)));
    let supported = match best {
        Some(s) => s,
        None => device.default_input_config().map_err(|e| e.to_string())?,
    };
    let config: cpal::StreamConfig = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let (prod, cons) = rtrb::RingBuffer::<f32>::new(rate as usize * channels * 8);
    let failed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::I8 => build::<i8>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::U8 => build::<u8>(&device, &config, prod, failed.clone(), dropped.clone()),
        cpal::SampleFormat::F64 => build::<f64>(&device, &config, prod, failed.clone(), dropped.clone()),
        f => return Err(format!("Unsupported sample format {f:?}")),
    }
    .map_err(|e| format!("Cannot open '{name}': {e}"))?;
    stream.play().map_err(|e| format!("Cannot start '{name}': {e}"))?;
    let selected = p.selected_channels(channels as u16);
    Ok(Live { _stream: stream, cons, failed, dropped, channels, rate, selected, device: name })
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut prod: rtrb::Producer<f32>,
    failed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let failed2 = failed.clone();
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let n = data.len().min(prod.slots());
            if n < data.len() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
            // keep frames whole: only write if everything fits
            if n == data.len() {
                if let Ok(mut chunk) = prod.write_chunk_uninit(n) {
                    let (a, b) = chunk.as_mut_slices();
                    let mut it = data.iter();
                    for slot in a.iter_mut().chain(b.iter_mut()) {
                        slot.write(f32::from_sample(*it.next().unwrap()));
                    }
                    unsafe { chunk.commit_all() };
                }
            }
        },
        move |err| {
            log::error!("audio stream error: {err}");
            failed2.store(true, Ordering::Relaxed);
        },
        None,
    )
}
