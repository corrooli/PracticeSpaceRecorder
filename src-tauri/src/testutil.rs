//! Helpers shared by the unit tests.

use crate::audio::{push_input, Backend, DeviceInfo, Input};
use crate::config::Preset;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// A fresh, empty folder under the system temp dir. Removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("psr-test-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.0.join(p)
    }
    pub fn str(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Poll `f` until it returns true or `secs` pass.
pub fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    f()
}

/// A sound card in a box. The test keeps a handle and pushes samples.
#[derive(Default)]
pub struct FakeState {
    pub devices: Vec<DeviceInfo>,
    pub fail: Option<String>,
    pub channels: usize,
    pub rate: u32,
    pub name: String,
    pub ring: usize,
    pub prod: Option<rtrb::Producer<f32>>,
    pub failed: Arc<AtomicBool>,
    pub dropped: Arc<AtomicU64>,
    pub opens: usize,
}

#[derive(Clone)]
pub struct Fake(pub Arc<Mutex<FakeState>>);

impl Fake {
    pub fn new(channels: usize, rate: u32) -> Fake {
        Fake(Arc::new(Mutex::new(FakeState { channels, rate, name: "Fake".into(), ring: 1 << 20, ..Default::default() })))
    }
    /// Push interleaved frames as the audio callback would.
    pub fn feed(&self, data: &[f32]) {
        let mut st = self.0.lock();
        let dropped = st.dropped.clone();
        push_input(st.prod.as_mut().expect("stream open"), data, &dropped);
    }
    pub fn opens(&self) -> usize {
        self.0.lock().opens
    }
}

impl Backend for Fake {
    fn devices(&self) -> Vec<DeviceInfo> {
        self.0.lock().devices.clone()
    }
    fn open(&self, _: &Preset) -> Result<Input, String> {
        let mut st = self.0.lock();
        st.opens += 1;
        if let Some(e) = &st.fail {
            return Err(e.clone());
        }
        let (prod, cons) = rtrb::RingBuffer::new(st.ring);
        st.prod = Some(prod);
        st.failed = Arc::new(AtomicBool::new(false));
        st.dropped = Arc::new(AtomicU64::new(0));
        Ok(Input { guard: Box::new(()), cons, failed: st.failed.clone(), dropped: st.dropped.clone(), channels: st.channels, rate: st.rate, device: st.name.clone() })
    }
}

