//! Turning the spool into files: a reader over frame ranges of the raw spool,
//! WAV and MP3 sinks, and the stereo mixdown. Every destination renders through
//! [`render`], so processing that belongs to all of them goes in there.

use crate::config::sync_file;
use crate::wav::WavWriter;
use mp3lame_encoder::{max_required_buffer_size, Bitrate, Builder, DualPcm, Encoder, FlushNoGap, Id3Tag, MonoPcm, Quality};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::path::Path;

/// Somewhere audio goes, a block of interleaved frames at a time.
pub trait Sink {
    fn write(&mut self, interleaved: &[f32]) -> io::Result<()>;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

impl Sink for WavWriter {
    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        WavWriter::write(self, interleaved)
    }
    fn finish(self: Box<Self>) -> io::Result<()> {
        self.finalize().map(|_| ())
    }
}

/// ID3 tags for an MP3.
#[derive(Default)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: String,
    pub comment: String,
}

pub struct Mp3Writer {
    enc: Encoder,
    out: BufWriter<File>,
    buf: Vec<u8>,
    channels: usize,
    left: Vec<f32>,
    right: Vec<f32>,
}

/// LAME speaks MPEG-1 rates up to 48 kHz. Everything else is resampled to the
/// closest family (88.2k goes to 44.1k, 96k to 48k).
pub fn mp3_rate(sample_rate: u32) -> u32 {
    match sample_rate {
        32_000 | 44_100 | 48_000 => sample_rate,
        r if r % 11_025 == 0 => 44_100,
        _ => 48_000,
    }
}

fn bitrate(kbps: u32) -> Bitrate {
    match kbps {
        0..=128 => Bitrate::Kbps128,
        129..=160 => Bitrate::Kbps160,
        161..=192 => Bitrate::Kbps192,
        193..=224 => Bitrate::Kbps224,
        225..=256 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    }
}

fn lame_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("MP3 encoder: {e}"))
}

impl Mp3Writer {
    /// Constant bitrate, so players know the length without scanning the file.
    pub fn create(path: &Path, sample_rate: u32, channels: u16, kbps: u32, tags: &Tags) -> io::Result<Mp3Writer> {
        let channels = channels.clamp(1, 2);
        let mut b = Builder::new().ok_or_else(|| lame_err("init failed"))?;
        b.set_sample_rate(sample_rate).map_err(lame_err)?;
        b.set_output_sample_rate(NonZeroU32::new(mp3_rate(sample_rate))).map_err(lame_err)?;
        b.set_num_channels(channels as u8).map_err(lame_err)?;
        b.set_brate(bitrate(kbps)).map_err(lame_err)?;
        b.set_quality(Quality::NearBest).map_err(lame_err)?;
        b.set_to_write_vbr_tag(false).map_err(lame_err)?;
        b.set_id3_tag(Id3Tag {
            title: tags.title.as_bytes(),
            artist: tags.artist.as_bytes(),
            album: tags.album.as_bytes(),
            album_art: &[],
            year: tags.year.as_bytes(),
            comment: tags.comment.as_bytes(),
        })
        .map_err(|e| lame_err(format!("{e:?}")))?;
        let enc = b.build().map_err(lame_err)?;
        let out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        Ok(Mp3Writer { enc, out, buf: Vec::new(), channels: channels as usize, left: Vec::new(), right: Vec::new() })
    }
}

impl Sink for Mp3Writer {
    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        for chunk in interleaved.chunks(4096 * self.channels) {
            let frames = chunk.len() / self.channels;
            self.buf.clear();
            self.buf.reserve(max_required_buffer_size(frames));
            let r = if self.channels == 1 {
                self.enc.encode_to_vec(MonoPcm(chunk), &mut self.buf)
            } else {
                self.left.clear();
                self.right.clear();
                for fr in chunk.chunks_exact(2) {
                    self.left.push(fr[0]);
                    self.right.push(fr[1]);
                }
                self.enc.encode_to_vec(DualPcm { left: &self.left, right: &self.right }, &mut self.buf)
            };
            r.map_err(lame_err)?;
            self.out.write_all(&self.buf)?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> io::Result<()> {
        self.buf.clear();
        self.buf.reserve(7200);
        self.enc.flush_to_vec::<FlushNoGap>(&mut self.buf).map_err(lame_err)?;
        self.out.write_all(&self.buf)?;
        self.out.flush()?;
        sync_file(self.out.get_ref())
    }
}

/// Reads interleaved f32 frames from a raw spool file, from `start` for `frames`.
pub struct SpoolReader {
    rd: BufReader<File>,
    n: usize,
    left: u64,
    total: u64,
    bytes: Vec<u8>,
    buf: Vec<f32>,
}

/// Frames in a spool file with `n` tracks.
pub fn spool_frames(raw: &Path, n: usize) -> Result<u64, String> {
    let len = std::fs::metadata(raw).map_err(|e| format!("Spool file missing: {e}"))?.len();
    Ok(len / (4 * n.max(1) as u64))
}

impl SpoolReader {
    pub fn open(raw: &Path, n: usize, start: u64, frames: u64) -> Result<SpoolReader, String> {
        let n = n.max(1);
        let mut f = File::open(raw).map_err(|e| format!("Spool file missing: {e}"))?;
        f.seek(SeekFrom::Start(start * 4 * n as u64)).map_err(|e| format!("Spool read failed: {e}"))?;
        Ok(SpoolReader { rd: BufReader::with_capacity(1 << 20, f), n, left: frames, total: frames, bytes: Vec::new(), buf: Vec::new() })
    }

