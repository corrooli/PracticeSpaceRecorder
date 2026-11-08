//! Destination health + auto-mount. Checks run on their own threads with a
//! timeout: a dead SMB mount can block `stat()` for ages, and the UI must not care.

use crate::config::Destination;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DestState {
    Checking,
    Mounting,
    Ready,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct DestStatus {
    pub id: String,
    pub label: String,
    pub path: String,
    pub state: DestState,
    pub message: String,
    pub free_bytes: u64,
}

struct Entry {
    status: DestStatus,
    busy: bool,
    last_mount: Option<Instant>,
}

pub struct DestMonitor {
    entries: Mutex<HashMap<String, Entry>>,
    order: Mutex<Vec<Destination>>,
    notify: Box<dyn Fn() + Send + Sync>,
}

impl DestMonitor {
    pub fn start(notify: Box<dyn Fn() + Send + Sync>) -> Arc<DestMonitor> {
        let m = Arc::new(DestMonitor { entries: Mutex::new(HashMap::new()), order: Mutex::new(vec![]), notify });
        let m2 = m.clone();
        std::thread::Builder::new()
            .name("psr-dest".into())
            .spawn(move || loop {
                m2.tick();
                std::thread::sleep(Duration::from_secs(4));
            })
            .expect("dest thread");
        m
    }

    /// Set the destinations to watch (active preset).
    pub fn watch(self: &Arc<Self>, dests: &[Destination]) {
        let dests: Vec<Destination> = dests.iter().filter(|d| d.enabled).cloned().collect();
        {
            let mut e = self.entries.lock();
            e.retain(|k, _| dests.iter().any(|d| key(d) == *k));
            for d in &dests {
                e.entry(key(d)).or_insert_with(|| Entry {
                    status: DestStatus { id: d.id.clone(), label: label(d), path: d.path.clone(), state: DestState::Checking, message: "Checking".into(), free_bytes: 0 },
                    busy: false,
                    last_mount: None,
                });
            }
        }
        *self.order.lock() = dests;
        let me = self.clone();
        std::thread::spawn(move || me.tick());
        (self.notify)();
    }

    pub fn snapshot(&self) -> Vec<DestStatus> {
        let e = self.entries.lock();
        self.order.lock().iter().filter_map(|d| e.get(&key(d)).map(|x| x.status.clone())).collect()
    }

    fn tick(self: &Arc<Self>) {
        let dests = self.order.lock().clone();
        for d in dests {
            let k = key(&d);
            {
                let mut e = self.entries.lock();
                let Some(en) = e.get_mut(&k) else { continue };
                if en.busy {
                    continue;
                }
                en.busy = true;
            }
            let me = self.clone();
            std::thread::spawn(move || me.check(d, k));
        }
    }

    fn set(&self, k: &str, f: impl FnOnce(&mut Entry)) {
        let changed = {
            let mut e = self.entries.lock();
            match e.get_mut(k) {
                Some(en) => {
                    let before = (en.status.state.clone(), en.status.message.clone());
                    f(en);
                    before != (en.status.state.clone(), en.status.message.clone())
                }
                None => false,
            }
        };
        if changed {
            (self.notify)();
        }
    }

    fn check(self: Arc<Self>, d: Destination, k: String) {
        let res = probe_with_timeout(&d.path, Duration::from_secs(6));
        let (state, msg, free) = match res {
            Ok(free) => (DestState::Ready, "Ready".to_string(), free),
            Err(err) => {
                let can_mount = !d.smb_url.trim().is_empty() || !d.mount_cmd.trim().is_empty();
                let due = self.entries.lock().get(&k).map(|e| e.last_mount.map(|t| t.elapsed() > Duration::from_secs(30)).unwrap_or(true)).unwrap_or(false);
                if can_mount && due {
                    self.set(&k, |e| {
                        e.last_mount = Some(Instant::now());
                        e.status.state = DestState::Mounting;
                        e.status.message = "Connecting share".into();
                    });
                    match mount(&d) {
                        Ok(()) => match probe_with_timeout(&d.path, Duration::from_secs(6)) {
                            Ok(free) => (DestState::Ready, "Ready".into(), free),
                            Err(e) => (DestState::Error, format!("Mounted, but {e}"), 0),
                        },
                        Err(e) => (DestState::Error, e, 0),
                    }
                } else if can_mount {
                    (DestState::Error, format!("{err} (retrying)"), 0)
                } else {
                    (DestState::Error, err, 0)
                }
            }
        };
        self.set(&k, |e| {
            e.status.state = state;
            e.status.message = msg;
            e.status.free_bytes = free;
            e.busy = false;
        });
    }
}

fn key(d: &Destination) -> String {
    format!("{}|{}|{}|{}", d.id, d.path, d.smb_url, d.mount_cmd)
}

fn label(d: &Destination) -> String {
    if d.label.trim().is_empty() { d.path.clone() } else { d.label.clone() }
}

fn probe_with_timeout(path: &str, timeout: Duration) -> Result<u64, String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let p = path.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(probe(&p));
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| Err("Share not responding".into()))
}

