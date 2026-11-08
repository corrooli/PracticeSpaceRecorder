//! Central state. Owns the config store, talks to the audio thread, the job
//! queue and the destination monitor, and fans state + live data out to every
//! connected UI (the app window and any tablet) as JSON over a broadcast channel.

use crate::audio::{self, AudioCmd, AudioEvent, AudioState, AudioStatus, DeviceInfo, StreamSpec, BINS_PER_SEC};
use crate::config::{self, Destination, Mode, Preset, Settings, Store};
use crate::dest::{DestMonitor, DestState};
use crate::jobs::{self, Job, Jobs, TakeMeta, TaskState};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const ROLLING_BINS: usize = (BINS_PER_SEC * 30) as usize;

struct RecInfo {
    meta: TakeMeta,
    started_at: i64,
    started: bool,
    stopping: bool,
    frames: u64,
}

#[derive(Default)]
struct Runtime {
    devices: Vec<DeviceInfo>,
    audio: Option<AudioStatus>,
    rec: Option<RecInfo>,
    awake: Option<std::process::Child>,
    rolling: Vec<VecDeque<[u8; 2]>>,
    take_bins: Vec<Vec<[u8; 2]>>,
    last_toggle: Option<Instant>,
    toast: Option<(u64, String, String)>,
    toast_seq: u64,
    port: u16,
    urls: Vec<String>,
    qr: String,
}

pub struct Engine {
    store: Mutex<Store>,
    rt: Mutex<Runtime>,
    audio: Sender<AudioCmd>,
    jobs: Arc<Jobs>,
    dests: Arc<DestMonitor>,
    out: broadcast::Sender<Arc<str>>,
    dirty: Arc<AtomicBool>,
    pub clients: AtomicUsize,
    window: Mutex<Option<Box<dyn Fn(WindowCmd) + Send + Sync>>>,
}