    pub fn channels(&self) -> usize {
        self.n
    }

    /// Up to `max` frames, `None` at the end of the range.
    pub fn next_block(&mut self, max: usize) -> Result<Option<&[f32]>, String> {
        if self.left == 0 {
            return Ok(None);
        }
        let want = (self.left as usize).min(max);
        self.bytes.resize(want * self.n * 4, 0);
        self.rd.read_exact(&mut self.bytes).map_err(|e| format!("Spool read failed: {e}"))?;
        self.buf.clear();
        self.buf.extend(self.bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        self.left -= want as u64;
        Ok(Some(&self.buf))
    }

    fn done(&self) -> f32 {
        (self.total - self.left) as f32 / self.total.max(1) as f32
    }
}

/// Channels that come out of [`Layout::Mix`] for `n` tracks: mono stays mono,
/// stereo stays stereo, more get mixed to stereo.
pub fn mix_channels(n: usize) -> u16 {
    n.clamp(1, 2) as u16
}

/// Mix multitrack frames to stereo: every track centered, equal power, peaks
/// over full scale bent softly back under it instead of clipping.
pub fn mix_stereo(frames: &[f32], n: usize, out: &mut Vec<f32>) {
    let g = 1.0 / (n as f32).sqrt();
    out.clear();
    for fr in frames.chunks_exact(n) {
        let v = soft_clip(fr.iter().sum::<f32>() * g);
        out.push(v);
        out.push(v);
    }
}

/// Untouched up to -1 dBFS, then a smooth knee that tops out just under full scale.
pub fn soft_clip(x: f32) -> f32 {
    const T: f32 = 0.891; // -1 dBFS
    const CEIL: f32 = 0.995;
    let a = x.abs();
    if a <= T {
        x
    } else {
        x.signum() * (T + (CEIL - T) * ((a - T) / (CEIL - T)).tanh())
    }
}

/// How tracks map onto sinks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layout {
    /// One sink, all tracks interleaved as recorded.
    AsRecorded,
    /// One mono sink per track.
    Split,
    /// One sink, more than two tracks mixed to stereo (see [`mix_channels`]).
    Mix,
}

/// Pump a spool range through the sinks. Finishes them on success; on error
/// the caller throws the half-written files away.
pub fn render(mut rd: SpoolReader, layout: Layout, mut sinks: Vec<Box<dyn Sink>>, progress: &mut dyn FnMut(f32)) -> Result<(), String> {
    let n = rd.channels();
    let mut tracks: Vec<Vec<f32>> = vec![Vec::new(); if layout == Layout::Split { n } else { 0 }];
    let mut mixed = Vec::new();
    let mut blocks = 0u32;
    let werr = |e: io::Error| format!("Write failed: {e}");
    while let Some(block) = rd.next_block(16384)? {
        match layout {
            Layout::Split => {
                for t in tracks.iter_mut() {
                    t.clear();
                }
                for fr in block.chunks_exact(n) {
                    for (t, &v) in tracks.iter_mut().zip(fr) {
                        t.push(v);
                    }
                }
                for (s, t) in sinks.iter_mut().zip(&tracks) {
                    s.write(t).map_err(werr)?;
                }
            }
            Layout::Mix if n > 2 => {
                mix_stereo(block, n, &mut mixed);
                sinks[0].write(&mixed).map_err(werr)?;
            }
            _ => sinks[0].write(block).map_err(werr)?,
        }
        blocks += 1;
        if blocks % 4 == 0 {
            progress(rd.done());
        }
    }
    for s in sinks {
        s.finish().map_err(|e| format!("Finalize failed: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn spool(dir: &TempDir, n: usize, frames: usize, f: impl Fn(usize, usize) -> f32) -> std::path::PathBuf {
        let p = dir.join("t.raw");
        let mut b = Vec::new();
        for i in 0..frames {
            for c in 0..n {
                b.extend_from_slice(&f(i, c).to_le_bytes());
            }
        }
        std::fs::write(&p, b).unwrap();
        p
    }

    fn sine(i: usize, c: usize) -> f32 {
        ((i as f32) * 0.05 * (c + 1) as f32).sin() * 0.5
    }

    /// Frame header check: MPEG-1 layer III, the bitrate and rate we asked for.
    pub fn first_frame(mp3: &[u8]) -> (u32, u32, bool) {
        let mut i = 0;
        if mp3.starts_with(b"ID3") {
            let sz = mp3[6..10].iter().fold(0usize, |a, &b| (a << 7) | b as usize);
            i = 10 + sz;
        }
        let h = &mp3[i..i + 4];
        assert_eq!(h[0], 0xFF);
        assert_eq!(h[1] & 0xFE, 0xFA, "MPEG-1 layer III");
        let kbps = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320][(h[2] >> 4) as usize];
        let rate = [44_100, 48_000, 32_000][((h[2] >> 2) & 3) as usize];
        (kbps, rate, (h[3] >> 6) == 3)
    }

    #[test]
    fn mp3_stereo_and_mono_with_tags() {
        let d = TempDir::new("enc");
        let raw = spool(&d, 2, 48_000 * 3, sine);
        let out = d.join("a.mp3");
        let tags = Tags { title: "Take 1".into(), artist: "Band".into(), album: "Band Practice".into(), year: "2026".into(), comment: "x".into() };
        let sink = Mp3Writer::create(&out, 48_000, 2, 192, &tags).unwrap();
        let mut seen = vec![];
        render(SpoolReader::open(&raw, 2, 0, 48_000 * 3).unwrap(), Layout::AsRecorded, vec![Box::new(sink)], &mut |p| seen.push(p)).unwrap();
        let b = std::fs::read(&out).unwrap();
        assert!(b.starts_with(b"ID3"));
        assert!(b.windows(6).any(|w| w == b"Take 1"));
        assert_eq!(first_frame(&b), (192, 48_000, false));
        // 3 s at 192k is ~72 KB
        assert!((60_000..90_000).contains(&b.len()), "{}", b.len());
        assert!(!seen.is_empty() && seen.windows(2).all(|w| w[0] <= w[1]));
        // a real decoder agrees (macOS ships one)
        if Path::new("/usr/bin/afinfo").exists() {
            let info = String::from_utf8(std::process::Command::new("/usr/bin/afinfo").arg(&out).output().unwrap().stdout).unwrap();
            assert!(info.contains("File type ID:   MPG3"), "{info}");
            assert!(info.contains("2 ch,") && info.contains(" 48000 Hz") && info.contains("bit rate: 192000"), "{info}");
            let secs: f64 = info.lines().find_map(|l| l.strip_prefix("estimated duration: ")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
            assert!((secs - 3.0).abs() < 0.1, "{secs}");
        }

        let mono = d.join("m.mp3");
        let sink = Mp3Writer::create(&mono, 48_000, 1, 128, &Tags::default()).unwrap();
        render(SpoolReader::open(&raw, 2, 0, 48_000).unwrap(), Layout::Split, vec![Box::new(sink), Box::new(Mp3Writer::create(&d.join("m2.mp3"), 48_000, 1, 128, &Tags::default()).unwrap())], &mut |_| {}).unwrap();
        let b = std::fs::read(&mono).unwrap();
        assert!(!b.starts_with(b"ID3"), "no tags, no ID3");
        assert_eq!(first_frame(&b), (128, 48_000, true));
    }

    #[test]
    fn mp3_rates_and_bitrates() {
        assert_eq!(mp3_rate(44_100), 44_100);
        assert_eq!(mp3_rate(48_000), 48_000);
        assert_eq!(mp3_rate(32_000), 32_000);
        assert_eq!(mp3_rate(88_200), 44_100);
        assert_eq!(mp3_rate(176_400), 44_100);
        assert_eq!(mp3_rate(96_000), 48_000);
        assert_eq!(mp3_rate(192_000), 48_000);
        let d = TempDir::new("enc");
        for (k, want) in [(64, 128), (128, 128), (160, 160), (192, 192), (224, 224), (256, 256), (320, 320), (999, 320)] {
            let out = d.join(format!("{k}.mp3"));
            let raw = spool(&d, 1, 96_000, sine);
            render(SpoolReader::open(&raw, 1, 0, 96_000).unwrap(), Layout::AsRecorded, vec![Box::new(Mp3Writer::create(&out, 96_000, 1, k, &Tags::default()).unwrap())], &mut |_| {}).unwrap();
            assert_eq!(first_frame(&std::fs::read(&out).unwrap()), (want, 48_000, true), "{k}");
        }
        assert!(Mp3Writer::create(&d.join("nope/x.mp3"), 48_000, 2, 192, &Tags::default()).is_err());
        assert!(Mp3Writer::create(&d.join("x.mp3"), 0, 2, 192, &Tags::default()).is_err(), "no rate");
    }

    #[test]
    fn mixdown_is_centered_equal_power_and_never_clips() {
        let mut out = vec![];
        mix_stereo(&[0.1, 0.1, 0.1, 0.1], 4, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.2).abs() < 1e-6 && out[0] == out[1]);
        mix_stereo(&[1.0; 8], 8, &mut out);
        assert!(out[0] < 1.0 && out[0] > 0.95, "{}", out[0]);
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.891), -0.891);
        assert!(soft_clip(5.0) < 1.0 && soft_clip(-5.0) > -1.0);
        assert!(soft_clip(0.95) > soft_clip(0.92), "monotonic");
        assert_eq!((mix_channels(1), mix_channels(2), mix_channels(8)), (1, 2, 2));
    }

