//! Background delivery queue. A stopped take becomes a job; every destination
//! is an independent task (render spool -> `.part` -> rename). Tasks retry with
//! backoff forever, survive restarts (jobs.json) and never block recording.

use crate::config::{sanitize, write_atomic, DestKind, Destination, Format};
use crate::wav::WavWriter;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TakeMeta {
    pub take_name: String,
    /// Template-expanded relative path, without extension.
    pub rel_path: PathBuf,
    pub preset_name: String,
    pub sample_rate: u32,
    pub track_names: Vec<String>,
    pub format: Format,
    pub split_tracks: bool,
    pub started_at: i64,
    pub destinations: Vec<Destination>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Pending,
    Busy,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DestTask {
    pub label: String,
    pub dest: Destination,
    pub state: TaskState,
    pub progress: f32,
    pub message: String,
    pub attempts: u32,
    pub next_try: i64,
    pub out_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub raw: PathBuf,
    pub meta: TakeMeta,
    pub frames: u64,
    pub created: i64,
    pub tasks: Vec<DestTask>,
    pub recovered: bool,
    pub note: String,
}

impl Job {
    pub fn done(&self) -> bool {
        self.tasks.iter().all(|t| t.state == TaskState::Done)
    }
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.meta.sample_rate.max(1) as f64
    }
}

pub struct Jobs {
    pub list: Mutex<Vec<Job>>,
    notify: Box<dyn Fn() + Send + Sync>,
    /// App data folder: holds `spool/` and `jobs.json`.
    dir: PathBuf,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Jobs {
    pub fn start(dir: PathBuf, notify: Box<dyn Fn() + Send + Sync>, workers: usize) -> Arc<Jobs> {
        let jobs = Jobs::load(dir, notify);
        for i in 0..workers.max(1) {
            let j = jobs.clone();
            std::thread::Builder::new().name(format!("psr-job-{i}")).spawn(move || j.worker()).expect("job worker");
        }
        jobs
    }

    pub fn spool_dir(&self) -> PathBuf {
        self.dir.join("spool")
    }

    fn jobs_file(&self) -> PathBuf {
        self.dir.join("jobs.json")
    }

    /// Read jobs.json, reset interrupted tasks and adopt orphaned spool files.
    /// No worker threads: see [`Jobs::start`].
    pub fn load(dir: PathBuf, notify: Box<dyn Fn() + Send + Sync>) -> Arc<Jobs> {
        let file = dir.join("jobs.json");
        let mut list: Vec<Job> = match fs::read(&file) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                // Unfinished takes are still rescued below from their spool files.
                log::error!("jobs.json unreadable: {e}");
                let _ = fs::copy(&file, file.with_extension("json.broken"));
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        for j in &mut list {
            for t in &mut j.tasks {
                if t.state == TaskState::Busy {
                    t.state = TaskState::Pending;
                }
                t.next_try = 0;
            }
        }
        // spool files nobody owns: a crash or power cut mid-take. Rescue them.
        if let Ok(rd) = fs::read_dir(dir.join("spool")) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "raw").unwrap_or(false) && !list.iter().any(|j| j.raw == p) {
                    if let Some(meta) = fs::read(p.with_extension("json")).ok().and_then(|b| serde_json::from_slice::<TakeMeta>(&b).ok()) {
                        let n = meta.track_names.len().max(1) as u64;
                        let frames = e.metadata().map(|m| m.len()).unwrap_or(0) / (4 * n);
                        log::warn!("recovering orphaned take {}", meta.take_name);
                        let mut j = Job::new(p, meta, frames);
                        j.recovered = true;
                        j.note = "Recovered after unexpected shutdown".into();
                        list.push(j);
                    }
                }
            }
        }
        let jobs = Arc::new(Jobs { list: Mutex::new(list), notify, dir });
        jobs.persist();
        jobs
    }

    pub fn push(&self, job: Job) {
        self.list.lock().push(job);
        self.persist();
        (self.notify)();
    }

    pub fn retry_all(&self) {
        for j in self.list.lock().iter_mut() {
            for t in &mut j.tasks {
                if t.state == TaskState::Failed {
                    t.next_try = 0;
                }
            }
        }
        (self.notify)();
    }

    pub fn persist(&self) {
        let list = self.list.lock();
        // keep the last few finished jobs for the UI history, all unfinished ones
        let keep: Vec<&Job> = {
            let done: Vec<&Job> = list.iter().filter(|j| j.done()).collect();
            let skip = done.len().saturating_sub(30);
            list.iter().filter(|j| !j.done() || done.iter().skip(skip).any(|d| d.id == j.id)).collect()
        };
        if let Ok(b) = serde_json::to_vec_pretty(&keep) {
            if let Err(e) = write_atomic(&self.jobs_file(), &b) {
                log::error!("jobs.json save failed: {e}");
            }
        }
    }

    fn worker(self: Arc<Self>) {
        loop {
            if !self.run_once() {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }

    /// Deliver one due task. Returns false when nothing was due.
    pub fn run_once(&self) -> bool {
        let pick = {
            let mut list = self.list.lock();
            let now = now_ms();
            let mut found = None;
            'o: for j in list.iter_mut() {
                for (ti, t) in j.tasks.iter_mut().enumerate() {
                    if (t.state == TaskState::Pending || t.state == TaskState::Failed) && t.next_try <= now {
                        t.state = TaskState::Busy;
                        t.progress = 0.0;
                        t.message = "Writing".into();
                        found = Some((j.id.clone(), ti, j.clone()));
                        break 'o;
                    }
                }
            }
            found
        };
        let Some((id, ti, job)) = pick else { return false };
        (self.notify)();
        let result = deliver(&job, &job.tasks[ti].dest, &mut |p| {
            if let Some(j) = self.list.lock().iter_mut().find(|j| j.id == id) {
                j.tasks[ti].progress = p;
            }
            (self.notify)();
        });
        let finished = {
            let mut list = self.list.lock();
            let Some(j) = list.iter_mut().find(|j| j.id == id) else { return true };
            let t = &mut j.tasks[ti];
            match result {
                Ok(out) => {
                    log::info!("delivered {} -> {}", j.meta.take_name, out);
                    t.state = TaskState::Done;
                    t.progress = 1.0;
                    t.message = "Delivered".into();
                    t.out_path = out;
                }
                Err(e) => {
                    t.attempts += 1;
                    let wait = [5, 15, 30, 60][(t.attempts as usize - 1).min(3)];
                    log::warn!("delivery of {} to {} failed (attempt {}): {e}", j.meta.take_name, t.label, t.attempts);
                    t.state = TaskState::Failed;
                    t.message = e;
                    t.next_try = now_ms() + wait * 1000;
                }
            }
            j.done().then(|| j.raw.clone())
        };
        if let Some(raw) = finished {
            let _ = fs::remove_file(&raw);
            let _ = fs::remove_file(raw.with_extension("json"));
        }
        self.persist();
        (self.notify)();
        true
    }
}