/// Folder exists (or can be created inside an existing parent), is writable, has space.
fn probe(path: &str) -> Result<u64, String> {
    let p = Path::new(path.trim());
    if path.trim().is_empty() {
        return Err("No folder set".into());
    }
    if !p.is_dir() {
        // allow a not-yet-existing leaf folder if its parent is there (local disks),
        // but never create mount points.
        let parent_ok = p.parent().map(|pp| pp.is_dir() && !is_mount_root(pp)).unwrap_or(false);
        if parent_ok {
            std::fs::create_dir_all(p).map_err(|e| format!("Cannot create folder: {e}"))?;
        } else {
            return Err("Folder not found".into());
        }
    }
    let probe = p.join(format!(".psr-probe-{}", std::process::id()));
    std::fs::write(&probe, b"ok").map_err(|e| format!("Not writable: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    fs2::available_space(p).map_err(|e| e.to_string())
}

/// `/Volumes` or `/mnt`-style roots: creating folders there fakes a share.
fn is_mount_root(p: &Path) -> bool {
    matches!(p.to_str(), Some("/Volumes") | Some("/mnt") | Some("/media")) || p.starts_with("/run/user") || p.starts_with("/media")
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<(), String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("Cannot run mount: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(_)) => {
                let mut s = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut s);
                }
                let s = s.trim().lines().last().unwrap_or("").to_string();
                return Err(if s.is_empty() { "Mount failed".into() } else { format!("Mount failed: {s}") });
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                return Err("Mount timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn mount(d: &Destination) -> Result<(), String> {
    if !d.mount_cmd.trim().is_empty() {
        let mut c = Command::new("sh");
        c.arg("-c").arg(d.mount_cmd.trim());
        return run_with_timeout(c, Duration::from_secs(45));
    }
    let url = d.smb_url.trim();
    if !url.starts_with("smb://") && !url.starts_with("cifs://") && !url.starts_with("afp://") && !url.starts_with("nfs://") {
        return Err("Share URL must start with smb://".into());
    }
    #[cfg(target_os = "macos")]
    {
        // Finder's mount: uses Keychain credentials, shows the login dialog once.
        let esc = url.replace('\\', "\\\\").replace('"', "\\\"");
        let mut c = Command::new("osascript");
        c.arg("-e").arg(format!("mount volume \"{esc}\""));
        run_with_timeout(c, Duration::from_secs(45))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // GVfs mount (credentials from the keyring). Path is usually
        // /run/user/<uid>/gvfs/smb-share:server=<host>,share=<share>
        let mut c = Command::new("gio");
        c.arg("mount").arg(url);
        run_with_timeout(c, Duration::from_secs(45))
    }
}
