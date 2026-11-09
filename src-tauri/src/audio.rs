//! Audio input. One dedicated thread owns the cpal stream (not `Send` on macOS),
//! drains the lock-free ring the callback fills, keeps a pre-roll, writes the
//! spool file while recording and produces meter / waveform data.
//!
//! The audio callback never allocates, locks or touches the disk.

use crate::config::Preset;
use cpal::FromSample;
use crossbeam_channel::{Receiver, Sender};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
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

/// Where audio comes from. The real one is cpal ([`crate::device::CpalBackend`]);
/// tests plug in a fake that feeds samples through the very same ring buffer.
pub trait Backend: Send + 'static {
    fn devices(&self) -> Vec<DeviceInfo>;
    fn open(&self, preset: &Preset) -> Result<Input, String>;
}

/// An open input stream, as handed over by a [`Backend`].
pub struct Input {
    /// Keeps the stream alive. Dropped = closed.
    pub guard: Box<dyn std::any::Any>,
    /// Interleaved f32 samples, `channels` per frame.
    pub cons: rtrb::Consumer<f32>,
    /// Set by the stream's error callback.
    pub failed: Arc<AtomicBool>,
    /// Counts callbacks that did not fit into the ring.
    pub dropped: Arc<AtomicU64>,
    pub channels: usize,
    pub rate: u32,
    pub device: String,
}

pub fn spawn(events: Sender<AudioEvent>, backend: impl Backend) -> Sender<AudioCmd> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("psr-audio".into())
        .spawn(move || Manager::new(events, rx, Box::new(backend)).run())
        .expect("audio thread");
    tx
}

/// Body of the realtime callback: copy one buffer into the ring, whole or not at
/// all (a torn buffer would shift every channel). Never allocates or blocks.
pub fn push_input<T: Copy>(prod: &mut rtrb::Producer<f32>, data: &[T], dropped: &AtomicU64)
where
    f32: FromSample<T>,
{
    if data.len() > prod.slots() {
        dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if let Ok(mut chunk) = prod.write_chunk_uninit(data.len()) {
        let (a, b) = chunk.as_mut_slices();
        for (slot, &x) in a.iter_mut().chain(b.iter_mut()).zip(data) {
            slot.write(f32::from_sample_(x));
        }
        // SAFETY: zip covered every slot, data.len() == chunk length.
        unsafe { chunk.commit_all() };
    }
}

struct Live {
    _guard: Box<dyn std::any::Any>,
    cons: rtrb::Consumer<f32>,
    failed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    channels: usize,
    rate: u32,
    selected: Vec<usize>,
    device: String,
}

/// The spool file a take is written to while recording.
pub trait Spool: Write + Send {
    /// Make everything written so far survive a power cut.
    fn sync(&mut self) -> std::io::Result<()>;
}

impl Spool for File {
    fn sync(&mut self) -> std::io::Result<()> {
        crate::config::sync_file(self)
    }
}

fn open_spool(path: &Path) -> std::io::Result<Box<dyn Spool>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(Box::new(File::create(path)?))
}

struct Rec {
    raw_path: PathBuf,
    out: BufWriter<Box<dyn Spool>>,
    frames: u64,
    error: Option<String>,
}

struct Manager {
    /// The loop's clock, set once per tick.
    now: Instant,
    ev: Sender<AudioEvent>,
    rx: Receiver<AudioCmd>,
    backend: Box<dyn Backend>,
    spool_fn: fn(&Path) -> std::io::Result<Box<dyn Spool>>,
    last_dropped: u64,
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
    fn new(ev: Sender<AudioEvent>, rx: Receiver<AudioCmd>, backend: Box<dyn Backend>) -> Self {
        let now = Instant::now();
        Manager {
            now,
            ev,
            rx,
            backend,
            spool_fn: open_spool,
            last_dropped: 0,
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
            match self.tick(Instant::now()) {
                Tick::Exit => return,
                Tick::Idle => std::thread::sleep(Duration::from_millis(4)),
                Tick::Busy => {}
            }
        }
    }