impl Job {
    pub fn new(raw: PathBuf, meta: TakeMeta, frames: u64) -> Job {
        let tasks = meta
            .destinations
            .iter()
            .filter(|d| d.usable())
            .map(|d| DestTask {
                label: d.display_name(),
                dest: d.clone(),
                state: TaskState::Pending,
                progress: 0.0,
                message: "Queued".into(),
                attempts: 0,
                next_try: 0,
                out_path: String::new(),
            })
            .collect();
        Job { id: crate::config::new_id("j"), raw, meta, frames, created: now_ms(), tasks, recovered: false, note: String::new() }
    }
}

/// Pick a name that does not exist yet: "x.wav", "x (2).wav", ...
fn unique(base: &Path, ext: &str) -> PathBuf {
    let mk = |n: u32| {
        let name = base.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let name = if n <= 1 { name } else { format!("{name} ({n})") };
        base.with_file_name(if ext.is_empty() { name } else { format!("{name}.{ext}") })
    };
    (1..10_000).map(mk).find(|p| !p.exists()).unwrap_or_else(|| mk(1))
}

/// Deliver one take to one destination. Returns what to show as "where it went".
pub fn deliver(job: &Job, dest: &Destination, progress: &mut dyn FnMut(f32)) -> Result<String, String> {
    match dest.kind {
        DestKind::Folder | DestKind::Share => deliver_files(job, dest, progress),
        // new kinds get their own deliver_* here (upload instead of file copy)
        DestKind::Unsupported => Err(crate::dest::UNSUPPORTED.into()),
    }
}