    #[test]
    fn render_layouts_and_ranges() {
        let d = TempDir::new("enc");
        let raw = spool(&d, 3, 1000, |i, c| (i * 10 + c) as f32 / 100_000.0);
        assert_eq!(spool_frames(&raw, 3).unwrap(), 1000);
        assert!(spool_frames(&d.join("gone"), 3).is_err());
        // a range from the middle
        let mut rd = SpoolReader::open(&raw, 3, 990, 10).unwrap();
        let b = rd.next_block(4).unwrap().unwrap().to_vec();
        assert_eq!(b.len(), 12);
        assert_eq!(b[0], 9900.0 / 100_000.0);
        assert_eq!(rd.next_block(100).unwrap().unwrap().len(), 18);
        assert!(rd.next_block(100).unwrap().is_none());
        // reading past the end of the file is an error, not silence
        let mut rd = SpoolReader::open(&raw, 3, 999, 5).unwrap();
        assert!(rd.next_block(5).unwrap_err().starts_with("Spool read failed"));
        assert!(SpoolReader::open(&d.join("gone"), 3, 0, 1).is_err());
        // mix: 3 tracks to 2 channels
        let header = {
            WavWriter::create(&d.join("empty.wav"), 48_000, 2, 16, false).unwrap().finalize().unwrap();
            std::fs::metadata(d.join("empty.wav")).unwrap().len()
        };
        let w = WavWriter::create(&d.join("mix.wav"), 48_000, 2, 16, false).unwrap();
        render(SpoolReader::open(&raw, 3, 0, 1000).unwrap(), Layout::Mix, vec![Box::new(w)], &mut |_| {}).unwrap();
        assert_eq!(std::fs::metadata(d.join("mix.wav")).unwrap().len(), header + 1000 * 2 * 2);
        // mix of 2 tracks passes through
        let raw2 = spool(&d, 2, 100, sine);
        let w = WavWriter::create(&d.join("st.wav"), 48_000, 2, 16, false).unwrap();
        render(SpoolReader::open(&raw2, 2, 0, 100).unwrap(), Layout::Mix, vec![Box::new(w)], &mut |_| {}).unwrap();
        assert_eq!(std::fs::metadata(d.join("st.wav")).unwrap().len(), header + 100 * 4);
    }

