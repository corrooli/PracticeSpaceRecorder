//! Background delivery queue. A stopped take becomes a job; every destination
//! is an independent task (render spool -> `.part` -> rename). Tasks retry with
//! backoff forever, survive restarts (jobs.json) and never block recording.

use crate::config::{data_dir, sanitize, write_atomic, Destination, Format};
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
}

pub fn spool_dir() -> PathBuf {
    data_dir().join("spool")
}
fn jobs_file() -> PathBuf {
    data_dir().join("jobs.json")
}
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Jobs {
    pub fn start(notify: Box<dyn Fn() + Send + Sync>, workers: usize) -> Arc<Jobs> {
        let mut list: Vec<Job> = fs::read(jobs_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for j in &mut list {
            for t in &mut j.tasks {
                if t.state == TaskState::Busy {
                    t.state = TaskState::Pending;
                }
                t.next_try = 0;
            }
        }
        // spool files nobody owns: a crash or power cut mid-take. Rescue them.
        if let Ok(rd) = fs::read_dir(spool_dir()) {
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
        let jobs = Arc::new(Jobs { list: Mutex::new(list), notify });
        jobs.persist();
        for i in 0..workers.max(1) {
            let j = jobs.clone();
            std::thread::Builder::new().name(format!("psr-job-{i}")).spawn(move || j.worker()).expect("job worker");
        }
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
            let _ = write_atomic(&jobs_file(), &b);
        }
    }

    fn worker(self: Arc<Self>) {
        loop {
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
            let Some((id, ti, job)) = pick else {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            };
            (self.notify)();
            let me = self.clone();
            let id2 = id.clone();
            let result = deliver(&job, &job.tasks[ti].dest, &mut |p| {
                if let Some(j) = me.list.lock().iter_mut().find(|j| j.id == id2) {
                    j.tasks[ti].progress = p;
                }
                (me.notify)();
            });
            let finished = {
                let mut list = self.list.lock();
                let Some(j) = list.iter_mut().find(|j| j.id == id) else { continue };
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
        }
    }
}

impl Job {
    pub fn new(raw: PathBuf, meta: TakeMeta, frames: u64) -> Job {
        let tasks = meta
            .destinations
            .iter()
            .filter(|d| d.enabled && !d.path.trim().is_empty())
            .map(|d| DestTask {
                label: if d.label.trim().is_empty() { d.path.clone() } else { d.label.clone() },
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

fn deliver(job: &Job, dest: &Destination, progress: &mut dyn FnMut(f32)) -> Result<String, String> {
    let root = PathBuf::from(dest.path.trim());
    if !root.is_dir() {
        return Err("Destination not reachable".into());
    }
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

    #[test]
    fn delivers_split_and_interleaved() {
        let root = std::env::temp_dir().join(format!("psr-jobs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let raw = root.join("t.raw");
        let frames = 1000u64;
        let mut b = Vec::new();
        for i in 0..frames {
            for c in 0..3 {
                b.extend_from_slice(&((i as f32 / frames as f32) * if c == 1 { -1.0 } else { 1.0 }).to_le_bytes());
            }
        }
        fs::write(&raw, &b).unwrap();
        let dest = Destination { path: root.join("out").to_string_lossy().into(), ..Default::default() };
        fs::create_dir_all(&dest.path).unwrap();
        let mut meta = TakeMeta {
            take_name: "t".into(),
            rel_path: PathBuf::from("day/take"),
            preset_name: "p".into(),
            sample_rate: 48000,
            track_names: vec!["Kick".into(), "Snare".into(), "Gtr/L".into()],
            format: Format::Wav24,
            split_tracks: true,
            started_at: 0,
            destinations: vec![dest.clone()],
        };
        let job = Job::new(raw.clone(), meta.clone(), frames);
        let out = deliver(&job, &dest, &mut |_| {}).unwrap();
        let dir = PathBuf::from(&out);
        assert!(dir.join("01 Kick.wav").exists());
        assert!(dir.join("03 Gtr-L.wav").exists());
        assert_eq!(fs::metadata(dir.join("02 Snare.wav")).unwrap().len(), 104 + frames * 3);
        meta.split_tracks = false;
        let job = Job::new(raw, meta, frames);
        let out = deliver(&job, &dest, &mut |_| {}).unwrap();
        assert!(out.ends_with("take.wav"));
        assert_eq!(fs::metadata(&out).unwrap().len(), 104 + frames * 9);
        let out2 = deliver(&job, &dest, &mut |_| {}).unwrap();
        assert!(out2.ends_with("take (2).wav"));
        fs::remove_dir_all(root).ok();
    }
}