/// Render the spool into WAV file(s) in a folder (local, or a mounted share).
fn deliver_files(job: &Job, dest: &Destination, progress: &mut dyn FnMut(f32)) -> Result<String, String> {
    let root = crate::dest::reach(dest)?;
    let m = &job.meta;
    let n = m.track_names.len().max(1);
    let raw_len = fs::metadata(&job.raw).map_err(|e| format!("Spool file missing: {e}"))?.len();
    let frames = raw_len / (4 * n as u64);
    let base = root.join(&m.rel_path);
    if let Some(parent) = base.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create folder: {e}"))?;
    }
    let split = m.split_tracks && n > 1;
    // (final path, temp path) for each output file
    let outputs: Vec<(PathBuf, PathBuf)> = if split {
        let dir = unique(&base, "");
        fs::create_dir_all(&dir).map_err(|e| format!("Cannot create folder: {e}"))?;
        m.track_names
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let f = dir.join(format!("{:02} {}.wav", i + 1, sanitize(t)));
                let tmp = f.with_extension("wav.part");
                (f, tmp)
            })
            .collect()
    } else {
        let f = unique(&base, "wav");
        let tmp = f.with_extension("wav.part");
        vec![(f, tmp)]
    };
    let (bits, float) = (m.format.bits(), m.format.is_float());
    let mut writers = Vec::new();
    for (_, tmp) in &outputs {
        let ch = if split { 1 } else { n as u16 };
        writers.push(WavWriter::create(tmp, m.sample_rate, ch, bits, float).map_err(|e| format!("Cannot write: {e}"))?);
    }
    let cleanup = |outputs: &[(PathBuf, PathBuf)]| {
        for (_, t) in outputs {
            let _ = fs::remove_file(t);
        }
    };
    let mut rd = BufReader::with_capacity(1 << 20, File::open(&job.raw).map_err(|e| e.to_string())?);
    let block = 16384usize;
    let mut bytes = vec![0u8; block * n * 4];
    let mut samples = vec![0f32; block * n];
    let mut per_track: Vec<Vec<f32>> = vec![Vec::with_capacity(block); if split { n } else { 0 }];
    let mut done = 0u64;
    let mut last_report = 0u64;
    while done < frames {
        let want = ((frames - done) as usize).min(block);
        let buf = &mut bytes[..want * n * 4];
        if let Err(e) = rd.read_exact(buf) {
            cleanup(&outputs);
            return Err(format!("Spool read failed: {e}"));
        }
        for (s, b) in samples.iter_mut().zip(buf.chunks_exact(4)) {
            *s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
        let s = &samples[..want * n];
        let r = if split {
            for t in per_track.iter_mut() {
                t.clear();
            }
            for fr in s.chunks_exact(n) {
                for (i, &v) in fr.iter().enumerate() {
                    per_track[i].push(v);
                }
            }
            writers.iter_mut().zip(&per_track).try_for_each(|(w, t)| w.write(t))
        } else {
            writers[0].write(s)
        };
        if let Err(e) = r {
            cleanup(&outputs);
            return Err(format!("Write failed: {e}"));
        }
        done += want as u64;
        if done - last_report > m.sample_rate as u64 * 5 {
            last_report = done;
            progress(done as f32 / frames.max(1) as f32 * 0.98);
        }
    }
    for w in writers {
        if let Err(e) = w.finalize() {
            cleanup(&outputs);
            return Err(format!("Finalize failed: {e}"));
        }
    }
    for (f, tmp) in &outputs {
        fs::rename(tmp, f).map_err(|e| format!("Rename failed: {e}"))?;
    }
    let shown = if split { outputs[0].0.parent().unwrap().to_path_buf() } else { outputs[0].0.clone() };
    Ok(shown.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Destination;
    use crate::testutil::{wait_for, TempDir};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dest(path: impl Into<String>) -> Destination {
        Destination { path: path.into(), ..Default::default() }
    }

    fn meta(tracks: &[&str], dests: Vec<Destination>) -> TakeMeta {
        TakeMeta {
            take_name: "take".into(),
            rel_path: PathBuf::from("day/take"),
            preset_name: "p".into(),
            sample_rate: 48000,
            track_names: tracks.iter().map(|s| s.to_string()).collect(),
            format: Format::Wav24,
            split_tracks: false,
            started_at: 0,
            destinations: dests,
        }
    }

    /// Interleaved ramp, `ch` channels, channel 1 inverted.
    fn raw(path: &Path, frames: u64, ch: usize) {
        let mut b = Vec::new();
        for i in 0..frames {
            for c in 0..ch {
                b.extend_from_slice(&((i as f32 / frames as f32) * if c == 1 { -1.0 } else { 1.0 }).to_le_bytes());
            }
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &b).unwrap();
    }

    fn counter() -> (Arc<AtomicUsize>, Box<dyn Fn() + Send + Sync>) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        (n, Box::new(move || {
            n2.fetch_add(1, Ordering::Relaxed);
        }))
    }

    #[test]
    fn new_job_skips_disabled_and_blank_destinations() {
        let mut off = dest("/x");
        off.enabled = false;
        let mut unlabeled = dest("/y");
        unlabeled.label = " ".into();
        let j = Job::new(PathBuf::from("r.raw"), meta(&["a"], vec![off, dest("  "), unlabeled, dest("/z")]), 96000);
        assert_eq!(j.tasks.len(), 2);
        assert_eq!(j.tasks[0].label, "/y");
        assert_eq!(j.tasks[1].label, "Local");
        assert!(j.tasks.iter().all(|t| t.state == TaskState::Pending));
        assert!(!j.done());
        assert_eq!(j.seconds(), 2.0);
        let mut m = meta(&["a"], vec![]);
        m.sample_rate = 0;
        assert_eq!(Job::new(PathBuf::new(), m, 10).seconds(), 10.0, "no division by zero");
    }

    #[test]
    fn unique_names() {
        let d = TempDir::new("jobs");
        let base = d.join("take");
        assert_eq!(unique(&base, "wav"), d.join("take.wav"));
        fs::write(d.join("take.wav"), b"").unwrap();
        fs::write(d.join("take (2).wav"), b"").unwrap();
        assert_eq!(unique(&base, "wav"), d.join("take (3).wav"));
        assert_eq!(unique(&base, ""), d.join("take"));
        fs::create_dir(d.join("take")).unwrap();
        assert_eq!(unique(&base, ""), d.join("take (2)"));
    }

    #[test]
    fn delivers_split_and_interleaved() {
        let d = TempDir::new("jobs");
        let r = d.join("t.raw");
        let frames = 1000u64;
        raw(&r, frames, 3);
        let ds = dest(d.join("out").to_string_lossy());
        fs::create_dir_all(&ds.path).unwrap();
        let mut m = meta(&["Kick", "Snare", "Gtr/L"], vec![ds.clone()]);
        m.split_tracks = true;
        let job = Job::new(r.clone(), m.clone(), frames);
        let out = deliver(&job, &ds, &mut |_| {}).unwrap();
        let dir = PathBuf::from(&out);
        assert!(dir.ends_with("day/take"));
        assert!(dir.join("01 Kick.wav").exists());
        assert!(dir.join("03 Gtr-L.wav").exists());
        assert_eq!(fs::metadata(dir.join("02 Snare.wav")).unwrap().len(), 104 + frames * 3);
        // split writes channel 1 alone: check it is the inverted ramp
        let snare = fs::read(dir.join("02 Snare.wav")).unwrap();
        let s500 = i32::from_le_bytes([0, snare[104 + 1500], snare[105 + 1500], snare[106 + 1500]]) >> 8;
        assert_eq!(s500, -4_194_304);
        // second split delivery gets its own folder
        let out2 = deliver(&job, &ds, &mut |_| {}).unwrap();
        assert!(out2.ends_with("take (2)"));
        m.split_tracks = false;
        let job = Job::new(r, m, frames);
        let out = deliver(&job, &ds, &mut |_| {}).unwrap();
        assert!(out.ends_with("take.wav"));
        assert_eq!(fs::metadata(&out).unwrap().len(), 104 + frames * 9);
        let out2 = deliver(&job, &ds, &mut |_| {}).unwrap();
        assert!(out2.ends_with("take (2).wav"));
        // no .part files left anywhere
        let parts = fs::read_dir(d.join("out/day")).unwrap().flatten().filter(|e| e.path().to_string_lossy().ends_with(".part")).count();
        assert_eq!(parts, 0);
    }

    #[test]
    fn split_with_one_track_writes_single_file() {
        let d = TempDir::new("jobs");
        let r = d.join("t.raw");
        raw(&r, 10, 1);
        let ds = dest(d.str());
        let mut m = meta(&["Mono"], vec![ds.clone()]);
        m.split_tracks = true;
        m.format = Format::Wav16;
        let out = deliver(&Job::new(r, m, 10), &ds, &mut |_| {}).unwrap();
        assert!(out.ends_with("take.wav"));
        assert_eq!(fs::metadata(&out).unwrap().len(), 104 + 20);
    }

    #[test]
    fn reports_progress_on_long_takes() {
        let d = TempDir::new("jobs");
        let r = d.join("t.raw");
        raw(&r, 40_000, 2);
        let ds = dest(d.str());
        let mut m = meta(&["L", "R"], vec![ds.clone()]);
        m.sample_rate = 1000;
        m.format = Format::Wav32f;
        let mut seen = vec![];
        deliver(&Job::new(r, m, 40_000), &ds, &mut |p| seen.push(p)).unwrap();
        assert!(!seen.is_empty());
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        assert!(seen.iter().all(|&p| p > 0.0 && p < 1.0));
    }

    #[test]
    fn delivery_errors() {
        let d = TempDir::new("jobs");
        let r = d.join("t.raw");
        raw(&r, 10, 1);
        let m = meta(&["a"], vec![]);
        let job = Job::new(r.clone(), m.clone(), 10);
        assert_eq!(deliver(&job, &dest(d.join("nope/deeper").to_string_lossy()), &mut |_| {}).unwrap_err(), "Destination not reachable");
        let missing = Job::new(d.join("gone.raw"), m.clone(), 10);
        assert!(deliver(&missing, &dest(d.str()), &mut |_| {}).unwrap_err().starts_with("Spool file missing"));
        // "day" exists as a file: folder can't be made
        let blocked = TempDir::new("jobs");
        fs::write(blocked.join("day"), b"").unwrap();
        assert!(deliver(&job, &dest(blocked.str()), &mut |_| {}).unwrap_err().starts_with("Cannot create folder"));
        // the temp file path is taken by a folder
        let busy = TempDir::new("jobs");
        fs::create_dir_all(busy.join("day/take.wav.part")).unwrap();
        assert!(deliver(&job, &dest(busy.str()), &mut |_| {}).unwrap_err().starts_with("Cannot write"));
        // split takes: the take folder can't be created in a read-only day folder
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut sm = m.clone();
            sm.track_names = vec!["a".into(), "b".into()];
            sm.split_tracks = true;
            raw(&r, 10, 2);
            let split = Job::new(r.clone(), sm, 10);
            let ro = TempDir::new("jobs");
            fs::create_dir_all(ro.join("day")).unwrap();
            fs::set_permissions(ro.join("day"), fs::Permissions::from_mode(0o555)).unwrap();
            let e = deliver(&split, &dest(ro.str()), &mut |_| {});
            fs::set_permissions(ro.join("day"), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(e.unwrap_err().starts_with("Cannot create folder"));
            raw(&r, 10, 1);
        }
        // spool is a folder: opens on unix, but reading fails
        let dir_raw = d.join("dir.raw");
        fs::create_dir_all(dir_raw.join("x")).unwrap();
        let bad = Job::new(dir_raw, m, 10);
        let out = TempDir::new("jobs");
        let e = deliver(&bad, &dest(out.str()), &mut |_| {}).unwrap_err();
        assert!(e.starts_with("Spool read failed") || e.contains("directory"), "{e}");
        let leftovers: Vec<_> = fs::read_dir(out.join("day")).unwrap().flatten().map(|e| e.path()).collect();
        assert!(leftovers.is_empty(), "temp files cleaned up: {leftovers:?}");
    }

    #[test]
    fn unsupported_kind_is_never_delivered() {
        let d = TempDir::new("jobs");
        let r = d.join("t.raw");
        raw(&r, 10, 1);
        let ds = Destination { kind: DestKind::Unsupported, path: d.str(), ..Default::default() };
        let job = Job::new(r, meta(&["a"], vec![ds.clone()]), 10);
        assert!(job.tasks.is_empty(), "not even queued");
        assert_eq!(deliver(&job, &ds, &mut |_| {}).unwrap_err(), crate::dest::UNSUPPORTED);
    }

    #[test]
    fn worker_delivers_and_removes_spool() {
        let d = TempDir::new("jobs");
        let out = TempDir::new("jobs");
        let (n, notify) = counter();
        let jobs = Jobs::load(d.path().to_path_buf(), notify);
        assert!(!jobs.run_once(), "nothing to do");
        let r = jobs.spool_dir().join("a.raw");
        raw(&r, 100, 2);
        fs::write(r.with_extension("json"), b"{}").unwrap();
        jobs.push(Job::new(r.clone(), meta(&["L", "R"], vec![dest(out.str())]), 100));
        assert!(jobs.run_once());
        let list = jobs.list.lock().clone();
        assert!(list[0].done());
        assert_eq!(list[0].tasks[0].message, "Delivered");
        assert_eq!(list[0].tasks[0].progress, 1.0);
        assert!(list[0].tasks[0].out_path.ends_with("take.wav"));
        assert!(!r.exists() && !r.with_extension("json").exists(), "spool removed after delivery");
        assert!(n.load(Ordering::Relaxed) >= 3);
        assert!(!jobs.run_once());
    }

    #[test]
    fn worker_publishes_progress() {
        let d = TempDir::new("jobs");
        let out = TempDir::new("jobs");
        let (n, notify) = counter();
        let jobs = Jobs::load(d.path().to_path_buf(), notify);
        let r = jobs.spool_dir().join("long.raw");
        raw(&r, 50_000, 1);
        let mut m = meta(&["M"], vec![dest(out.str())]);
        m.sample_rate = 1000;
        jobs.push(Job::new(r, m, 50_000));
        let before = n.load(Ordering::Relaxed);
        assert!(jobs.run_once());
        // claim + 1 per 5 s of audio + finish
        assert!(n.load(Ordering::Relaxed) - before >= 5);
    }

    #[test]
    fn worker_retries_with_backoff_and_keeps_spool() {
        let d = TempDir::new("jobs");
        let out = TempDir::new("jobs");
        let (_, notify) = counter();
        let jobs = Jobs::load(d.path().to_path_buf(), notify);
        let r = jobs.spool_dir().join("a.raw");
        raw(&r, 100, 1);
        let missing = dest(out.join("not-mounted/takes").to_string_lossy());
        jobs.push(Job::new(r.clone(), meta(&["M"], vec![dest(out.str()), missing]), 100));
        let t0 = now_ms();
        assert!(jobs.run_once());
        assert!(jobs.run_once());
        {
            let l = jobs.list.lock();
            assert_eq!(l[0].tasks[0].state, TaskState::Done);
            let t = &l[0].tasks[1];
            assert_eq!(t.state, TaskState::Failed);
            assert_eq!(t.attempts, 1);
            assert_eq!(t.message, "Destination not reachable");
            assert!(t.next_try >= t0 + 5000 && t.next_try <= now_ms() + 5000);
        }
        assert!(r.exists(), "spool kept while a destination is missing");
        assert!(!jobs.run_once(), "not due yet");
        // backoff grows and caps at 60 s
        for (attempt, wait) in [(2, 15), (3, 30), (4, 60), (5, 60)] {
            jobs.list.lock()[0].tasks[1].next_try = 0;
            assert!(jobs.run_once());
            let t = jobs.list.lock()[0].tasks[1].clone();
            assert_eq!(t.attempts, attempt);
            assert!(t.next_try - now_ms() > (wait - 1) * 1000 && t.next_try - now_ms() <= wait * 1000);
        }
        // retry_all makes it due now; share comes back
        fs::create_dir_all(out.join("not-mounted/takes")).unwrap();
        jobs.retry_all();
        assert!(jobs.run_once());
        assert!(jobs.list.lock()[0].done());
        assert!(!r.exists());
    }

    #[test]
    fn job_removed_while_delivering_is_ignored() {
        use std::sync::{OnceLock, Weak};
        let d = TempDir::new("jobs");
        let out = TempDir::new("jobs");
        let me: Arc<OnceLock<Weak<Jobs>>> = Arc::new(OnceLock::new());
        let me2 = me.clone();
        // the first notify fires right after a task is claimed: drop the job there
        let jobs = Jobs::load(d.path().to_path_buf(), Box::new(move || {
            if let Some(j) = me2.get().and_then(Weak::upgrade) {
                j.list.lock().clear();
            }
        }));
        me.set(Arc::downgrade(&jobs)).unwrap();
        let r = d.join("a.raw");
        raw(&r, 10, 1);
        jobs.list.lock().push(Job::new(r.clone(), meta(&["M"], vec![dest(out.str())]), 10));
        assert!(jobs.run_once());
        assert!(jobs.list.lock().is_empty());
        assert!(r.exists(), "nobody owns the result, spool untouched");
    }

    #[test]
    fn persist_keeps_open_jobs_and_last_30_done() {
        let d = TempDir::new("jobs");
        let jobs = Jobs::load(d.path().to_path_buf(), Box::new(|| {}));
        for i in 0..50 {
            let mut j = Job::new(PathBuf::from(format!("{i}.raw")), meta(&["a"], vec![dest("/x")]), 1);
            j.id = format!("j{i}");
            if i % 4 != 0 {
                j.tasks[0].state = TaskState::Done;
            }
            jobs.list.lock().push(j);
        }
        jobs.persist();
        let saved: Vec<Job> = serde_json::from_slice(&fs::read(d.join("jobs.json")).unwrap()).unwrap();
        let open = saved.iter().filter(|j| !j.done()).count();
        let done: Vec<_> = saved.iter().filter(|j| j.done()).map(|j| j.id.clone()).collect();
        assert_eq!(open, 13);
        assert_eq!(done.len(), 30);
        assert!(!done.contains(&"j1".to_string()), "oldest finished dropped first");
        assert!(done.contains(&"j49".to_string()));
        assert!(saved.iter().any(|j| j.id == "j0"), "unfinished jobs are never dropped");
    }

    #[test]
    fn load_resets_busy_and_recovers_orphans() {
        let d = TempDir::new("jobs");
        let spool = d.join("spool");
        // known job, was busy when the app died
        let mut known = Job::new(spool.join("known.raw"), meta(&["a"], vec![dest("/x")]), 1);
        known.tasks[0].state = TaskState::Busy;
        known.tasks[0].next_try = i64::MAX;
        raw(&known.raw, 1, 1);
        fs::write(d.join("jobs.json"), serde_json::to_vec(&vec![known.clone()]).unwrap()).unwrap();
        // orphan with meta: a take that was recording during a power cut
        raw(&spool.join("orphan.raw"), 480, 2);
        fs::write(spool.join("orphan.json"), serde_json::to_vec(&meta(&["L", "R"], vec![dest("/x")])).unwrap()).unwrap();
        // orphan without meta (can't be delivered, left alone) and unrelated files
        raw(&spool.join("nometa.raw"), 1, 1);
        fs::write(spool.join("notes.txt"), b"").unwrap();
        let jobs = Jobs::load(d.path().to_path_buf(), Box::new(|| {}));
        let l = jobs.list.lock();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].tasks[0].state, TaskState::Pending);
        assert_eq!(l[0].tasks[0].next_try, 0);
        assert!(l[1].recovered);
        assert_eq!(l[1].frames, 480);
        assert_eq!(l[1].note, "Recovered after unexpected shutdown");
    }

    #[test]
    fn broken_jobs_file_is_kept_and_spool_still_recovered() {
        let d = TempDir::new("jobs");
        fs::write(d.join("jobs.json"), b"[{ nope").unwrap();
        raw(&d.join("spool/x.raw"), 10, 1);
        fs::write(d.join("spool/x.json"), serde_json::to_vec(&meta(&["a"], vec![dest("/x")])).unwrap()).unwrap();
        let jobs = Jobs::load(d.path().to_path_buf(), Box::new(|| {}));
        assert_eq!(jobs.list.lock().len(), 1);
        assert_eq!(fs::read(d.join("jobs.json.broken")).unwrap(), b"[{ nope");
        let saved: Vec<Job> = serde_json::from_slice(&fs::read(d.join("jobs.json")).unwrap()).unwrap();
        assert_eq!(saved.len(), 1);
    }

    #[test]
    fn persist_failure_is_not_fatal() {
        let d = TempDir::new("jobs");
        fs::write(d.join("file"), b"").unwrap();
        let jobs = Jobs::load(d.join("file"), Box::new(|| {}));
        jobs.persist();
        assert!(jobs.list.lock().is_empty());
    }

    #[test]
    fn worker_threads_deliver_in_background() {
        let d = TempDir::new("jobs");
        let out = TempDir::new("jobs");
        let jobs = Jobs::start(d.path().to_path_buf(), Box::new(|| {}), 0);
        let r = jobs.spool_dir().join("bg.raw");
        raw(&r, 100, 1);
        jobs.push(Job::new(r, meta(&["a"], vec![dest(out.str())]), 100));
        assert!(wait_for(10, || jobs.list.lock()[0].done()));
    }
}