    /// One pass of the audio thread loop.
    fn tick(&mut self, now: Instant) -> Tick {
        self.now = now;
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => self.handle(cmd),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // engine gone (app quitting): don't lose the take in flight
                    self.stop_rec(false);
                    return Tick::Exit;
                }
            }
        }
        if self.rec.is_none() && now.saturating_duration_since(self.last_scan) > Duration::from_secs(5) {
            self.scan();
        }
        if self.live.is_none() && self.spec.is_some() && now >= self.next_retry {
            self.open();
        }
        let got = self.drain();
        if self.rec.as_ref().is_some_and(|r| r.error.is_some()) {
            // disk full / USB stick pulled: end the take now so the UI says so,
            // instead of "recording" into nothing for the rest of the song
            self.stop_rec(true);
        }
        if let Some(l) = &self.live {
            let failed = l.failed.load(Ordering::Relaxed);
            if failed || (!got && now.saturating_duration_since(self.last_data) > Duration::from_secs(3)) {
                self.fail(if failed { "Input device disconnected" } else { "Input device stopped delivering audio" });
            }
        }
        if now.saturating_duration_since(self.last_emit) >= Duration::from_millis(33) {
            self.emit_live();
        }
        if got { Tick::Busy } else { Tick::Idle }
    }

    fn handle(&mut self, cmd: AudioCmd) {
        match cmd {
            AudioCmd::Configure(spec) => {
                let same = self.spec.as_ref().map(|s| stream_key(&s.preset) == stream_key(&spec.preset)).unwrap_or(false);
                self.spec = Some(spec);
                if !same && self.rec.is_none() {
                    self.live = None;
                    self.next_retry = self.now;
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
        self.last_scan = self.now;
        let _ = self.ev.send(AudioEvent::Devices(self.backend.devices()));
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
        self.next_retry = self.now + Duration::from_secs(2);
        self.send_status();
    }

    fn open(&mut self) {
        let spec = self.spec.clone().unwrap();
        let _ = self.ev.send(AudioEvent::Status(AudioStatus { state: AudioState::Opening, ..self.status(AudioState::Opening) }));
        match self.backend.open(&spec.preset) {
            Ok(i) => {
                let selected = spec.preset.selected_channels(i.channels as u16);
                let live = Live { _guard: i.guard, cons: i.cons, failed: i.failed, dropped: i.dropped, channels: i.channels.max(1), rate: i.rate.max(1), selected, device: i.device };
                log::info!("audio: opened '{}' {} ch @ {} Hz", live.device, live.channels, live.rate);
                self.status_msg = if live.rate != spec.preset.sample_rate {
                    format!("Device runs at {} Hz (preset wants {})", live.rate, spec.preset.sample_rate)
                } else {
                    String::new()
                };
                self.live = Some(live);
                self.last_data = self.now;
                self.reset_dsp();
                self.send_status();
            }
            Err(e) => {
                self.status_msg = e;
                self.next_retry = self.now + Duration::from_secs(2);
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
        self.last_data = self.now;
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
        self.last_emit = self.now;
        if self.live.is_none() {
            return;
        }
        let n = self.meters.len();
        let meters = std::mem::replace(&mut self.meters, vec![0.0; n]);
        let bins = self.bins.iter_mut().map(std::mem::take).collect();
        let rec_frames = self.rec.as_ref().map(|r| r.frames).unwrap_or(0);
        let _ = self.ev.send(AudioEvent::Live { meters, bins, rec_frames });
        if let Some(l) = &self.live {
            let d = l.dropped.load(Ordering::Relaxed);
            if std::mem::replace(&mut self.last_dropped, d) != d {
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
        let file = (self.spool_fn)(&raw_path);
        match file {
            Ok(f) => {
                let mut out = BufWriter::with_capacity(1 << 20, f);
                let pre: Vec<u8> = self.preroll.iter().flat_map(|s| s.to_le_bytes()).collect();
                let preroll_frames = (self.preroll.len() / n.max(1)) as u64;
                let error = out.write_all(&pre).err().map(|e| format!("Spool write failed: {e}"));
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
        if let Err(e) = r.out.flush().and_then(|_| r.out.get_mut().sync()) {
            error.get_or_insert(format!("Spool flush failed: {e}"));
        }
        let _ = self.ev.send(AudioEvent::RecStopped { raw_path: r.raw_path, frames: r.frames, interrupted, error });
    }
}

#[derive(Debug, PartialEq)]
enum Tick {
    Busy,
    Idle,
    Exit,
}

fn stream_key(p: &Preset) -> (String, u32) {
    (p.device.clone(), p.sample_rate)
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::testutil::{Fake, TempDir};

    struct Rig {
        m: Manager,
        fake: Fake,
        cmd: Sender<AudioCmd>,
        ev: Receiver<AudioEvent>,
        t: Instant,
        dir: TempDir,
    }

    impl Rig {
        fn new(channels: usize, rate: u32) -> Rig {
            let fake = Fake::new(channels, rate);
            let (ev_tx, ev) = crossbeam_channel::unbounded();
            let (cmd, rx) = crossbeam_channel::unbounded();
            let m = Manager::new(ev_tx, rx, Box::new(fake.clone()));
            Rig { m, fake, cmd, ev, t: Instant::now(), dir: TempDir::new("audio") }
        }
        fn configure(&mut self, p: Preset) {
            self.cmd.send(AudioCmd::Configure(StreamSpec { preset: p })).unwrap();
            self.tick(0);
        }
        /// Advance the fake clock and run one loop pass.
        fn tick(&mut self, ms: u64) -> Tick {
            self.t += Duration::from_millis(ms);
            self.m.tick(self.t)
        }
        fn send(&mut self, c: AudioCmd) {
            self.cmd.send(c).unwrap();
            self.tick(0);
        }
        fn events(&self) -> Vec<AudioEvent> {
            self.ev.try_iter().collect()
        }
        fn statuses(&self) -> Vec<AudioStatus> {
            self.events().into_iter().filter_map(|e| if let AudioEvent::Status(s) = e { Some(s) } else { None }).collect()
        }
        fn raw(&self, name: &str) -> PathBuf {
            self.dir.join(format!("spool/{name}.raw"))
        }
    }

    fn preset(mode: Mode, channels: Vec<u16>, preroll: f32) -> Preset {
        Preset { mode, channels, preroll_secs: preroll, sample_rate: 1000, ..Preset::default() }
    }

    fn floats(path: &Path) -> Vec<f32> {
        std::fs::read(path).unwrap().chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
    }

    /// Stereo frames, left = i, right = -i (scaled), starting at `from`.
    fn frames(from: usize, n: usize) -> Vec<f32> {
        (from..from + n).flat_map(|i| [i as f32 / 1e5, -(i as f32) / 1e5]).collect()
    }

    #[test]
    fn opens_on_configure_and_reports_ready() {
        let mut r = Rig::new(4, 1000);
        r.configure(preset(Mode::Stereo, vec![3, 4], 0.0));
        let st = r.statuses();
        assert_eq!(st.first().unwrap().state, AudioState::Opening);
        let last = st.last().unwrap();
        assert_eq!(last.state, AudioState::Ready);
        assert_eq!(last.selected, vec![2, 3]);
        assert_eq!((last.device.as_str(), last.sample_rate, last.device_channels), ("Fake", 1000, 4));
        assert_eq!(last.message, "");
        assert_eq!(r.fake.opens(), 1);
    }

    #[test]
    fn says_when_device_rate_differs() {
        let mut r = Rig::new(2, 44100);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        assert_eq!(r.statuses().last().unwrap().message, "Device runs at 44100 Hz (preset wants 1000)");
    }

    #[test]
    fn open_failure_retries_every_two_seconds() {
        let mut r = Rig::new(2, 1000);
        r.fake.0.lock().fail = Some("'Scarlett' not connected".into());
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        let st = r.statuses();
        assert_eq!(st.last().unwrap().state, AudioState::Error);
        assert_eq!(st.last().unwrap().message, "'Scarlett' not connected");
        assert_eq!(st.last().unwrap().sample_rate, 0);
        r.tick(1000);
        assert_eq!(r.fake.opens(), 1, "no hammering");
        r.fake.0.lock().fail = None;
        r.tick(1100);
        assert_eq!(r.fake.opens(), 2);
        assert_eq!(r.statuses().last().unwrap().state, AudioState::Ready);
    }

    #[test]
    fn nothing_happens_before_configure() {
        let mut r = Rig::new(2, 1000);
        r.m.last_scan = r.t; // skip the device scan
        assert_eq!(r.tick(100), Tick::Idle);
        assert_eq!(r.fake.opens(), 0);
        r.m.send_status();
        assert_eq!(r.statuses()[0].state, AudioState::Opening);
    }

    #[test]
    fn scans_devices_on_start_every_5s_and_on_request() {
        let mut r = Rig::new(2, 1000);
        r.fake.0.lock().devices = vec![DeviceInfo { name: "A".into(), channels: 2, rates: vec![48000], is_default: true }];
        r.tick(0);
        let n = |r: &Rig| r.events().into_iter().filter(|e| matches!(e, AudioEvent::Devices(d) if d[0].name == "A")).count();
        assert_eq!(n(&r), 1);
        r.tick(4000);
        assert_eq!(n(&r), 0);
        r.tick(1100);
        assert_eq!(n(&r), 1);
        r.send(AudioCmd::Rescan);
        assert_eq!(n(&r), 1);
    }

    #[test]
    fn meters_and_waveform_bins() {
        let mut r = Rig::new(3, 1000);
        r.configure(preset(Mode::Multitrack, vec![1, 3], 0.0));
        r.events();
        // 10 frames per bin at 1000 Hz. ch0 = 0.5 constant, ch1 = 0.9 (not selected), ch2 = alternating ±1
        let data: Vec<f32> = (0..25).flat_map(|i| [0.5, -0.9, if i % 2 == 0 { 1.0 } else { -1.0 }]).collect();
        r.fake.feed(&data);
        assert_eq!(r.tick(40), Tick::Busy);
        let live: Vec<_> = r.events().into_iter().filter_map(|e| if let AudioEvent::Live { meters, bins, rec_frames } = e { Some((meters, bins, rec_frames)) } else { None }).collect();
        assert_eq!(live.len(), 1);
        let (meters, bins, rec_frames) = &live[0];
        assert_eq!(meters, &vec![0.5, 0.9, 1.0], "peak per device channel, abs");
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0], vec![[128, 128]; 2], "two full bins, 5 frames pending");
        assert_eq!(bins[1], vec![[255, 255]; 2]);
        assert_eq!(*rec_frames, 0);
        // meters reset after each emit
        r.tick(40);
        let m = r.events().into_iter().find_map(|e| if let AudioEvent::Live { meters, .. } = e { Some(meters) } else { None }).unwrap();
        assert_eq!(m, vec![0.0; 3]);
        assert_eq!(to_u8(-1.0), 0);
        assert_eq!(to_u8(5.0), 255);
    }

    #[test]
    fn records_preroll_then_live_audio() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.5));
        r.fake.feed(&frames(0, 2000));
        r.tick(10);
        let raw = r.raw("take");
        r.send(AudioCmd::StartRec { raw_path: raw.clone() });
        let pre = r.events().into_iter().find_map(|e| if let AudioEvent::RecStarted { preroll_frames } = e { Some(preroll_frames) } else { None });
        assert_eq!(pre, Some(500), "0.5 s at 1000 Hz");
        r.fake.feed(&frames(2000, 300));
        r.tick(10);
        r.fake.feed(&frames(2300, 7));
        // stop drains what the callback already delivered
        r.cmd.send(AudioCmd::StopRec).unwrap();
        r.m.tick(r.t);
        let stopped = r.events().into_iter().find_map(|e| if let AudioEvent::RecStopped { raw_path, frames, interrupted, error } = e { Some((raw_path, frames, interrupted, error)) } else { None }).unwrap();
        assert_eq!(stopped, (raw.clone(), 807, false, None));
        let got = floats(&raw);
        assert_eq!(got, frames(1500, 807), "seamless: pre-roll, then every live frame, in order");
    }

    #[test]
    fn mono_records_only_the_selected_channel() {
        let mut r = Rig::new(4, 1000);
        r.configure(preset(Mode::Mono, vec![3], 0.0));
        let raw = r.raw("mono");
        r.send(AudioCmd::StartRec { raw_path: raw.clone() });
        r.fake.feed(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
        r.send(AudioCmd::StopRec);
        assert_eq!(floats(&raw), vec![0.3, 0.7]);
    }

    #[test]
    fn live_events_carry_recorded_frames() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        r.send(AudioCmd::StartRec { raw_path: r.raw("x") });
        r.fake.feed(&frames(0, 120));
        r.tick(40);
        let f = r.events().into_iter().find_map(|e| if let AudioEvent::Live { rec_frames, .. } = e { Some(rec_frames) } else { None });
        assert_eq!(f, Some(120));
    }

    #[test]
    fn start_twice_and_stop_without_take_are_harmless() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        r.send(AudioCmd::StopRec);
        r.send(AudioCmd::StartRec { raw_path: r.raw("a") });
        r.send(AudioCmd::StartRec { raw_path: r.raw("b") });
        let ev = r.events();
        assert_eq!(ev.iter().filter(|e| matches!(e, AudioEvent::RecStarted { .. })).count(), 1);
        assert!(!ev.iter().any(|e| matches!(e, AudioEvent::RecStopped { .. })));
        assert!(!r.raw("b").exists());
    }

    #[test]
    fn start_without_device_reports_error() {
        let mut r = Rig::new(2, 1000);
        let raw = r.raw("x");
        r.send(AudioCmd::StartRec { raw_path: raw.clone() });
        let e = r.events().into_iter().find_map(|e| if let AudioEvent::RecStopped { frames, interrupted, error, .. } = e { Some((frames, interrupted, error)) } else { None });
        assert_eq!(e, Some((0, true, Some("No input device".into()))));
    }

    #[test]
    fn spool_create_failure_reports_error() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        std::fs::write(r.dir.join("spool"), b"file, not folder").unwrap();
        r.send(AudioCmd::StartRec { raw_path: r.raw("x") });
        let e = r.events().into_iter().find_map(|e| if let AudioEvent::RecStopped { error, .. } = e { error } else { None }).unwrap();
        assert!(e.starts_with("Cannot create spool file"), "{e}");
        assert!(r.m.rec.is_none());
    }

    /// A spool that dies: like a full disk.
    struct Broken {
        fail_write: bool,
    }
    impl Write for Broken {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.fail_write { Err(std::io::Error::other("disk full")) } else { Ok(b.len()) }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Spool for Broken {
        fn sync(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("sync failed"))
        }
    }

    #[test]
    fn disk_full_while_recording_ends_the_take_immediately() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        r.m.spool_fn = |_| Ok(Box::new(Broken { fail_write: true }));
        r.send(AudioCmd::StartRec { raw_path: r.raw("x") });
        r.events();
        // more than the 1 MB buffer, so the write reaches the "disk"
        r.fake.feed(&frames(0, 200_000));
        r.tick(10);
        let (frames, interrupted, error) = r.events().into_iter().find_map(|e| if let AudioEvent::RecStopped { frames, interrupted, error, .. } = e { Some((frames, interrupted, error)) } else { None }).expect("take stopped by itself");
        assert_eq!(frames, 0);
        assert!(interrupted);
        assert!(error.unwrap().starts_with("Spool write failed: disk full"));
        assert!(r.m.rec.is_none());
        assert!(r.m.live.is_some(), "input stays open for the next take");
    }

    #[test]
    fn preroll_write_failure_also_ends_the_take() {
        // 30 s pre-roll at 10 kHz stereo = 2.4 MB: bigger than the write buffer
        let mut r = Rig::new(2, 10_000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 30.0));
        r.fake.feed(&frames(0, 400_000));
        r.tick(10);
        r.m.spool_fn = |_| Ok(Box::new(Broken { fail_write: true }));
        r.send(AudioCmd::StartRec { raw_path: r.raw("x") });
        let e = r.events().into_iter().find_map(|e| if let AudioEvent::RecStopped { error, .. } = e { error } else { None });
        assert!(e.unwrap().starts_with("Spool write failed"));
    }

    #[test]
    fn sync_failure_on_stop_is_reported() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        r.m.spool_fn = |_| Ok(Box::new(Broken { fail_write: false }));
        r.send(AudioCmd::StartRec { raw_path: r.raw("x") });
        r.fake.feed(&frames(0, 10));
        r.send(AudioCmd::StopRec);
        let e = r.events().into_iter().find_map(|e| if let AudioEvent::RecStopped { frames, error, .. } = e { Some((frames, error)) } else { None });
        assert_eq!(e, Some((10, Some("Spool flush failed: sync failed".into()))));
    }

    #[test]
    fn unplugged_device_saves_take_and_reconnects() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        let raw = r.raw("x");
        r.send(AudioCmd::StartRec { raw_path: raw.clone() });
        r.fake.feed(&frames(0, 50));
        r.tick(10);
        r.events();
        r.fake.0.lock().failed.store(true, Ordering::Relaxed);
        r.tick(10);
        let ev = r.events();
        let stop = ev.iter().find_map(|e| if let AudioEvent::RecStopped { frames, interrupted, error, .. } = e { Some((*frames, *interrupted, error.clone())) } else { None });
        assert_eq!(stop, Some((50, true, None)));
        let st = ev.iter().find_map(|e| if let AudioEvent::Status(s) = e { Some(s.clone()) } else { None }).unwrap();
        assert_eq!((st.state, st.message.as_str()), (AudioState::Error, "Input device disconnected"));
        assert_eq!(floats(&raw).len(), 100, "audio up to the unplug is on disk");
        r.tick(1000);
        assert_eq!(r.fake.opens(), 1);
        r.tick(1100);
        assert_eq!(r.fake.opens(), 2, "reopens by itself");
        assert_eq!(r.statuses().last().unwrap().state, AudioState::Ready);
    }

    #[test]
    fn silent_device_is_treated_as_dead() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        r.events();
        r.tick(2000);
        assert!(r.m.live.is_some());
        r.tick(1500);
        assert!(r.m.live.is_none());
        assert_eq!(r.statuses().last().unwrap().message, "Input device stopped delivering audio");
    }

    #[test]
    fn same_device_reroutes_without_reopening() {
        let mut r = Rig::new(4, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 1.0));
        r.fake.feed(&[0.0; 4 * 100]);
        r.tick(10);
        assert_eq!(r.m.preroll.len(), 200);
        r.events();
        r.configure(preset(Mode::Stereo, vec![3, 4], 1.0));
        assert_eq!(r.fake.opens(), 1);
        assert_eq!(r.statuses().last().unwrap().selected, vec![2, 3]);
        assert!(r.m.preroll.is_empty(), "old channels' pre-roll is dropped");
        // identical config: nothing at all
        r.configure(preset(Mode::Stereo, vec![3, 4], 1.0));
        assert!(r.statuses().is_empty());
        // shorter pre-roll trims the buffer
        r.fake.feed(&[0.0; 4 * 100]);
        r.tick(10);
        r.configure(preset(Mode::Stereo, vec![3, 4], 0.01));
        assert_eq!(r.m.preroll.len(), 20);
        // other device: reopen
        let mut p = preset(Mode::Stereo, vec![1, 2], 0.0);
        p.device = "Other".into();
        r.configure(p);
        assert_eq!(r.fake.opens(), 2);
    }

    #[test]
    fn config_change_while_recording_never_breaks_the_take() {
        let mut r = Rig::new(4, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        let raw = r.raw("x");
        r.send(AudioCmd::StartRec { raw_path: raw.clone() });
        let mut other = preset(Mode::Stereo, vec![3, 4], 0.0);
        other.device = "Other".into();
        r.configure(other.clone());
        r.configure(preset(Mode::Stereo, vec![3, 4], 0.0));
        r.fake.feed(&[0.1, 0.2, 0.3, 0.4]);
        r.send(AudioCmd::StopRec);
        assert_eq!(r.fake.opens(), 1);
        assert_eq!(floats(&raw), vec![0.1, 0.2], "still the channels the take started with");
    }

    #[test]
    fn dropouts_are_counted_and_reported() {
        let mut r = Rig::new(2, 1000);
        r.fake.0.lock().ring = 8;
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        r.events();
        r.fake.feed(&[0.0; 10]); // does not fit: dropped whole
        r.tick(40);
        let st = r.statuses();
        assert_eq!(st.last().unwrap().dropouts, 1);
        r.tick(40);
        assert!(r.statuses().is_empty(), "only reported when it changes");
    }

    #[test]
    fn push_input_converts_and_keeps_buffers_whole() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(4);
        let dropped = AtomicU64::new(0);
        push_input(&mut prod, &[i16::MAX, i16::MIN, 0], &dropped);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        push_input(&mut prod, &[1i16, 2], &dropped);
        assert_eq!(dropped.load(Ordering::Relaxed), 1, "would not fit: nothing written");
        let got: Vec<f32> = std::iter::from_fn(|| cons.pop().ok()).collect();
        assert_eq!(got.len(), 3);
        assert!((got[0] - 1.0).abs() < 1e-4 && got[1] == -1.0 && got[2] == 0.0);
        // wraps around the ring end
        push_input(&mut prod, &[0.25f32, 0.5, 0.75], &dropped);
        let got: Vec<f32> = std::iter::from_fn(|| cons.pop().ok()).collect();
        assert_eq!(got, vec![0.25, 0.5, 0.75]);
    }

    #[test]
    fn engine_gone_finishes_the_take_and_exits() {
        let mut r = Rig::new(2, 1000);
        r.configure(preset(Mode::Stereo, vec![1, 2], 0.0));
        let raw = r.raw("x");
        r.send(AudioCmd::StartRec { raw_path: raw.clone() });
        r.fake.feed(&frames(0, 5));
        let Rig { mut m, cmd, ev, t, .. } = r;
        drop(cmd);
        assert_eq!(m.tick(t), Tick::Exit);
        let f = ev.try_iter().find_map(|e| if let AudioEvent::RecStopped { frames, interrupted, .. } = e { Some((frames, interrupted)) } else { None });
        assert_eq!(f, Some((5, false)));
        assert_eq!(floats(&raw).len(), 10);
    }

    #[test]
    fn spawned_thread_runs_and_exits() {
        let fake = Fake::new(2, 1000);
        let (ev_tx, ev) = crossbeam_channel::unbounded();
        let cmd = spawn(ev_tx, fake.clone());
        cmd.send(AudioCmd::Configure(StreamSpec { preset: preset(Mode::Stereo, vec![1, 2], 0.0) })).unwrap();
        let ready = ev.iter().take(20).any(|e| matches!(e, AudioEvent::Status(s) if s.state == AudioState::Ready));
        assert!(ready);
        drop(cmd);
        // thread exits: event sender dropped, iterator ends
        assert!(crate::testutil::wait_for(5, || ev.try_iter().count() == 0 && ev.is_empty() && matches!(ev.recv_timeout(Duration::from_millis(50)), Err(crossbeam_channel::RecvTimeoutError::Disconnected))));
    }
}
