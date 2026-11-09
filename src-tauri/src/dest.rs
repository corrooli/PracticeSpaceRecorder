//! Destination health + auto-mount. Checks run on their own threads with a
//! timeout: a dead SMB mount can block `stat()` for ages, and the UI must not care.

use crate::config::{DestKind, Destination};

pub const UNSUPPORTED: &str = "This destination needs a newer version of the app";
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
        let m = DestMonitor::new(notify);
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

    /// A monitor without the 4 s polling thread. `watch` still checks once.
    pub fn new(notify: Box<dyn Fn() + Send + Sync>) -> Arc<DestMonitor> {
        Arc::new(DestMonitor { entries: Mutex::new(HashMap::new()), order: Mutex::new(vec![]), notify })
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
        let res = probe_dest(&d);
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
                        Ok(()) => match probe_dest(&d) {
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
        let resolved = locate(&d).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        self.set(&k, |e| {
            e.status.state = state;
            e.status.message = msg;
            e.status.free_bytes = free;
            if !resolved.is_empty() {
                e.status.path = resolved;
            }
            e.busy = false;
        });
    }
}

fn key(d: &Destination) -> String {
    format!("{}|{}|{}|{}", d.id, d.path, d.smb_url, d.mount_cmd)
}

fn label(d: &Destination) -> String {
    d.display_name()
}

fn probe_dest(d: &Destination) -> Result<u64, String> {
    let p = locate(d)?;
    probe_with_timeout(&p.to_string_lossy(), Duration::from_secs(6))
}

/// The folder takes go to. An explicit folder wins; otherwise it is found on the
/// mounted share from the share URL (`smb://me@nas/Music/Band` -> `/Volumes/Music/Band`).
pub fn locate(d: &Destination) -> Result<PathBuf, String> {
    match d.kind {
        DestKind::Unsupported => return Err(UNSUPPORTED.into()),
        DestKind::Folder if !d.path.trim().is_empty() => return Ok(PathBuf::from(d.path.trim())),
        _ => {}
    }
    if d.smb_url.trim().is_empty() {
        return Err("No folder set".into());
    }
    let u = ShareUrl::parse(&d.smb_url).ok_or("Share URL must look like smb://user@server/share/folder")?;
    mounted_share(&u).map(|root| root.join(&u.sub)).ok_or_else(|| "Share not connected".into())
}

/// Folder for a delivery: locate it, mounting the share once if it is missing.
/// A missing last folder is created, but never inside a bare mount root.
pub fn reach(d: &Destination) -> Result<PathBuf, String> {
    if d.kind == DestKind::Unsupported {
        return Err(UNSUPPORTED.into());
    }
    let ready = |p: &Path| -> Result<bool, String> {
        if p.is_dir() {
            return Ok(true);
        }
        if p.parent().is_some_and(|pp| pp.is_dir() && !is_mount_root(pp)) {
            std::fs::create_dir_all(p).map_err(|e| format!("Cannot create folder: {e}"))?;
            return Ok(true);
        }
        Ok(false)
    };
    if let Ok(p) = locate(d) {
        if ready(&p)? {
            return Ok(p);
        }
    }
    let can_mount = !d.smb_url.trim().is_empty() || !d.mount_cmd.trim().is_empty();
    if can_mount && mount(d).is_ok() {
        if let Ok(p) = locate(d) {
            if ready(&p)? {
                return Ok(p);
            }
        }
    }
    Err("Destination not reachable".into())
}

/// Result of the TEST button: where takes would land and how much fits there.
#[derive(Debug, PartialEq)]
pub struct TestResult {
    pub path: PathBuf,
    pub free_bytes: u64,
}

/// Really write, sync, read back and delete a small file where takes would go,
/// mounting the share first if needed. Gives up after `timeout` (dead shares hang).
pub fn test_write(d: &Destination, timeout: Duration) -> Result<TestResult, String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let d = d.clone();
    std::thread::spawn(move || {
        let _ = tx.send(test_write_now(&d));
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| Err("Share not responding".into()))
}

