//! Central state. Owns the config store, talks to the audio thread, the job
//! queue and the destination monitor, and fans state + live data out to every
//! connected UI (the app window and any tablet) as JSON over a broadcast channel.

use crate::audio::{self, AudioCmd, AudioEvent, Backend, AudioState, AudioStatus, DeviceInfo, StreamSpec, BINS_PER_SEC};
use crate::config::{self, Destination, Mode, Preset, Settings, Store};
use crate::dest::{DestMonitor, DestState};
use crate::jobs::{self, Job, Jobs, TakeMeta, TaskState};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
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
    window: Mutex<Option<WindowHandler>>,
    /// Starts the "don't sleep while recording" helper process.
    awake_fn: fn() -> Option<std::process::Child>,
}

/// Called for things only the native window can do.
pub type WindowHandler = Box<dyn Fn(WindowCmd) + Send + Sync>;

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
    TestDest { req: String, dest: Destination },
    Quit,
}

#[derive(Deserialize)]
struct SettingsPatch {
    fullscreen: Option<bool>,
    remote_enabled: Option<bool>,
    port: Option<u16>,
}

impl Engine {
    /// The whole app minus the window: audio thread, job workers, destination
    /// monitor and the state broadcaster.
    pub fn start_with(config: &Path, data: PathBuf, backend: impl Backend, awake_fn: fn() -> Option<std::process::Child>) -> Arc<Engine> {
        let store = Store::load(config);
        let dirty = Arc::new(AtomicBool::new(true));
        let d1 = dirty.clone();
        let d2 = dirty.clone();
        let jobs = Jobs::start(data, Box::new(move || d1.store(true, Ordering::Relaxed)), 2);
        let dests = DestMonitor::start(Box::new(move || d2.store(true, Ordering::Relaxed)));
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let audio = audio::spawn(ev_tx, backend);
        let eng = Engine::new(store, audio, jobs, dests, dirty, awake_fn);
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
                    if e.publish_if_dirty(last_full.elapsed() > Duration::from_secs(1)) {
                        last_full = Instant::now();
                    }
                }
            })
            .unwrap();
        eng
    }

    /// Wire an engine from its parts. Spawns nothing: [`Engine::start`] adds the
    /// threads, tests drive `on_audio` / `handle` directly.
    pub fn new(store: Store, audio: Sender<AudioCmd>, jobs: Arc<Jobs>, dests: Arc<DestMonitor>, dirty: Arc<AtomicBool>, awake_fn: fn() -> Option<std::process::Child>) -> Arc<Engine> {
        let (out, _) = broadcast::channel(512);
        Arc::new(Engine {
            store: Mutex::new(store),
            rt: Mutex::new(Runtime::default()),
            audio,
            jobs,
            dests,
            out,
            dirty,
            clients: AtomicUsize::new(0),
            window: Mutex::new(None),
            awake_fn,
        })
    }

    /// Broadcast a full state snapshot if anything changed (or `force`).
    fn publish_if_dirty(&self, force: bool) -> bool {
        if self.dirty.swap(false, Ordering::Relaxed) || force {
            let _ = self.out.send(self.snapshot().to_string().into());
            true
        } else {
            false
        }
    }

    pub fn set_window_handler(&self, f: WindowHandler) {
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
        let spool = self.jobs.spool_dir();
        let raw = spool.join(format!("{id}.raw"));
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
        std::fs::create_dir_all(&spool).map_err(|e| format!("Spool folder: {e}"))?;
        config::write_atomic(&raw.with_extension("json"), &serde_json::to_vec_pretty(&meta).unwrap()).map_err(|e| format!("Spool: {e}"))?;
        log::info!("recording {} ({} tracks @ {rate} Hz)", meta.take_name, sel.len());
        {
            let mut rt = self.rt.lock();
            rt.rec = Some(RecInfo { meta, started_at: now.timestamp_millis(), started: false, stopping: false, frames: 0 });
            rt.awake = (self.awake_fn)();
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
                        d.normalize();
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
            ClientCmd::TestDest { req, dest } => {
                let out = self.out.clone();
                std::thread::spawn(move || {
                    let msg = match crate::dest::test_write(&dest, Duration::from_secs(25)) {
                        Ok(r) => json!({ "t": "dest_test", "req": req, "ok": true, "msg": "", "path": r.path, "free": r.free_bytes }),
                        Err(e) => json!({ "t": "dest_test", "req": req, "ok": false, "msg": e, "path": "", "free": 0 }),
                    };
                    let _ = out.send(msg.to_string().into());
                });
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
        let spool = self.jobs.spool_dir();
        let spool_free = fs2::available_space(spool.parent().unwrap_or(&spool)).unwrap_or(0);
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
    let v: Vec<Destination> = p.destinations.iter().filter(|d| d.usable()).cloned().collect();
    if v.is_empty() {
        vec![Destination { id: "fallback".into(), label: "Music folder (fallback)".into(), ..Default::default() }]
    } else {
        v
    }
}

/// Keep the machine from sleeping while tape is rolling.
pub fn keep_awake() -> Option<std::process::Child> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::DestTask;
    use crate::testutil::{wait_for, Fake, TempDir};
    use crossbeam_channel::Receiver;

    struct Rig {
        e: Arc<Engine>,
        audio: Receiver<AudioCmd>,
        dir: TempDir,
    }

    fn no_awake() -> Option<std::process::Child> {
        None
    }

    impl Rig {
        fn new() -> Rig {
            let dir = TempDir::new("engine");
            let mut store = Store::load(&dir.join("config.json"));
            let out = dir.join("out");
            std::fs::create_dir_all(&out).unwrap();
            store.presets[0].destinations[0].path = out.to_string_lossy().into();
            store.presets[0].sample_rate = 1000;
            store.save().unwrap();
            let jobs = Jobs::load(dir.join("data"), Box::new(|| {}));
            let dests = DestMonitor::new(Box::new(|| {}));
            let (tx, audio) = crossbeam_channel::unbounded();
            let e = Engine::new(store, tx, jobs, dests, Arc::new(AtomicBool::new(false)), no_awake);
            Rig { e, audio, dir }
        }
        fn cmd(&self, json: &str) {
            self.e.handle(json, true);
        }
        fn remote(&self, json: &str) {
            self.e.handle(json, false);
        }
        fn audio_cmds(&self) -> Vec<AudioCmd> {
            self.audio.try_iter().collect()
        }
        fn ready(&self, selected: Vec<usize>) {
            self.e.on_audio(AudioEvent::Status(AudioStatus { state: AudioState::Ready, message: String::new(), device: "Fake".into(), sample_rate: 1000, device_channels: 2, selected, dropouts: 0 }));
        }
        fn toast(&self) -> Option<(String, String)> {
            self.e.rt.lock().toast.as_ref().map(|(_, l, m)| (l.clone(), m.clone()))
        }
        fn undebounce(&self) {
            self.e.rt.lock().last_toggle = None;
        }
        /// Start a take and return its spool path.
        fn start(&self) -> PathBuf {
            self.undebounce();
            self.cmd(r#"{"cmd":"start"}"#);
            match self.audio_cmds().pop() {
                Some(AudioCmd::StartRec { raw_path }) => raw_path,
                _ => panic!("no StartRec"),
            }
        }
        fn presets(&self) -> Vec<Preset> {
            self.e.store.lock().presets.clone()
        }
        fn active(&self) -> String {
            self.e.store.lock().settings.active_preset.clone()
        }
        fn saved(&self) -> Store {
            Store::load(&self.dir.join("config.json"))
        }
        fn add_preset(&self, id: &str) {
            self.cmd(&format!(r#"{{"cmd":"save_preset","preset":{{"id":"{id}","name":"{id}"}}}}"#));
        }
    }

    fn stopped(raw: &Path, frames: u64, interrupted: bool, error: Option<&str>) -> AudioEvent {
        AudioEvent::RecStopped { raw_path: raw.to_path_buf(), frames, interrupted, error: error.map(String::from) }
    }

    #[test]
    fn refuses_to_record_without_input() {
        let r = Rig::new();
        r.cmd(r#"{"cmd":"toggle"}"#);
        assert_eq!(r.toast(), Some(("error".into(), "Input device not ready".into())));
        assert!(r.audio_cmds().is_empty());
        assert!(!r.e.is_recording());
    }

    #[test]
    fn full_take_becomes_a_job() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        let raw = r.start();
        assert!(r.e.is_recording());
        assert!(raw.starts_with(r.dir.join("data/spool")));
        let meta: TakeMeta = serde_json::from_slice(&std::fs::read(raw.with_extension("json")).unwrap()).unwrap();
        assert_eq!(meta.track_names, ["Left", "Right"]);
        assert_eq!(meta.sample_rate, 1000);
        assert!(meta.take_name.ends_with("take 01"));
        assert_eq!(r.saved().settings.take_counter, 1, "counter persisted before recording");
        // audio confirms, data flows
        r.e.on_audio(AudioEvent::RecStarted { preroll_frames: 0 });
        r.e.on_audio(AudioEvent::Live { meters: vec![0.5, 3.0], bins: vec![vec![[10, 5]], vec![[20, 6]]], rec_frames: 10 });
        let snap = r.e.snapshot();
        assert_eq!(snap["rec"]["started"], true);
        assert_eq!(snap["rec"]["frames"], 10);
        assert_eq!(r.e.history()["take"], json!([[10, 5], [20, 6]]));
        // stop
        r.undebounce();
        r.cmd(r#"{"cmd":"toggle"}"#);
        assert!(matches!(r.audio_cmds()[..], [AudioCmd::StopRec]));
        assert_eq!(r.e.snapshot()["rec"]["stopping"], true);
        std::fs::write(&raw, [0u8; 80]).unwrap();
        r.e.on_audio(stopped(&raw, 10, false, None));
        assert!(!r.e.is_recording());
        let jobs = r.e.jobs.list.lock().clone();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].frames, 10);
        assert_eq!(jobs[0].note, "");
        assert_eq!(r.toast(), None, "clean take: no toast");
        assert_eq!(r.e.snapshot()["queue_open"], 1);
    }

    #[test]
    fn take_counter_counts_up_and_resets_daily() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        for n in 1..=3 {
            let raw = r.start();
            r.e.on_audio(stopped(&raw, 0, false, None));
            assert_eq!(r.e.store.lock().settings.take_counter, n);
        }
        r.e.store.lock().settings.take_date = "1999-01-01".into();
        let raw = r.start();
        r.e.on_audio(stopped(&raw, 0, false, None));
        assert_eq!(r.e.store.lock().settings.take_counter, 1);
    }

    #[test]
    fn empty_take_is_discarded_with_a_reason() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        let raw = r.start();
        std::fs::write(&raw, b"").unwrap();
        r.e.on_audio(stopped(&raw, 0, true, Some("Cannot create spool file: denied")));
        assert!(!raw.exists() && !raw.with_extension("json").exists());
        assert_eq!(r.toast().unwrap().1, "Cannot create spool file: denied");
        assert!(r.e.jobs.list.lock().is_empty());
        let raw = r.start();
        r.e.on_audio(stopped(&raw, 0, false, None));
        assert_eq!(r.toast().unwrap().1, "Nothing was recorded");
    }

    #[test]
    fn interrupted_take_is_kept_and_explained() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        let raw = r.start();
        r.e.on_audio(stopped(&raw, 65_000, true, None));
        let j = r.e.jobs.list.lock()[0].clone();
        assert_eq!(j.note, "Recording was cut short");
        let (lvl, msg) = r.toast().unwrap();
        assert_eq!(lvl, "error");
        assert!(msg.ends_with("stopped early (1:05). Audio up to that point is safe."), "{msg}");
        let raw = r.start();
        r.e.on_audio(stopped(&raw, 5, true, Some("Spool write failed: disk full")));
        assert!(r.toast().unwrap().1.ends_with(": Spool write failed: disk full. Saved what was recorded."));
        assert_eq!(r.e.jobs.list.lock().len(), 2);
    }

    #[test]
    fn stray_stop_event_is_ignored() {
        let r = Rig::new();
        r.e.on_audio(stopped(Path::new("/nonexistent.raw"), 10, false, None));
        assert!(r.e.jobs.list.lock().is_empty());
        assert_eq!(r.toast(), None);
    }

    #[test]
    fn debounce_and_double_stop() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        r.start();
        // bouncing key: stop right after start is ignored
        r.cmd(r#"{"cmd":"toggle"}"#);
        assert!(r.audio_cmds().is_empty());
        r.undebounce();
        r.cmd(r#"{"cmd":"stop"}"#);
        r.undebounce();
        r.cmd(r#"{"cmd":"stop"}"#);
        assert_eq!(r.audio_cmds().len(), 1, "one StopRec only");
        // start while a take is still open does nothing
        r.undebounce();
        r.cmd(r#"{"cmd":"start"}"#);
        assert!(r.audio_cmds().is_empty());
        // stop when idle does nothing
        let r = Rig::new();
        r.cmd(r#"{"cmd":"stop"}"#);
        assert!(r.audio_cmds().is_empty());
        // start debounce
        r.ready(vec![0, 1]);
        r.e.rt.lock().last_toggle = Some(Instant::now());
        r.cmd(r#"{"cmd":"start"}"#);
        assert!(!r.e.is_recording());
    }

    #[test]
    fn spool_folder_failure_is_reported() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        std::fs::create_dir_all(r.dir.join("data")).unwrap();
        std::fs::write(r.dir.join("data/spool"), b"").unwrap();
        r.cmd(r#"{"cmd":"start"}"#);
        assert!(r.toast().unwrap().1.starts_with("Spool folder"));
        assert!(!r.e.is_recording());
    }

    #[test]
    fn keep_awake_runs_only_while_recording() {
        fn sleeper() -> Option<std::process::Child> {
            std::process::Command::new("sleep").arg("30").spawn().ok()
        }
        let r = Rig::new();
        let e = Engine::new(r.e.store.lock().clone(), crossbeam_channel::unbounded().0, r.e.jobs.clone(), DestMonitor::new(Box::new(|| {})), Arc::new(AtomicBool::new(false)), sleeper);
        e.on_audio(AudioEvent::Status(AudioStatus { state: AudioState::Ready, message: String::new(), device: "F".into(), sample_rate: 1000, device_channels: 2, selected: vec![0, 1], dropouts: 0 }));
        e.handle(r#"{"cmd":"start"}"#, true);
        let pid = e.rt.lock().awake.as_ref().expect("helper running").id();
        e.on_audio(stopped(Path::new("/x.raw"), 0, false, None));
        assert!(e.rt.lock().awake.is_none());
        let alive = std::process::Command::new("kill").arg("-0").arg(pid.to_string()).status().unwrap().success();
        assert!(!alive, "helper killed and reaped");
    }

    #[test]
    fn remote_can_record_but_not_configure() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        r.remote(r#"{"cmd":"start"}"#);
        assert!(r.e.is_recording());
        for c in [
            r#"{"cmd":"save_preset","preset":{"name":"evil","destinations":[{"mount_cmd":"rm -rf ~"}]}}"#,
            r#"{"cmd":"delete_preset","id":"x"}"#,
            r#"{"cmd":"save_settings","settings":{"remote_enabled":true}}"#,
            r#"{"cmd":"fullscreen"}"#,
            r#"{"cmd":"rescan"}"#,
            r#"{"cmd":"pick_folder","req":"1"}"#,
            r#"{"cmd":"quit"}"#,
        ] {
            r.e.rt.lock().toast = None;
            r.remote(c);
            assert_eq!(r.toast().unwrap().1, "Settings can only be changed on the recording computer", "{c}");
        }
        assert_eq!(r.presets().len(), 1);
        assert!(matches!(r.audio_cmds()[..], [AudioCmd::StartRec { .. }]), "no rescan");
        for c in [r#"{"cmd":"dismiss_toast"}"#, r#"{"cmd":"retry_jobs"}"#] {
            r.remote(c);
        }
        assert_eq!(r.toast(), None, "dismiss worked for remote");
    }

    #[test]
    fn garbage_messages_are_ignored() {
        let r = Rig::new();
        for m in ["", "{", "null", r#"{"cmd":"format_disk"}"#, r#"{"cmd":"select_preset"}"#, r#"{"cmd":"step_preset","dir":"x"}"#] {
            r.cmd(m);
            r.remote(m);
        }
        assert_eq!(r.toast(), None);
        assert_eq!(r.presets().len(), 1);
    }

    #[test]
    fn select_and_step_presets() {
        let r = Rig::new();
        let first = r.active();
        r.add_preset("b");
        r.add_preset("c");
        r.audio_cmds();
        r.remote(r#"{"cmd":"select_preset","id":"c"}"#);
        assert_eq!(r.active(), "c");
        assert_eq!(r.saved().settings.active_preset, "c");
        assert!(matches!(&r.audio_cmds()[..], [AudioCmd::Configure(s)] if s.preset.id == "c"));
        r.cmd(r#"{"cmd":"select_preset","id":"nope"}"#);
        assert_eq!(r.active(), "c");
        r.cmd(r#"{"cmd":"step_preset","dir":1}"#);
        assert_eq!(r.active(), first, "wraps forward");
        r.cmd(r#"{"cmd":"step_preset","dir":-5}"#);
        assert_eq!(r.active(), "c", "wraps backward, one step");
        r.cmd(r#"{"cmd":"step_preset","dir":-1}"#);
        assert_eq!(r.active(), "b");
        // locked while recording
        r.ready(vec![0, 1]);
        r.start();
        r.cmd(r#"{"cmd":"select_preset","id":"c"}"#);
        assert_eq!(r.toast().unwrap().1, "Stop recording before switching presets");
        r.e.rt.lock().toast = None;
        r.cmd(r#"{"cmd":"step_preset","dir":1}"#);
        assert_eq!(r.toast().unwrap().1, "Stop recording before switching presets");
        assert_eq!(r.active(), "b");
    }

    #[test]
    fn save_preset_fills_gaps_and_replaces() {
        let r = Rig::new();
        r.cmd(r#"{"cmd":"save_preset","preset":{"id":"","name":"  ","destinations":[{"id":"","path":"/x"},{"path":"","smb_url":"smb://nas/a"}]}}"#);
        let p = r.presets()[1].clone();
        assert!(p.id.starts_with('p'));
        assert_eq!(p.name, "Untitled");
        assert!(p.destinations[0].id.starts_with('d'));
        assert_eq!(p.destinations[0].kind, config::DestKind::Folder);
        assert_eq!(p.destinations[1].kind, config::DestKind::Share, "kindless share from an old client");
        assert_eq!(r.toast(), Some(("ok".into(), "Preset \u{201c}Untitled\u{201d} saved".into())));
        assert_ne!(r.active(), p.id);
        assert!(r.audio_cmds().is_empty(), "inactive preset: audio untouched");
        // replace, activate
        r.cmd(&format!(r#"{{"cmd":"save_preset","preset":{{"id":"{}","name":"Live"}},"activate":true}}"#, p.id));
        assert_eq!(r.presets().len(), 2);
        assert_eq!(r.presets()[1].name, "Live");
        assert_eq!(r.active(), p.id);
        assert!(matches!(&r.audio_cmds()[..], [AudioCmd::Configure(_)]));
        assert_eq!(r.saved().presets[1].name, "Live");
    }

    #[test]
    fn active_preset_is_locked_while_recording() {
        let r = Rig::new();
        let active = r.active();
        r.add_preset("other");
        r.ready(vec![0, 1]);
        r.start();
        r.cmd(&format!(r#"{{"cmd":"save_preset","preset":{{"id":"{active}","name":"x"}}}}"#));
        assert_eq!(r.toast().unwrap().1, "Stop recording before changing the active preset");
        r.cmd(r#"{"cmd":"save_preset","preset":{"id":"other","name":"x"},"activate":true}"#);
        assert_eq!(r.toast().unwrap().1, "Stop recording before changing the active preset");
        r.cmd(r#"{"cmd":"save_preset","preset":{"id":"other","name":"edited"}}"#);
        assert_eq!(r.presets()[1].name, "edited", "other presets can be edited mid-take");
        r.cmd(&format!(r#"{{"cmd":"delete_preset","id":"{active}"}}"#));
        assert_eq!(r.toast().unwrap().1, "Stop recording first");
        r.cmd(r#"{"cmd":"quit"}"#);
        assert_eq!(r.toast().unwrap().1, "Stop recording before quitting");
    }

    #[test]
    fn delete_preset() {
        let r = Rig::new();
        let first = r.active();
        r.cmd(&format!(r#"{{"cmd":"delete_preset","id":"{first}"}}"#));
        assert_eq!(r.toast().unwrap().1, "Keep at least one preset");
        r.add_preset("b");
        r.add_preset("c");
        r.cmd(r#"{"cmd":"delete_preset","id":"c"}"#);
        assert_eq!(r.presets().len(), 2);
        assert_eq!(r.active(), first);
        r.audio_cmds();
        r.cmd(&format!(r#"{{"cmd":"delete_preset","id":"{first}"}}"#));
        assert_eq!(r.active(), "b", "active deleted: next one takes over");
        assert!(matches!(&r.audio_cmds()[..], [AudioCmd::Configure(s)] if s.preset.id == "b"));
        assert_eq!(r.saved().presets.len(), 1);
    }

    #[test]
    fn save_settings() {
        let r = Rig::new();
        r.cmd(r#"{"cmd":"save_settings","settings":{"fullscreen":false,"remote_enabled":false}}"#);
        let s = r.saved().settings;
        assert!(!s.fullscreen && !s.remote_enabled);
        assert_eq!(r.toast(), None);
        r.cmd(r#"{"cmd":"save_settings","settings":{"port":80}}"#);
        assert_eq!(r.saved().settings.port, 7878, "privileged ports refused");
        r.cmd(r#"{"cmd":"save_settings","settings":{"port":9000}}"#);
        assert_eq!(r.saved().settings.port, 9000);
        assert_eq!(r.toast().unwrap().1, "New port is used after the next app start");
    }

    #[test]
    fn window_commands() {
        let r = Rig::new();
        // no window yet: nothing breaks
        r.cmd(r#"{"cmd":"fullscreen"}"#);
        r.cmd(r#"{"cmd":"pick_folder","req":"r0"}"#);
        r.cmd(r#"{"cmd":"quit"}"#);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = log.clone();
        r.e.set_window_handler(Box::new(move |c| {
            l2.lock().push(match c {
                WindowCmd::Fullscreen(on) => format!("fs {on}"),
                WindowCmd::PickFolder { req, start } => format!("pick {req} {start}"),
                WindowCmd::Quit => "quit".into(),
            })
        }));
        let fs0 = r.e.settings().fullscreen;
        r.cmd(r#"{"cmd":"fullscreen"}"#);
        r.cmd(r#"{"cmd":"fullscreen","on":true}"#);
        r.cmd(r#"{"cmd":"pick_folder","req":"r1","start":"/tmp"}"#);
        r.cmd(r#"{"cmd":"pick_folder","req":"r2"}"#);
        r.cmd(r#"{"cmd":"quit"}"#);
        assert_eq!(*log.lock(), [format!("fs {}", !fs0), "fs true".into(), "pick r1 /tmp".into(), "pick r2 ".into(), "quit".into()]);
        assert!(r.saved().settings.fullscreen);
        // picked folder goes back to all UIs
        let mut sub = r.e.subscribe();
        r.e.picked("r1", Some("/music".into()));
        r.e.picked("r2", None);
        let a: Value = serde_json::from_str(&sub.try_recv().unwrap()).unwrap();
        let b: Value = serde_json::from_str(&sub.try_recv().unwrap()).unwrap();
        assert_eq!(a, json!({"t":"picked","req":"r1","path":"/music"}));
        assert_eq!(b["path"], Value::Null);
    }

    #[test]
    fn misc_commands() {
        let r = Rig::new();
        r.cmd(r#"{"cmd":"rescan"}"#);
        assert!(matches!(r.audio_cmds()[..], [AudioCmd::Rescan]));
        let mut j = Job::new(PathBuf::from("x.raw"), TakeMeta {
            take_name: "t".into(), rel_path: "t".into(), preset_name: "p".into(), sample_rate: 1, track_names: vec![], format: config::Format::Wav24, split_tracks: false, started_at: 0,
            destinations: vec![Destination { path: "/x".into(), ..Default::default() }],
        }, 1);
        j.tasks[0].state = TaskState::Failed;
        j.tasks[0].next_try = i64::MAX;
        r.e.jobs.list.lock().push(j);
        r.cmd(r#"{"cmd":"retry_jobs"}"#);
        assert_eq!(r.e.jobs.list.lock()[0].tasks[0].next_try, 0);
        r.e.toast("ok", "hi");
        r.cmd(r#"{"cmd":"dismiss_toast"}"#);
        assert_eq!(r.toast(), None);
    }

    #[test]
    fn test_destination_answers_the_asking_ui() {
        let r = Rig::new();
        let mut sub = r.e.subscribe();
        let ok = serde_json::to_string(&Destination { path: r.dir.join("out").to_string_lossy().into(), ..Default::default() }).unwrap();
        r.cmd(&format!(r#"{{"cmd":"test_dest","req":"a","dest":{ok}}}"#));
        r.cmd(r#"{"cmd":"test_dest","req":"b","dest":{"path":"/nonexistent/x/y"}}"#);
        let mut got = std::collections::HashMap::new();
        assert!(wait_for(10, || {
            while let Ok(m) = sub.try_recv() {
                let v: Value = serde_json::from_str(&m).unwrap();
                got.insert(v["req"].as_str().unwrap().to_string(), v);
            }
            got.len() == 2
        }));
        assert_eq!(got["a"]["ok"], true);
        assert!(got["a"]["free"].as_u64().unwrap() > 0);
        assert!(got["a"]["path"].as_str().unwrap().ends_with("out"));
        assert_eq!(got["b"]["ok"], false);
        assert_eq!(got["b"]["msg"], "Destination not reachable");
        r.remote(r#"{"cmd":"test_dest","req":"c","dest":{}}"#);
        assert_eq!(r.toast().unwrap().1, "Settings can only be changed on the recording computer");
    }

    #[test]
    fn toasts_get_increasing_ids() {
        let r = Rig::new();
        r.e.toast("ok", "a");
        let a = r.e.snapshot()["toast"]["id"].as_u64().unwrap();
        r.e.toast("error", "b");
        let s = r.e.snapshot();
        assert_eq!(s["toast"]["id"].as_u64().unwrap(), a + 1);
        assert_eq!(s["toast"]["level"], "error");
        assert_eq!(s["toast"]["msg"], "b");
    }

    #[test]
    fn config_save_failure_is_toasted() {
        let r = Rig::new();
        r.e.store.lock().path = r.dir.join("missing\0/config.json");
        r.cmd(r#"{"cmd":"save_settings","settings":{"fullscreen":false}}"#);
        assert!(r.toast().unwrap().1.starts_with("Could not save settings"));
    }

    #[test]
    fn rolling_waveform_and_preroll_history() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        let bins = |v: u8, n: usize| vec![vec![[v, v]; n], vec![[v, 0]; n]];
        // more than 30 s of bins: oldest are dropped
        r.e.on_audio(AudioEvent::Live { meters: vec![], bins: bins(1, ROLLING_BINS), rec_frames: 0 });
        r.e.on_audio(AudioEvent::Live { meters: vec![], bins: bins(2, 300), rec_frames: 0 });
        {
            let rt = r.e.rt.lock();
            assert_eq!(rt.rolling[0].len(), ROLLING_BINS);
            assert_eq!(rt.rolling[0].back(), Some(&[2, 2]));
            assert_eq!(rt.rolling[0][ROLLING_BINS - 301], [1, 1]);
            assert!(rt.take_bins.is_empty(), "not recording: no take waveform");
        }
        r.start();
        // 1.5 s pre-roll at 1000 Hz = 150 bins copied into the take
        r.e.on_audio(AudioEvent::RecStarted { preroll_frames: 1500 });
        assert_eq!(r.e.rt.lock().take_bins[0].len(), 150);
        assert_eq!(r.e.rt.lock().rec.as_ref().unwrap().frames, 1500);
        let h = r.e.history();
        assert_eq!(h["recording"], true);
        assert_eq!(h["bps"], BINS_PER_SEC);
        assert_eq!(h["rolling"][0].as_array().unwrap().len(), ROLLING_BINS * 2);
        // channel count changes mid-stream: buffers follow
        r.e.on_audio(AudioEvent::Live { meters: vec![], bins: vec![vec![[9, 9]]], rec_frames: 1600 });
        let rt = r.e.rt.lock();
        assert_eq!(rt.rolling.len(), 1);
        assert_eq!(rt.take_bins.len(), 1);
    }

    #[test]
    fn live_messages_are_broadcast() {
        let r = Rig::new();
        let mut sub = r.e.subscribe();
        r.e.on_audio(AudioEvent::Live { meters: vec![0.5, -1.0, 9.0], bins: vec![vec![[1, 2], [3, 4]]], rec_frames: 7 });
        let m: Value = serde_json::from_str(&sub.try_recv().unwrap()).unwrap();
        assert_eq!(m, json!({"t":"live","m":[500, 0, 2000],"b":[[1,2,3,4]],"f":7,"r":false}));
    }

    #[test]
    fn status_and_device_events() {
        let r = Rig::new();
        r.ready(vec![0, 1]);
        r.e.on_audio(AudioEvent::Live { meters: vec![], bins: vec![vec![[1, 1]], vec![[1, 1]]], rec_frames: 0 });
        r.ready(vec![0, 1]);
        assert_eq!(r.e.rt.lock().rolling[0].len(), 1, "same routing keeps the waveform");
        r.ready(vec![1]);
        assert_eq!(r.e.rt.lock().rolling.len(), 1);
        assert!(r.e.rt.lock().rolling[0].is_empty(), "new routing starts fresh");
        let dev = |n: &str| DeviceInfo { name: n.into(), channels: 2, rates: vec![], is_default: false };
        r.e.dirty.store(false, Ordering::Relaxed);
        r.e.on_audio(AudioEvent::Devices(vec![dev("A")]));
        assert!(r.e.dirty.swap(false, Ordering::Relaxed));
        r.e.on_audio(AudioEvent::Devices(vec![dev("A")]));
        assert!(!r.e.dirty.load(Ordering::Relaxed), "unchanged list: no redraw");
        assert_eq!(r.e.snapshot()["devices"][0]["name"], "A");
    }

    #[test]
    fn snapshot_contents() {
        let r = Rig::new();
        let s = r.e.snapshot();
        assert_eq!(s["t"], "state");
        assert_eq!(s["ready"], false);
        assert_eq!(s["tracks"], json!(["Left", "Right"]), "track names from the preset before audio is up");
        assert_eq!(s["rec"], Value::Null);
        assert_eq!(s["dest_ready"], false);
        assert_eq!(s["platform"], std::env::consts::OS);
        assert!(s["spool_free"].as_u64().unwrap() > 0);
        r.e.on_audio(AudioEvent::Status(AudioStatus { state: AudioState::Ready, message: String::new(), device: "F".into(), sample_rate: 1000, device_channels: 8, selected: vec![4, 6], dropouts: 0 }));
        let mut p = r.presets()[0].clone();
        p.mode = Mode::Multitrack;
        p.track_names = vec!["Kick".into()];
        r.cmd(&format!(r#"{{"cmd":"save_preset","preset":{}}}"#, serde_json::to_string(&p).unwrap()));
        let s = r.e.snapshot();
        assert_eq!(s["ready"], true);
        assert_eq!(s["tracks"], json!(["Kick", "In 7"]));
        // job states
        let mk = |states: &[TaskState]| {
            let mut j = Job::new(PathBuf::from("x"), TakeMeta {
                take_name: "t".into(), rel_path: "t".into(), preset_name: "p".into(), sample_rate: 1, track_names: vec![], format: config::Format::Wav24, split_tracks: false, started_at: 0, destinations: vec![],
            }, 1);
            j.tasks = states.iter().map(|st| DestTask { label: "d".into(), dest: Destination::default(), state: st.clone(), progress: 0.0, message: String::new(), attempts: 0, next_try: 0, out_path: String::new() }).collect();
            j
        };
        {
            let mut l = r.e.jobs.list.lock();
            l.push(mk(&[TaskState::Pending]));
            l.push(mk(&[TaskState::Done, TaskState::Busy]));
            l.push(mk(&[TaskState::Busy, TaskState::Failed]));
            l.push(mk(&[TaskState::Done]));
            for _ in 0..12 {
                l.push(mk(&[TaskState::Done]));
            }
        }
        let s = r.e.snapshot();
        let states: Vec<_> = s["jobs"].as_array().unwrap().iter().map(|j| j["state"].as_str().unwrap().to_string()).collect();
        assert_eq!(states.len(), 12, "UI gets the newest 12");
        assert!(states.iter().all(|x| x == "done"));
        r.e.jobs.list.lock().truncate(4);
        let s = r.e.snapshot();
        let states: Vec<_> = s["jobs"].as_array().unwrap().iter().map(|j| j["state"].as_str().unwrap().to_string()).collect();
        assert_eq!(states, ["done", "failed", "busy", "pending"], "newest first");
        assert_eq!(s["queue_open"], 3);
    }

    #[test]
    fn destinations_ready_flag() {
        let r = Rig::new();
        r.e.apply_active();
        assert!(wait_for(10, || r.e.snapshot()["dest_ready"] == true));
    }

    #[test]
    fn publish_only_when_dirty() {
        let r = Rig::new();
        let mut sub = r.e.subscribe();
        assert!(!r.e.publish_if_dirty(false));
        assert!(sub.try_recv().is_err());
        r.e.toast("ok", "x");
        assert!(r.e.publish_if_dirty(false));
        assert_eq!(serde_json::from_str::<Value>(&sub.try_recv().unwrap()).unwrap()["t"], "state");
        assert!(r.e.publish_if_dirty(true), "forced heartbeat");
    }

    #[test]
    fn server_info_and_qr() {
        let r = Rig::new();
        r.e.set_server_info(7999);
        let s = r.e.snapshot();
        assert_eq!(s["remote"]["port"], 7999);
        let urls = s["remote"]["urls"].as_array().unwrap();
        assert!(urls.iter().all(|u| u.as_str().unwrap().starts_with("http://") && u.as_str().unwrap().ends_with(":7999")));
        if !urls.is_empty() {
            assert!(s["remote"]["qr"].as_str().unwrap().starts_with("<?xml"));
        }
    }

    #[test]
    fn effective_destinations_never_empty() {
        let mut p = Preset::default();
        p.destinations[0].enabled = false;
        p.destinations.push(Destination { path: "  ".into(), ..Default::default() });
        let d = effective_dests(&p);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, "fallback");
        assert!(!d[0].path.is_empty());
        p.destinations.push(Destination { path: "/nas".into(), ..Default::default() });
        assert_eq!(effective_dests(&p)[0].path, "/nas");
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(0.0), "0:00");
        assert_eq!(fmt_dur(65.9), "1:05");
        assert_eq!(fmt_dur(3599.0), "59:59");
        assert_eq!(fmt_dur(3600.0), "1:00:00");
        assert_eq!(fmt_dur(7322.0), "2:02:02");
    }

    #[test]
    fn keep_awake_helper_starts() {
        // present on macOS (caffeinate) and most Linux desktops (systemd-inhibit)
        if let Some(mut c) = keep_awake() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// The whole pipeline with real threads: fake audio -> engine -> spool ->
    /// job worker -> WAV in the destination folder.
    #[test]
    fn end_to_end_take_lands_as_wav() {
        let dir = TempDir::new("e2e");
        let out = dir.join("Takes");
        let mut store = Store::load(&dir.join("config.json"));
        store.presets[0].destinations[0].path = out.to_string_lossy().into();
        store.presets[0].sample_rate = 1000;
        store.presets[0].preroll_secs = 0.0;
        store.presets[0].filename = "{preset} {take}".into();
        store.save().unwrap();
        let fake = Fake::new(2, 1000);
        let e = Engine::start_with(&dir.join("config.json"), dir.join("data"), fake.clone(), no_awake);
        assert!(wait_for(10, || e.snapshot()["ready"] == true), "audio opens");
        assert!(wait_for(10, || e.snapshot()["dest_ready"] == true), "destination checked");
        e.handle(r#"{"cmd":"toggle"}"#, true);
        assert!(wait_for(10, || e.snapshot()["rec"]["started"] == true));
        let frames: Vec<f32> = (0..500).flat_map(|i| [i as f32 / 1000.0, -(i as f32) / 1000.0]).collect();
        fake.feed(&frames);
        assert!(wait_for(10, || e.snapshot()["rec"]["frames"].as_u64() == Some(500)));
        e.rt.lock().last_toggle = None;
        e.handle(r#"{"cmd":"toggle"}"#, true);
        let wav = out.join("Band Practice 01.wav");
        assert!(wait_for(20, || wav.exists()), "delivered");
        assert!(wait_for(10, || e.snapshot()["queue_open"] == 0));
        let b = std::fs::read(&wav).unwrap();
        assert_eq!(b.len(), 104 + 500 * 2 * 3);
        let s = |i: usize| i32::from_le_bytes([0, b[104 + i * 3], b[105 + i * 3], b[106 + i * 3]]) >> 8;
        assert_eq!(s(2 * 250), (0.25f64 * 8_388_607.0).round() as i32);
        assert_eq!(s(2 * 250 + 1), -(0.25f64 * 8_388_607.0).round() as i32);
        assert!(std::fs::read_dir(dir.join("data/spool")).unwrap().flatten().all(|f| f.path().extension().is_none_or(|x| x != "raw")), "spool cleaned");
        assert_eq!(e.settings().take_counter, 1);
    }
}
