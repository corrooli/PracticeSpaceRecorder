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


/// Dev tools for tuning the auto master. Ignored by default, kept here so they stay out of coverage.
#[cfg(test)]
mod master_tools {
    use crate::master::tests::{measure, run, sine, true_peak};
    use crate::master::{db, gain_db, lin};
    use std::path::Path;

    /// `cargo test --release bench_master -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_master() {
        let x = sine(48_000, 2, 60.0, 220.0, 0.05);
        let t = std::time::Instant::now();
        let g = gain_db(&measure(48_000, 2, &x));
        let a = t.elapsed();
        let y = run(48_000, 2, g, &x);
        println!("1 min stereo: analysis {:?}, master {:?} ({} frames)", a, t.elapsed() - a, y.len() / 2);
    }

    /// Master a real recording to listen to:
    /// `PSR_WAV=in.wav PSR_OUT=out.wav cargo test --release master_file -- --ignored --nocapture`
    /// (16 or 24 bit PCM WAV in, 24 bit WAV out).
    #[test]
    #[ignore]
    fn master_file() {
        let (Ok(inp), Ok(outp)) = (std::env::var("PSR_WAV"), std::env::var("PSR_OUT")) else { panic!("set PSR_WAV and PSR_OUT") };
        let b = std::fs::read(&inp).unwrap();
        let find = |id: &[u8]| b.windows(4).position(|w| w == id).unwrap();
        let fmt = find(b"fmt ") + 8;
        let ch = u16::from_le_bytes([b[fmt + 2], b[fmt + 3]]) as usize;
        let rate = u32::from_le_bytes(b[fmt + 4..fmt + 8].try_into().unwrap());
        let bits = u16::from_le_bytes([b[fmt + 14], b[fmt + 15]]);
        let data = find(b"data") + 8;
        let x: Vec<f32> = match bits {
            16 => b[data..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
            24 => b[data..].chunks_exact(3).map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0).collect(),
            _ => panic!("{bits} bit not supported here"),
        };
        let before = measure(rate, ch, &x);
        let g = gain_db(&before);
        let y = run(rate, ch, g, &x);
        let after = measure(rate, ch, &y);
        // how hard the limiter worked: output vs. plain gain, per 10 ms
        let lin_g = lin(g) as f32;
        let mut gr: Vec<f64> = x.chunks(480 * ch).zip(y.chunks(480 * ch)).filter_map(|(a, b)| {
            let pa = a.iter().fold(0f32, |m, v| m.max(v.abs())) * lin_g;
            let pb = b.iter().fold(0f32, |m, v| m.max(v.abs()));
            (pa > 1e-4).then(|| db(pa as f64) - db(pb as f64))
        }).collect();
        gr.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f64| gr.get(((gr.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
        println!("{inp}: {ch} ch, {rate} Hz, {:.1} s", x.len() as f64 / ch as f64 / rate as f64);
        println!("before: {:.1} LUFS, peaks {:.1} dBFS | gain {:+.1} dB | after: {:.1} LUFS, true peak {:.2} dBTP", before.lufs, before.peak_db, g, after.lufs, true_peak(&y, ch));
        println!("limiting (10 ms windows): median {:.1} dB, 90% {:.1} dB, max {:.1} dB", pct(0.5), pct(0.9), pct(1.0));
        let mut w = crate::wav::WavWriter::create(Path::new(&outp), rate, ch as u16, 24, false).unwrap();
        w.write(&y).unwrap();
        w.finalize().unwrap();
    }
}