fn test_write_now(d: &Destination) -> Result<TestResult, String> {
    use std::io::Write;
    let path = reach(d)?;
    let f = path.join(format!(".psr-test-{}", crate::config::new_id("")));
    let data: Vec<u8> = (0..256 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
    let res = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&f)?;
        file.write_all(&data)?;
        crate::config::sync_file(&file)?;
        drop(file);
        if std::fs::read(&f)? != data {
            return Err(std::io::Error::other("file read back differently"));
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&f);
    res.map_err(|e| format!("Not writable: {e}"))?;
    let free_bytes = fs2::available_space(&path).unwrap_or(0);
    Ok(TestResult { path, free_bytes })
}

#[derive(Debug, PartialEq)]
pub struct ShareUrl {
    pub host: String,
    pub share: String,
    /// Folder inside the share.
    pub sub: PathBuf,
}

impl ShareUrl {
    pub fn parse(url: &str) -> Option<ShareUrl> {
        let (scheme, rest) = url.trim().split_once("://")?;
        if !matches!(scheme.to_ascii_lowercase().as_str(), "smb" | "cifs" | "afp" | "nfs") {
            return None;
        }
        let (auth, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
        let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
        let mut parts = path.split('/').filter(|p| !p.is_empty()).map(pct_decode);
        let share = parts.next()?;
        if host.is_empty() {
            return None;
        }
        let sub: PathBuf = parts.filter(|p| p != ".." && p != ".").collect();
        Some(ShareUrl { host, share, sub })
    }

    fn matches(&self, host: &str, share: &str) -> bool {
        host.eq_ignore_ascii_case(&self.host) && pct_decode(share).eq_ignore_ascii_case(&self.share)
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount point of a share, if this machine has it mounted.
fn mounted_share(u: &ShareUrl) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/sbin/mount").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        find_in_bsd_mounts(&String::from_utf8_lossy(&out.stdout), u)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let proc = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        find_in_proc_mounts(&proc, u).or_else(|| {
            let users = std::fs::read_dir("/run/user").ok()?;
            users.flatten().find_map(|e| find_in_gvfs(&e.path().join("gvfs"), u))
        })
    }
}

/// macOS `mount` output: `//me@nas.local/Music on /Volumes/Music (smbfs, ...)`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn find_in_bsd_mounts(table: &str, u: &ShareUrl) -> Option<PathBuf> {
    table.lines().find_map(|line| {
        let (src, rest) = line.split_once(" on ")?;
        let (mp, _) = rest.rsplit_once(" (")?;
        let (host, share) = unc(src)?;
        u.matches(host, share).then(|| PathBuf::from(mp))
    })
}

/// Linux `/proc/mounts`: `//nas/music /mnt/nas cifs ...`, spaces as `\040`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn find_in_proc_mounts(table: &str, u: &ShareUrl) -> Option<PathBuf> {
    table.lines().find_map(|line| {
        let mut f = line.split(' ');
        let (src, mp) = (f.next()?, f.next()?);
        let src = src.replace("\\040", " ");
        let (host, share) = unc(&src)?;
        u.matches(host, share).then(|| PathBuf::from(mp.replace("\\040", " ")))
    })
}

/// GVfs: `<dir>/smb-share:server=nas.local,share=music,user=me`.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn find_in_gvfs(dir: &Path, u: &ShareUrl) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        let kv = name.strip_prefix("smb-share:")?;
        let get = |k: &str| kv.split(',').find_map(|p| p.strip_prefix(k).and_then(|v| v.strip_prefix('=')));
        u.matches(get("server")?, get("share")?).then(|| e.path())
    })
}