pub enum WindowCmd {
    Fullscreen(bool),
    PickFolder { req: String, start: String },
    Quit,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ClientCmd {
    Toggle,
    Start,
    Stop,
    SelectPreset { id: String },
    StepPreset { dir: i32 },
    SavePreset { preset: Preset, activate: Option<bool> },
    DeletePreset { id: String },
    SaveSettings { settings: SettingsPatch },
    Fullscreen { on: Option<bool> },
    Rescan,
    RetryJobs,
    DismissToast,
    PickFolder { req: String, start: Option<String> },
    Quit,
}

#[derive(Deserialize)]
struct SettingsPatch {
    fullscreen: Option<bool>,
    remote_enabled: Option<bool>,
    port: Option<u16>,
}

impl Engine {
    pub fn start() -> Arc<Engine> {
        let store = Store::load();
        let dirty = Arc::new(AtomicBool::new(true));
        let d1 = dirty.clone();
        let d2 = dirty.clone();
        let jobs = Jobs::start(Box::new(move || d1.store(true, Ordering::Relaxed)), 2);
        let dests = DestMonitor::start(Box::new(move || d2.store(true, Ordering::Relaxed)));
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let audio = audio::spawn(ev_tx);
        let (out, _) = broadcast::channel(512);
        let eng = Arc::new(Engine {
            store: Mutex::new(store),
            rt: Mutex::new(Runtime::default()),
            audio,
            jobs,
            dests,
            out,
            dirty,
            clients: AtomicUsize::new(0),
            window: Mutex::new(None),
        });
        eng.apply_active();
        let e = eng.clone();
        std::thread::Builder::new()
            .name("psr-events".into())
            .spawn(move || {
                while let Ok(ev) = ev_rx.recv() {
                    e.on_audio(ev);
                }
            })
            .unwrap();
        let e = eng.clone();
        std::thread::Builder::new()
            .name("psr-state".into())
            .spawn(move || {
                let mut last_full = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(80));
                    if e.dirty.swap(false, Ordering::Relaxed) || last_full.elapsed() > Duration::from_secs(1) {
                        last_full = Instant::now();
                        let s = e.snapshot().to_string();
                        let _ = e.out.send(s.into());
                    }
                }
            })
            .unwrap();
        eng
    }

    pub fn set_window_handler(&self, f: Box<dyn Fn(WindowCmd) + Send + Sync>) {
        *self.window.lock() = Some(f);
    }

    pub fn settings(&self) -> Settings {
        self.store.lock().settings.clone()
    }

    pub fn set_server_info(&self, port: u16) {
        let mut urls = Vec::new();
        if let Ok(ip) = local_ip_address::local_ip() {
            urls.push(format!("http://{ip}:{port}"));
        }
        let host = gethostname::gethostname().to_string_lossy().trim_end_matches(".local").to_string();
        if !host.is_empty() {
            urls.push(format!("http://{host}.local:{port}"));
        }
        let qr = urls
            .first()
            .and_then(|u| qrcode::QrCode::new(u.as_bytes()).ok())
            .map(|c| c.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).dark_color(qrcode::render::svg::Color("#121116")).light_color(qrcode::render::svg::Color("#ffb048")).quiet_zone(true).build())
            .unwrap_or_default();
        let mut rt = self.rt.lock();
        rt.port = port;
        rt.urls = urls;
        rt.qr = qr;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.out.subscribe()
    }

    fn toast(&self, level: &str, msg: impl Into<String>) {
        let mut rt = self.rt.lock();
        rt.toast_seq += 1;
        rt.toast = Some((rt.toast_seq, level.into(), msg.into()));
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Push the active preset to audio + destination monitor.
    fn apply_active(&self) {
        let p = self.store.lock().active().clone();
        self.dests.watch(&effective_dests(&p));
        let _ = self.audio.send(AudioCmd::Configure(StreamSpec { preset: p }));
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn save_store(&self, s: &Store) {
        if let Err(e) = s.save() {
            log::error!("config save failed: {e}");
            self.toast("error", format!("Could not save settings: {e}"));
        }
    }

    // ---------------------------------------------------------------- audio

    fn on_audio(&self, ev: AudioEvent) {
        match ev {
            AudioEvent::Status(st) => {
                let mut rt = self.rt.lock();
                let n = st.selected.len();
                if rt.rolling.len() != n || rt.audio.as_ref().map(|a| a.selected != st.selected || a.device != st.device).unwrap_or(true) {
                    rt.rolling = vec![VecDeque::with_capacity(ROLLING_BINS); n];
                }
                rt.audio = Some(st);
                self.dirty.store(true, Ordering::Relaxed);
            }
            AudioEvent::Devices(d) => {
                let mut rt = self.rt.lock();
                let changed = rt.devices.iter().map(|x| (&x.name, x.channels)).ne(d.iter().map(|x| (&x.name, x.channels)));
                rt.devices = d;
                if changed {
                    self.dirty.store(true, Ordering::Relaxed);
                }
            }
            AudioEvent::Live { meters, bins, rec_frames } => {
                let mut rt = self.rt.lock();
                if rt.rolling.len() != bins.len() {
                    rt.rolling = vec![VecDeque::with_capacity(ROLLING_BINS); bins.len()];
                }
                for (r, b) in rt.rolling.iter_mut().zip(&bins) {
                    for &x in b {
                        if r.len() >= ROLLING_BINS {
                            r.pop_front();
                        }
                        r.push_back(x);
                    }
                }
                let recording = rt.rec.as_ref().map(|r| r.started).unwrap_or(false);
                if recording {
                    if rt.take_bins.len() != bins.len() {
                        rt.take_bins = vec![Vec::new(); bins.len()];
                    }
                    for (t, b) in rt.take_bins.iter_mut().zip(&bins) {
                        t.extend_from_slice(b);
                    }
                    if let Some(r) = &mut rt.rec {
                        r.frames = rec_frames;
                    }
                }
                drop(rt);
                let m: Vec<u16> = meters.iter().map(|v| (v.clamp(0.0, 2.0) * 1000.0) as u16).collect();
                let b: Vec<Vec<u8>> = bins.iter().map(|c| c.iter().flat_map(|x| [x[0], x[1]]).collect()).collect();
                let msg = json!({ "t": "live", "m": m, "b": b, "f": rec_frames, "r": recording });
                let _ = self.out.send(msg.to_string().into());
            }
            AudioEvent::RecStarted { preroll_frames } => {
                let mut rt = self.rt.lock();
                let rate = rt.audio.as_ref().map(|a| a.sample_rate).unwrap_or(48000).max(1);
                let pre_bins = (preroll_frames * BINS_PER_SEC as u64 / rate as u64) as usize;
                let take: Vec<Vec<[u8; 2]>> = rt.rolling.iter().map(|r| r.iter().skip(r.len().saturating_sub(pre_bins)).copied().collect()).collect();
                rt.take_bins = take;
                if let Some(r) = &mut rt.rec {
                    r.started = true;
                    r.frames = preroll_frames;
                }
                drop(rt);
                self.send_history();
                self.dirty.store(true, Ordering::Relaxed);
            }
            AudioEvent::RecStopped { raw_path, frames, interrupted, error } => {
                let rec = {
                    let mut rt = self.rt.lock();
                    if let Some(mut c) = rt.awake.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    rt.rec.take()
                };
                if let Some(rec) = rec {
                    if frames > 0 {
                        let mut job = Job::new(raw_path, rec.meta, frames);
                        if interrupted {
                            job.note = "Recording was cut short".into();
                        }
                        let secs = job.seconds();
                        let name = job.meta.take_name.clone();
                        self.jobs.push(job);
                        if let Some(e) = &error {
                            self.toast("error", format!("{name}: {e}. Saved what was recorded."));
                        } else if interrupted {
                            self.toast("error", format!("{name} stopped early ({}). Audio up to that point is safe.", fmt_dur(secs)));
                        }
                    } else {
                        let _ = std::fs::remove_file(&raw_path);
                        let _ = std::fs::remove_file(raw_path.with_extension("json"));
                        self.toast("error", error.unwrap_or_else(|| "Nothing was recorded".into()));
                    }
                }
                self.send_history();
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    // ------------------------------------------------------------ recording

    fn toggle(&self) -> Result<(), String> {
        if self.rt.lock().rec.is_some() { self.stop() } else { self.start_rec() }
    }

    fn debounce(&self) -> bool {
        let mut rt = self.rt.lock();
        let ok = rt.last_toggle.map(|t| t.elapsed() > Duration::from_millis(600)).unwrap_or(true);
        if ok {
            rt.last_toggle = Some(Instant::now());
        }
        ok
    }

    fn start_rec(&self) -> Result<(), String> {
        if self.rt.lock().rec.is_some() || !self.debounce() {
            return Ok(());
        }
        let (audio_ok, rate, sel) = {
            let rt = self.rt.lock();
            match &rt.audio {
                Some(a) if a.state == AudioState::Ready => (true, a.sample_rate, a.selected.clone()),
                _ => (false, 0, vec![]),
            }
        };
        if !audio_ok {
            return Err("Input device not ready".into());
        }
        let now = chrono::Local::now();
        let (preset, take) = {
            let mut s = self.store.lock();
            let today = now.format("%Y-%m-%d").to_string();
            if s.settings.take_date != today {
                s.settings.take_date = today;
                s.settings.take_counter = 0;
            }
            s.settings.take_counter += 1;
            let t = s.settings.take_counter;
            let p = s.active().clone();
            let snapshot = s.clone();
            drop(s);
            self.save_store(&snapshot);
            (p, t)
        };
        let rel = config::expand_template(&preset.filename, &preset.name, take, now);
        let id = config::new_id("t");
        let raw = jobs::spool_dir().join(format!("{id}.raw"));
        let meta = TakeMeta {
            take_name: rel.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| id.clone()),
            rel_path: rel,
            preset_name: preset.name.clone(),
            sample_rate: rate,
            track_names: sel.iter().enumerate().map(|(i, &c)| preset.track_name(i, c)).collect(),
            format: preset.format,
            split_tracks: preset.split_tracks && preset.mode == Mode::Multitrack,
            started_at: now.timestamp_millis(),
            destinations: effective_dests(&preset),
        };
        std::fs::create_dir_all(jobs::spool_dir()).map_err(|e| format!("Spool folder: {e}"))?;
        config::write_atomic(&raw.with_extension("json"), &serde_json::to_vec_pretty(&meta).unwrap()).map_err(|e| format!("Spool: {e}"))?;
        log::info!("recording {} ({} tracks @ {rate} Hz)", meta.take_name, sel.len());
        {
            let mut rt = self.rt.lock();
            rt.rec = Some(RecInfo { meta, started_at: now.timestamp_millis(), started: false, stopping: false, frames: 0 });
            rt.awake = keep_awake();
        }
        let _ = self.audio.send(AudioCmd::StartRec { raw_path: raw });
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        {
            let mut rt = self.rt.lock();
            let ok = rt.last_toggle.map(|t| t.elapsed() > Duration::from_millis(600)).unwrap_or(true);
            match &mut rt.rec {
                Some(r) if !r.stopping && ok => r.stopping = true,
                _ => return Ok(()),
            }
            rt.last_toggle = Some(Instant::now());
        }
        let _ = self.audio.send(AudioCmd::StopRec);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    // -------------------------------------------------------------- clients

    pub fn handle(&self, text: &str, privileged: bool) {
        let cmd: ClientCmd = match serde_json::from_str(text) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("bad client message: {e}");
                return;
            }
        };
        let open_to_remote = matches!(cmd, ClientCmd::Toggle | ClientCmd::Start | ClientCmd::Stop | ClientCmd::SelectPreset { .. } | ClientCmd::StepPreset { .. } | ClientCmd::DismissToast | ClientCmd::RetryJobs);
        if !privileged && !open_to_remote {
            self.toast("error", "Settings can only be changed on the recording computer");
            return;
        }
        let recording = self.rt.lock().rec.is_some();
        let res = match cmd {
            ClientCmd::Toggle => self.toggle(),
            ClientCmd::Start => self.start_rec(),
            ClientCmd::Stop => self.stop(),
            ClientCmd::SelectPreset { id } => {
                if recording {
                    Err("Stop recording before switching presets".into())
                } else {
                    let mut s = self.store.lock();
                    if s.presets.iter().any(|p| p.id == id) {
                        s.settings.active_preset = id;
                        let snap = s.clone();
                        drop(s);
                        self.save_store(&snap);
                        self.apply_active();
                    }
                    Ok(())
                }
            }
            ClientCmd::StepPreset { dir } => {
                if recording {
                    Err("Stop recording before switching presets".into())
                } else {
                    let mut s = self.store.lock();
                    let n = s.presets.len() as i32;
                    let i = s.presets.iter().position(|p| p.id == s.settings.active_preset).unwrap_or(0) as i32;
                    s.settings.active_preset = s.presets[((i + dir.signum()) % n + n) as usize % n as usize].id.clone();
                    let snap = s.clone();
                    drop(s);
                    self.save_store(&snap);
                    self.apply_active();
                    Ok(())
                }
            }
            ClientCmd::SavePreset { mut preset, activate } => {
                let mut s = self.store.lock();
                let is_active = s.settings.active_preset == preset.id || activate.unwrap_or(false);
                if recording && is_active {
                    Err("Stop recording before changing the active preset".into())
                } else {
                    if preset.id.is_empty() {
                        preset.id = config::new_id("p");
                    }
                    if preset.name.trim().is_empty() {
                        preset.name = "Untitled".into();
                    }
                    for d in &mut preset.destinations {
                        if d.id.is_empty() {
                            d.id = config::new_id("d");
                        }
                    }
                    let name = preset.name.clone();
                    match s.presets.iter_mut().find(|p| p.id == preset.id) {
                        Some(p) => *p = preset.clone(),
                        None => s.presets.push(preset.clone()),
                    }
                    if activate.unwrap_or(false) {
                        s.settings.active_preset = preset.id.clone();
                    }
                    let snap = s.clone();
                    drop(s);
                    self.save_store(&snap);
                    if is_active {
                        self.apply_active();
                    }
                    self.toast("ok", format!("Preset \u{201c}{name}\u{201d} saved"));
                    Ok(())
                }
            }
            ClientCmd::DeletePreset { id } => {
                let mut s = self.store.lock();
                if s.presets.len() <= 1 {
                    Err("Keep at least one preset".into())
                } else if recording && s.settings.active_preset == id {
                    Err("Stop recording first".into())
                } else {
                    s.presets.retain(|p| p.id != id);
                    let was_active = s.settings.active_preset == id;
                    if was_active {
                        s.settings.active_preset = s.presets[0].id.clone();
                    }
                    let snap = s.clone();
                    drop(s);
                    self.save_store(&snap);
                    if was_active {
                        self.apply_active();
                    }
                    Ok(())
                }
            }
            ClientCmd::SaveSettings { settings } => {
                let mut s = self.store.lock();
                let old_port = s.settings.port;
                if let Some(f) = settings.fullscreen {
                    s.settings.fullscreen = f;
                }
                if let Some(r) = settings.remote_enabled {
                    s.settings.remote_enabled = r;
                }
                if let Some(p) = settings.port {
                    if p >= 1024 {
                        s.settings.port = p;
                    }
                }
                let snap = s.clone();
                drop(s);
                self.save_store(&snap);
                if snap.settings.port != old_port {
                    self.toast("ok", "New port is used after the next app start");
                }
                Ok(())
            }
            ClientCmd::Fullscreen { on } => {
                let mut s = self.store.lock();
                let v = on.unwrap_or(!s.settings.fullscreen);
                s.settings.fullscreen = v;
                let snap = s.clone();
                drop(s);
                self.save_store(&snap);
                if let Some(w) = &*self.window.lock() {
                    w(WindowCmd::Fullscreen(v));
                }
                Ok(())
            }
            ClientCmd::Rescan => {
                let _ = self.audio.send(AudioCmd::Rescan);
                Ok(())
            }
            ClientCmd::RetryJobs => {
                self.jobs.retry_all();
                Ok(())
            }
            ClientCmd::DismissToast => {
                self.rt.lock().toast = None;
                Ok(())
            }
            ClientCmd::PickFolder { req, start } => {
                if let Some(w) = &*self.window.lock() {
                    w(WindowCmd::PickFolder { req, start: start.unwrap_or_default() });
                }
                Ok(())
            }
            ClientCmd::Quit => {
                if recording {
                    Err("Stop recording before quitting".into())
                } else {
                    if let Some(w) = &*self.window.lock() {
                        w(WindowCmd::Quit);
                    }
                    Ok(())
                }
            }
        };
        if let Err(e) = res {
            self.toast("error", e);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Result of a native folder dialog, routed back to the UI that asked.
    pub fn picked(&self, req: &str, path: Option<String>) {
        let _ = self.out.send(json!({ "t": "picked", "req": req, "path": path }).to_string().into());
    }

    pub fn is_recording(&self) -> bool {
        self.rt.lock().rec.is_some()
    }

    // ------------------------------------------------------------- snapshot

    fn send_history(&self) {
        let _ = self.out.send(self.history().to_string().into());
    }

    pub fn history(&self) -> Value {
        let rt = self.rt.lock();
        let flat = |v: &mut dyn Iterator<Item = &[u8; 2]>| v.flat_map(|x| [x[0], x[1]]).collect::<Vec<u8>>();
        let rolling: Vec<Vec<u8>> = rt.rolling.iter().map(|r| flat(&mut r.iter())).collect();
        let take: Vec<Vec<u8>> = rt.take_bins.iter().map(|r| flat(&mut r.iter())).collect();
        json!({ "t": "hist", "rolling": rolling, "take": take, "recording": rt.rec.as_ref().map(|r| r.started).unwrap_or(false), "bps": BINS_PER_SEC })
    }

    pub fn snapshot(&self) -> Value {
        let store = self.store.lock().clone();
        let rt = self.rt.lock();
        let active = store.active().clone();
        let dests = self.dests.snapshot();
        let spool_free = fs2::available_space(jobs::spool_dir().parent().unwrap_or(&jobs::spool_dir())).unwrap_or(0);
        let audio = rt.audio.clone();
        let tracks: Vec<String> = match &audio {
            Some(a) if !a.selected.is_empty() => a.selected.iter().enumerate().map(|(i, &c)| active.track_name(i, c)).collect(),
            _ => active.selected_channels(active.channels.iter().copied().max().unwrap_or(2)).iter().enumerate().map(|(i, &c)| active.track_name(i, c)).collect(),
        };
        let jobs: Vec<Value> = {
            let list = self.jobs.list.lock();
            let start = list.len().saturating_sub(12);
            list[start..]
                .iter()
                .rev()
                .map(|j| {
                    json!({
                        "id": j.id, "take": j.meta.take_name, "preset": j.meta.preset_name, "created": j.created,
                        "seconds": j.seconds(), "tracks": j.meta.track_names.len(), "note": j.note,
                        "state": if j.done() { "done" } else if j.tasks.iter().any(|t| t.state == TaskState::Failed) { "failed" } else if j.tasks.iter().any(|t| t.state == TaskState::Busy) { "busy" } else { "pending" },
                        "tasks": j.tasks.iter().map(|t| json!({"label": t.label, "state": t.state, "progress": t.progress, "message": t.message, "out": t.out_path, "next_try": t.next_try})).collect::<Vec<_>>(),
                    })
                })
                .collect()
        };
        let queue_open = self.jobs.list.lock().iter().filter(|j| !j.done()).count();
        let rec = rt.rec.as_ref().map(|r| {
            json!({ "take": r.meta.take_name, "started_at": r.started_at, "started": r.started, "stopping": r.stopping,
                    "frames": r.frames, "sample_rate": r.meta.sample_rate, "tracks": r.meta.track_names })
        });
        let dest_ready = !dests.is_empty() && dests.iter().all(|d| d.state == DestState::Ready);
        let audio_ready = audio.as_ref().map(|a| a.state == AudioState::Ready).unwrap_or(false);
        json!({
            "t": "state",
            "version": env!("CARGO_PKG_VERSION"),
            "now": jobs::now_ms(),
            "settings": store.settings,
            "presets": store.presets,
            "active": active.id,
            "tracks": tracks,
            "devices": rt.devices,
            "audio": audio,
            "rec": rec,
            "dests": dests,
            "spool_free": spool_free,
            "queue_open": queue_open,
            "jobs": jobs,
            "ready": audio_ready,
            "dest_ready": dest_ready,
            "toast": rt.toast.as_ref().map(|(id, lvl, msg)| json!({"id": id, "level": lvl, "msg": msg})),
            "remote": { "port": rt.port, "urls": rt.urls, "qr": rt.qr, "clients": self.clients.load(Ordering::Relaxed), "enabled": store.settings.remote_enabled },
            "music_dir": config::default_music_dir(),
            "platform": std::env::consts::OS,
        })
    }
}

/// Destinations a take goes to. Never zero: a take with nowhere to go would be lost.
fn effective_dests(p: &Preset) -> Vec<Destination> {
    let v: Vec<Destination> = p.destinations.iter().filter(|d| d.enabled && !d.path.trim().is_empty()).cloned().collect();
    if v.is_empty() {
        vec![Destination { id: "fallback".into(), label: "Music folder (fallback)".into(), ..Default::default() }]
    } else {
        v
    }
}

/// Keep the machine from sleeping while tape is rolling.
fn keep_awake() -> Option<std::process::Child> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut c = {
        let mut c = Command::new("caffeinate");
        c.args(["-i", "-s", "-w", &std::process::id().to_string()]);
        c
    };
    #[cfg(not(target_os = "macos"))]
    let mut c = {
        let mut c = Command::new("systemd-inhibit");
        c.args(["--what=idle:sleep", "--who=PracticeSpaceRecorder", "--why=Recording", "sleep", "infinity"]);
        c
    };
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()
}

fn fmt_dur(s: f64) -> String {
    let s = s as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}