    struct Broken(bool);
    impl Sink for Broken {
        fn write(&mut self, _: &[f32]) -> io::Result<()> {
            if self.0 { Err(io::Error::other("disk full")) } else { Ok(()) }
        }
        fn finish(self: Box<Self>) -> io::Result<()> {
            Err(io::Error::other("nope"))
        }
    }

    #[test]
    fn render_errors() {
        let d = TempDir::new("enc");
        let raw = spool(&d, 2, 100, sine);
        let rd = || SpoolReader::open(&raw, 2, 0, 100).unwrap();
        assert_eq!(render(rd(), Layout::AsRecorded, vec![Box::new(Broken(true))], &mut |_| {}).unwrap_err(), "Write failed: disk full");
        assert_eq!(render(rd(), Layout::Split, vec![Box::new(Broken(true)), Box::new(Broken(true))], &mut |_| {}).unwrap_err(), "Write failed: disk full");
        assert_eq!(render(rd(), Layout::AsRecorded, vec![Box::new(Broken(false))], &mut |_| {}).unwrap_err(), "Finalize failed: nope");
        let raw3 = spool(&d, 3, 10, sine);
        assert!(render(SpoolReader::open(&raw3, 3, 0, 10).unwrap(), Layout::Mix, vec![Box::new(Broken(true))], &mut |_| {}).is_err());
        let rd = SpoolReader::open(&raw, 2, 0, 200).unwrap();
        assert!(render(rd, Layout::AsRecorded, vec![Box::new(Broken(false))], &mut |_| {}).unwrap_err().starts_with("Spool read failed"));
    }
}