/// `//user@host/share` -> (host, share).
fn unc(src: &str) -> Option<(&str, &str)> {
    let rest = src.strip_prefix("//")?;
    let (auth, share) = rest.split_once('/')?;
    let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    Some((host.split(':').next()?, share.trim_end_matches('/')))
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

pub fn mount(d: &Destination) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("psr-dest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn probe_existing_folder_reports_space_and_cleans_up() {
        let d = tmp("ok");
        let free = probe(d.to_str().unwrap()).unwrap();
        assert!(free > 0);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "probe file left behind");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn probe_creates_missing_leaf() {
        let d = tmp("leaf");
        let leaf = d.join("takes");
        probe(leaf.to_str().unwrap()).unwrap();
        assert!(leaf.is_dir());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn probe_refuses_missing_parent() {
        let d = tmp("deep");
        let deep = d.join("a/b");
        assert_eq!(probe(deep.to_str().unwrap()).unwrap_err(), "Folder not found");
        assert!(!d.join("a").exists());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn probe_empty_path() {
        assert_eq!(probe("").unwrap_err(), "No folder set");
        assert_eq!(probe("   ").unwrap_err(), "No folder set");
    }

    #[test]
    fn probe_with_timeout_passes_result_through() {
        let d = tmp("timeout");
        assert!(probe_with_timeout(d.to_str().unwrap(), Duration::from_secs(5)).is_ok());
        assert!(probe_with_timeout("", Duration::from_secs(5)).is_err());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn mount_roots() {
        assert!(is_mount_root(Path::new("/Volumes")));
        assert!(is_mount_root(Path::new("/mnt")));
        assert!(is_mount_root(Path::new("/media")));
        assert!(is_mount_root(Path::new("/media/usb")));
        assert!(is_mount_root(Path::new("/run/user/1000/gvfs")));
        assert!(!is_mount_root(Path::new("/Volumes/NAS")));
        assert!(!is_mount_root(Path::new("/home/me")));
    }

    #[test]
    fn label_falls_back_to_path() {
        let mut d = Destination { path: "/x/y".into(), label: String::new(), ..Default::default() };
        assert_eq!(label(&d), "/x/y");
        d.label = "  ".into();
        assert_eq!(label(&d), "/x/y");
        d.label = "NAS".into();
        assert_eq!(label(&d), "NAS");
    }

    #[test]
    fn key_changes_with_mount_settings() {
        let a = Destination { id: "1".into(), path: "/x".into(), ..Default::default() };
        let mut b = a.clone();
        assert_eq!(key(&a), key(&b));
        b.smb_url = "smb://nas/share".into();
        assert_ne!(key(&a), key(&b));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_outcomes() {
        let sh = |s: &str| {
            let mut c = Command::new("sh");
            c.arg("-c").arg(s);
            c
        };
        assert!(run_with_timeout(sh("true"), Duration::from_secs(5)).is_ok());
        assert_eq!(run_with_timeout(sh("false"), Duration::from_secs(5)).unwrap_err(), "Mount failed");
        assert_eq!(run_with_timeout(sh("echo nope >&2; exit 1"), Duration::from_secs(5)).unwrap_err(), "Mount failed: nope");
        assert_eq!(run_with_timeout(sh("sleep 5"), Duration::from_millis(300)).unwrap_err(), "Mount timed out");
    }

    #[test]
    fn mount_rejects_unknown_url_scheme() {
        let d = Destination { smb_url: "http://nas/share".into(), ..Default::default() };
        assert_eq!(mount(&d).unwrap_err(), "Share URL must start with smb://");
    }

    // ---- monitor: the auto-mount state machine

    fn monitor() -> (Arc<DestMonitor>, Arc<std::sync::atomic::AtomicUsize>) {
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        (DestMonitor::new(Box::new(move || {
            n2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })), n)
    }

    /// Register without the background check `watch` kicks off.
    fn add(m: &Arc<DestMonitor>, d: &Destination) {
        m.entries.lock().insert(key(d), Entry {
            status: DestStatus { id: d.id.clone(), label: label(d), path: d.path.clone(), state: DestState::Checking, message: "Checking".into(), free_bytes: 0 },
            busy: true,
            last_mount: None,
        });
        m.order.lock().push(d.clone());
    }

    fn check(m: &Arc<DestMonitor>, d: &Destination) -> DestStatus {
        m.clone().check(d.clone(), key(d));
        m.snapshot().into_iter().find(|s| s.id == d.id).unwrap()
    }

    fn share(tmp: &TempDir, mount_cmd: &str) -> Destination {
        Destination { id: "nas".into(), label: "NAS".into(), path: tmp.join("share/takes").to_string_lossy().into(), mount_cmd: mount_cmd.into(), ..Default::default() }
    }

    #[test]
    fn local_folder_is_ready() {
        let t = TempDir::new("mon");
        let (m, n) = monitor();
        let d = Destination { id: "l".into(), path: t.str(), ..Default::default() };
        add(&m, &d);
        let s = check(&m, &d);
        assert_eq!((s.state, s.message.as_str()), (DestState::Ready, "Ready"));
        assert!(s.free_bytes > 0);
        assert!(n.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        assert!(!m.entries.lock()[&key(&d)].busy);
    }

    #[test]
    fn missing_folder_without_mount_info() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let d = share(&t, "");
        add(&m, &d);
        let s = check(&m, &d);
        assert_eq!((s.state, s.message.as_str()), (DestState::Error, "Folder not found"));
    }

    #[test]
    fn mount_command_brings_share_online() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let d = share(&t, &format!("mkdir -p '{}'", t.join("share").display()));
        add(&m, &d);
        let s = check(&m, &d);
        assert_eq!((s.state, s.message.as_str()), (DestState::Ready, "Ready"));
        assert!(t.join("share/takes").is_dir());
        assert!(m.entries.lock()[&key(&d)].last_mount.is_some());
    }

    #[test]
    fn mount_that_does_not_produce_the_folder() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let d = share(&t, "true");
        add(&m, &d);
        let s = check(&m, &d);
        assert_eq!((s.state, s.message.as_str()), (DestState::Error, "Mounted, but Folder not found"));
    }

    #[test]
    fn failed_mount_is_not_retried_for_30s() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let d = share(&t, "echo 'bad password' >&2; exit 2");
        add(&m, &d);
        let s = check(&m, &d);
        assert_eq!((s.state, s.message.as_str()), (DestState::Error, "Mount failed: bad password"));
        let s = check(&m, &d);
        assert_eq!(s.message, "Folder not found (retrying)", "no mount storm");
        m.entries.lock().get_mut(&key(&d)).unwrap().last_mount = Some(Instant::now() - Duration::from_secs(31));
        assert_eq!(check(&m, &d).message, "Mount failed: bad password");
    }

    #[test]
    fn bad_share_url() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let mut d = share(&t, "");
        d.smb_url = "ftp://nas/x".into();
        add(&m, &d);
        assert_eq!(check(&m, &d).message, "Share URL must start with smb://");
    }

    #[test]
    fn watch_tracks_enabled_destinations_in_order() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let a = Destination { id: "a".into(), path: t.join("a").to_string_lossy().into(), ..Default::default() };
        let b = Destination { id: "b".into(), path: t.join("b").to_string_lossy().into(), ..Default::default() };
        let mut off = b.clone();
        off.id = "off".into();
        off.enabled = false;
        m.watch(&[b.clone(), off, a.clone()]);
        assert!(crate::testutil::wait_for(10, || m.snapshot().iter().all(|s| s.state == DestState::Ready)));
        let ids: Vec<_> = m.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["b", "a"]);
        m.watch(std::slice::from_ref(&a));
        assert_eq!(m.snapshot().len(), 1);
        assert_eq!(m.entries.lock().len(), 1, "dropped destinations are forgotten");
        // results for a forgotten destination are ignored
        m.clone().check(b.clone(), key(&b));
        assert_eq!(m.snapshot().len(), 1);
    }

    #[test]
    fn tick_skips_busy_entries() {
        let t = TempDir::new("mon");
        let (m, _) = monitor();
        let d = Destination { id: "x".into(), path: t.str(), ..Default::default() };
        add(&m, &d); // busy = true
        m.tick();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(m.snapshot()[0].state, DestState::Checking, "busy entry not checked twice");
        m.entries.lock().get_mut(&key(&d)).unwrap().busy = false;
        m.tick();
        assert!(crate::testutil::wait_for(10, || m.snapshot()[0].state == DestState::Ready));
    }

    #[test]
    fn background_thread_polls() {
        let t = TempDir::new("mon");
        let m = DestMonitor::start(Box::new(|| {}));
        m.watch(&[Destination { id: "x".into(), path: t.str(), ..Default::default() }]);
        assert!(crate::testutil::wait_for(10, || m.snapshot()[0].state == DestState::Ready));
    }

    // ---- share URL -> folder

    fn su(host: &str, share: &str, sub: &str) -> ShareUrl {
        ShareUrl { host: host.into(), share: share.into(), sub: PathBuf::from(sub) }
    }

    #[test]
    fn parses_share_urls() {
        assert_eq!(ShareUrl::parse("smb://oliver@nasssd.internal/Oliver/Test/"), Some(su("nasssd.internal", "Oliver", "Test")));
        assert_eq!(ShareUrl::parse(" SMB://NAS.local:445/Music "), Some(su("nas.local", "Music", "")));
        assert_eq!(ShareUrl::parse("smb://me:pw@nas/My%20Music/Band/2026"), Some(su("nas", "My Music", "Band/2026")));
        assert_eq!(ShareUrl::parse("cifs://nas/x/../../etc"), Some(su("nas", "x", "etc")), "no escaping the share");
        assert_eq!(ShareUrl::parse("afp://nas/x").unwrap().share, "x");
        assert_eq!(ShareUrl::parse("nfs://nas/x").unwrap().share, "x");
        assert_eq!(ShareUrl::parse("smb://nas"), None, "needs a share");
        assert_eq!(ShareUrl::parse("smb:///share"), None, "needs a host");
        assert_eq!(ShareUrl::parse("http://nas/x"), None);
        assert_eq!(ShareUrl::parse("/Volumes/x"), None);
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(pct_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz%4"), "%zz%4");
        assert_eq!(pct_decode("M%C3%BCsik"), "Müsik");
        assert_eq!(pct_decode("ü%"), "ü%");
    }

    #[test]
    fn finds_share_in_macos_mount_table() {
        let table = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                     //oliver@nasssd.internal/Oliver on /Volumes/Oliver (smbfs, nodev, nosuid, mounted by oliver)\n\
                     //GUEST:@nas._smb._tcp.local/My%20Music on /Volumes/My Music 1 (smbfs, nodev)\n";
        let u = ShareUrl::parse("smb://oliver@nasssd.internal/Oliver/Test/").unwrap();
        assert_eq!(find_in_bsd_mounts(table, &u), Some(PathBuf::from("/Volumes/Oliver")));
        let u = ShareUrl::parse("smb://nas._smb._tcp.local/my music").unwrap();
        assert_eq!(find_in_bsd_mounts(table, &u), Some(PathBuf::from("/Volumes/My Music 1")));
        let u = ShareUrl::parse("smb://other/Oliver").unwrap();
        assert_eq!(find_in_bsd_mounts(table, &u), None);
    }

    #[test]
    fn finds_share_in_linux_mounts_and_gvfs() {
        let table = "sysfs /sys sysfs rw 0 0\n//nas/music /mnt/nas\\040drive cifs rw 0 0\n";
        let u = ShareUrl::parse("smb://nas/Music/Band").unwrap();
        assert_eq!(find_in_proc_mounts(table, &u), Some(PathBuf::from("/mnt/nas drive")));
        assert_eq!(find_in_proc_mounts(table, &ShareUrl::parse("smb://nas/other").unwrap()), None);
        let t = TempDir::new("gvfs");
        std::fs::create_dir(t.join("smb-share:server=nas.local,share=music,user=me")).unwrap();
        std::fs::create_dir(t.join("sftp:host=x")).unwrap();
        let u = ShareUrl::parse("smb://me@NAS.local/Music").unwrap();
        assert_eq!(find_in_gvfs(t.path(), &u), Some(t.join("smb-share:server=nas.local,share=music,user=me")));
        assert_eq!(find_in_gvfs(t.path(), &ShareUrl::parse("smb://nas.local/video").unwrap()), None);
        assert_eq!(find_in_gvfs(&t.join("missing"), &u), None);
        assert_eq!(unc("nas/x"), None);
        assert_eq!(unc("//nas"), None);
    }

    #[test]
    fn kinds_decide_where_takes_go() {
        let share = Destination { kind: DestKind::Share, path: "/stale/folder".into(), smb_url: "smb://no-such-host.invalid/x".into(), ..Default::default() };
        assert_eq!(locate(&share).unwrap_err(), "Share not connected", "a share ignores the folder field");
        let newer = Destination { kind: DestKind::Unsupported, path: "/tmp".into(), ..Default::default() };
        assert_eq!(locate(&newer).unwrap_err(), UNSUPPORTED);
        assert_eq!(reach(&newer).unwrap_err(), UNSUPPORTED);
        assert_eq!(test_write(&newer, Duration::from_secs(5)).unwrap_err(), UNSUPPORTED);
        let (m, _) = monitor();
        add(&m, &newer);
        assert_eq!(check(&m, &newer).message, UNSUPPORTED);
    }

    #[test]
    fn locate_prefers_the_explicit_folder() {
        let d = Destination { path: " /x/y ".into(), smb_url: "smb://nas/s".into(), ..Default::default() };
        assert_eq!(locate(&d), Ok(PathBuf::from("/x/y")));
        let d = Destination { path: "".into(), smb_url: "".into(), ..Default::default() };
        assert_eq!(locate(&d).unwrap_err(), "No folder set");
        let d = Destination { path: "".into(), smb_url: "ftp://x".into(), ..Default::default() };
        assert!(locate(&d).unwrap_err().starts_with("Share URL must look like"));
        let d = Destination { path: "".into(), smb_url: "smb://no-such-host.invalid/none".into(), ..Default::default() };
        assert_eq!(locate(&d).unwrap_err(), "Share not connected");
    }

    #[test]
    fn reach_mounts_when_needed() {
        let t = TempDir::new("reach");
        let d = Destination { path: t.str(), ..Default::default() };
        assert_eq!(reach(&d).unwrap(), t.path());
        let gone = Destination { path: t.join("a/b").to_string_lossy().into(), ..Default::default() };
        assert_eq!(reach(&gone).unwrap_err(), "Destination not reachable");
        // mount command makes the share appear; the leaf folder is created
        let m = Destination { path: t.join("share/takes").to_string_lossy().into(), mount_cmd: format!("mkdir -p '{}'", t.join("share").display()), ..Default::default() };
        assert_eq!(reach(&m).unwrap(), t.join("share/takes"));
        assert!(t.join("share/takes").is_dir());
        let bad = Destination { path: t.join("x/y").to_string_lossy().into(), mount_cmd: "true".into(), ..Default::default() };
        assert_eq!(reach(&bad).unwrap_err(), "Destination not reachable");
    }

    #[test]
    fn display_names() {
        let mut d = Destination { label: "".into(), path: "/p".into(), smb_url: "smb://nas/s".into(), ..Default::default() };
        assert_eq!(d.display_name(), "/p");
        d.path.clear();
        assert_eq!(d.display_name(), "smb://nas/s");
        assert!(d.usable());
        d.smb_url.clear();
        assert!(!d.usable());
    }

    #[test]
    fn test_button_really_writes() {
        let t = TempDir::new("tbtn");
        let d = Destination { path: t.join("takes").to_string_lossy().into(), ..Default::default() };
        let r = test_write(&d, Duration::from_secs(10)).unwrap();
        assert_eq!(r.path, t.join("takes"));
        assert!(r.free_bytes > 0);
        assert_eq!(std::fs::read_dir(t.join("takes")).unwrap().count(), 0, "test file removed");
        let gone = Destination { path: t.join("a/b").to_string_lossy().into(), ..Default::default() };
        assert_eq!(test_write(&gone, Duration::from_secs(10)).unwrap_err(), "Destination not reachable");
        let hang = Destination { path: t.join("x/y").to_string_lossy().into(), mount_cmd: "sleep 5".into(), ..Default::default() };
        assert_eq!(test_write(&hang, Duration::from_millis(200)).unwrap_err(), "Share not responding");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir(t.join("ro")).unwrap();
            std::fs::set_permissions(t.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();
            let ro = Destination { path: t.join("ro").to_string_lossy().into(), ..Default::default() };
            let e = test_write(&ro, Duration::from_secs(10));
            std::fs::set_permissions(t.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(e.unwrap_err().starts_with("Not writable"));
        }
    }

    /// Against a real share: `PSR_NAS_URL=smb://me@nas/share/dir cargo test live_nas -- --ignored`
    #[test]
    #[ignore]
    fn live_nas_delivery() {
        let url = std::env::var("PSR_NAS_URL").expect("PSR_NAS_URL");
        let d = Destination { label: "NAS".into(), path: String::new(), smb_url: url, ..Default::default() };
        let root = reach(&d).expect("share reachable");
        eprintln!("share folder: {}", root.display());
        let spool = TempDir::new("live");
        let raw = spool.join("t.raw");
        std::fs::write(&raw, vec![0u8; 48000 * 2 * 4]).unwrap();
        let meta = crate::jobs::TakeMeta {
            take_name: "psr-live-test".into(), rel_path: PathBuf::from("psr-live-test"), preset_name: "t".into(), sample_rate: 48000,
            track_names: vec!["L".into(), "R".into()], format: crate::config::Format::Wav24, split_tracks: false, started_at: 0, destinations: vec![d.clone()],
        };
        let job = crate::jobs::Job::new(raw, meta, 48000);
        let out = crate::jobs::deliver(&job, &d, &mut |_| {}).expect("delivered");
        eprintln!("wrote {out}");
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 104 + 48000 * 6);
        std::fs::remove_file(out).unwrap();
    }
}
