//! WAV writer with automatic RF64 promotion (multitrack takes cross 4 GB fast:
//! 16 ch × 24 bit × 48 kHz is ~2.3 MB/s, so ~30 minutes).
//!
//! Layout: RIFF | JUNK(28, becomes ds64 if needed) | fmt (EXTENSIBLE) | data

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

const GUID_TAIL: [u8; 14] = [0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71];

pub struct WavWriter {
    out: BufWriter<File>,
    channels: u16,
    bits: u16,
    float: bool,
    data_bytes: u64,
    data_start: u64,
    dither: u32,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u16, bits: u16, float: bool) -> std::io::Result<Self> {
        let f = File::create(path)?;
        let mut w = WavWriter { out: BufWriter::with_capacity(1 << 20, f), channels, bits, float, data_bytes: 0, data_start: 0, dither: 0x1234_5678 };
        let block_align = channels as u32 * (bits as u32 / 8);
        let o = &mut w.out;
        o.write_all(b"RIFF")?;
        o.write_all(&0u32.to_le_bytes())?;
        o.write_all(b"WAVE")?;
        o.write_all(b"JUNK")?;
        o.write_all(&28u32.to_le_bytes())?;
        o.write_all(&[0u8; 28])?;
        o.write_all(b"fmt ")?;
        o.write_all(&40u32.to_le_bytes())?;
        o.write_all(&0xFFFEu16.to_le_bytes())?; // WAVE_FORMAT_EXTENSIBLE
        o.write_all(&channels.to_le_bytes())?;
        o.write_all(&sample_rate.to_le_bytes())?;
        o.write_all(&(sample_rate * block_align).to_le_bytes())?;
        o.write_all(&(block_align as u16).to_le_bytes())?;
        o.write_all(&bits.to_le_bytes())?;
        o.write_all(&22u16.to_le_bytes())?;
        o.write_all(&bits.to_le_bytes())?; // valid bits
        o.write_all(&0u32.to_le_bytes())?; // channel mask: unspecified
        o.write_all(&(if float { 3u16 } else { 1u16 }).to_le_bytes())?;
        o.write_all(&GUID_TAIL)?;
        o.write_all(b"data")?;
        o.write_all(&0u32.to_le_bytes())?;
        w.data_start = 12 + 36 + 48 + 8;
        Ok(w)
    }

    #[inline]
    fn rnd(&mut self) -> f32 {
        let mut s = self.dither;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        self.dither = s;
        (s as f32 / u32::MAX as f32) - 0.5
    }

    /// Interleaved samples, length must be a multiple of `channels`.
    pub fn write(&mut self, samples: &[f32]) -> std::io::Result<()> {
        match (self.float, self.bits) {
            (true, _) => {
                for &s in samples {
                    self.out.write_all(&s.to_le_bytes())?;
                }
                self.data_bytes += samples.len() as u64 * 4;
            }
            (false, 16) => {
                for &s in samples {
                    let d = (self.rnd() + self.rnd()) / 32767.0; // TPDF
                    let v = ((s + d).clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    self.out.write_all(&v.to_le_bytes())?;
                }
                self.data_bytes += samples.len() as u64 * 2;
            }
            _ => {
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) as f64 * 8_388_607.0).round() as i32;
                    self.out.write_all(&v.to_le_bytes()[..3])?;
                }
                self.data_bytes += samples.len() as u64 * 3;
            }
        }
        Ok(())
    }

    pub fn finalize(self) -> std::io::Result<u64> {
        self.finalize_with_limit(u32::MAX as u64)
    }

    /// `limit`: biggest size a plain RIFF header can hold. Only tests lower it.
    fn finalize_with_limit(mut self, limit: u64) -> std::io::Result<u64> {
        let pad = self.data_bytes & 1;
        if pad == 1 {
            self.out.write_all(&[0])?;
        }
        let riff = self.data_start - 8 + self.data_bytes + pad;
        let frames = self.data_bytes / (self.channels as u64 * self.bits as u64 / 8).max(1);
        let mut f = self.out.into_inner().map_err(|e| e.into_error())?;
        if riff <= limit && self.data_bytes <= limit {
            f.seek(SeekFrom::Start(4))?;
            f.write_all(&(riff as u32).to_le_bytes())?;
            f.seek(SeekFrom::Start(self.data_start - 4))?;
            f.write_all(&(self.data_bytes as u32).to_le_bytes())?;
        } else {
            f.seek(SeekFrom::Start(0))?;
            f.write_all(b"RF64")?;
            f.write_all(&u32::MAX.to_le_bytes())?;
            f.seek(SeekFrom::Start(12))?;
            f.write_all(b"ds64")?;
            f.write_all(&28u32.to_le_bytes())?;
            f.write_all(&riff.to_le_bytes())?;
            f.write_all(&self.data_bytes.to_le_bytes())?;
            f.write_all(&frames.to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?;
            f.seek(SeekFrom::Start(self.data_start - 4))?;
            f.write_all(&u32::MAX.to_le_bytes())?;
        }
        f.flush()?;
        crate::config::sync_file(&f)?;
        Ok(self.data_start + self.data_bytes + pad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn u16le(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }
    fn u32le(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64le(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    fn write(d: &TempDir, ch: u16, bits: u16, float: bool, samples: &[f32]) -> Vec<u8> {
        let p = d.join(format!("{ch}-{bits}-{float}.wav"));
        let mut w = WavWriter::create(&p, 48000, ch, bits, float).unwrap();
        w.write(samples).unwrap();
        let len = w.finalize().unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(b.len() as u64, len);
        b
    }

    #[test]
    fn header_fields() {
        let d = TempDir::new("wav");
        let b = write(&d, 2, 24, false, &[0.5, -0.5, 0.25, -0.25, 1.0, -1.0]);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32le(&b, 4) as usize, b.len() - 8);
        assert_eq!(&b[8..12], b"WAVE");
        assert_eq!(&b[12..16], b"JUNK");
        assert_eq!(&b[48..52], b"fmt ");
        assert_eq!(u16le(&b, 56), 0xFFFE);
        assert_eq!(u16le(&b, 58), 2); // channels
        assert_eq!(u32le(&b, 60), 48000);
        assert_eq!(u32le(&b, 64), 48000 * 6); // byte rate
        assert_eq!(u16le(&b, 68), 6); // block align
        assert_eq!(u16le(&b, 70), 24);
        assert_eq!(u16le(&b, 80), 1); // PCM subformat
        assert_eq!(&b[96..100], b"data");
        assert_eq!(u32le(&b, 100), 18);
    }

    #[test]
    fn pcm24_values_and_clipping() {
        let d = TempDir::new("wav");
        let b = write(&d, 1, 24, false, &[0.5, -0.5, 1.0, -1.0, 7.0, -7.0]);
        let s = |i: usize| i32::from_le_bytes([0, b[104 + i * 3], b[105 + i * 3], b[106 + i * 3]]) >> 8;
        assert_eq!(s(0), 4_194_304);
        assert_eq!(s(1), -4_194_304);
        assert_eq!(s(2), 8_388_607);
        assert_eq!(s(3), -8_388_607);
        assert_eq!(s(4), 8_388_607, "clipped, not wrapped");
        assert_eq!(s(5), -8_388_607);
    }

    #[test]
    fn pcm16_dithered_values() {
        let d = TempDir::new("wav");
        let b = write(&d, 1, 16, false, &[0.0, 0.5, -0.5, 1.0, -1.0, 3.0]);
        assert_eq!(u16le(&b, 70), 16);
        let s = |i: usize| i16::from_le_bytes([b[104 + i * 2], b[105 + i * 2]]) as i32;
        // TPDF dither: at most 1 LSB off
        for (i, want) in [0, 16384, -16384, 32767, -32767, 32767].into_iter().enumerate() {
            assert!((s(i) - want).abs() <= 1, "sample {i}: {} vs {want}", s(i));
        }
        assert_eq!(u32le(&b, 100), 12);
    }

    #[test]
    fn float32_is_bit_exact() {
        let d = TempDir::new("wav");
        let v = [0.1f32, -0.75, 1.5, f32::MIN_POSITIVE];
        let b = write(&d, 2, 32, true, &v);
        assert_eq!(u16le(&b, 80), 3, "IEEE float subformat");
        for (i, x) in v.iter().enumerate() {
            assert_eq!(f32::from_le_bytes(b[104 + i * 4..108 + i * 4].try_into().unwrap()), *x);
        }
    }

    #[test]
    fn odd_data_gets_pad_byte() {
        let d = TempDir::new("wav");
        let b = write(&d, 1, 24, false, &[0.1]);
        assert_eq!(u32le(&b, 100), 3, "data size excludes pad");
        assert_eq!(b.len(), 104 + 4);
        assert_eq!(u32le(&b, 4) as usize, b.len() - 8, "riff size includes pad");
    }

    #[test]
    fn empty_take_is_valid() {
        let d = TempDir::new("wav");
        let b = write(&d, 2, 24, false, &[]);
        assert_eq!(b.len(), 104);
        assert_eq!(u32le(&b, 100), 0);
    }

    #[test]
    fn promotes_to_rf64() {
        let d = TempDir::new("wav");
        let p = d.join("big.wav");
        let mut w = WavWriter::create(&p, 48000, 2, 24, false).unwrap();
        w.write(&[0.0; 10]).unwrap();
        let len = w.finalize_with_limit(16).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(b.len() as u64, len);
        assert_eq!(&b[0..4], b"RF64");
        assert_eq!(u32le(&b, 4), u32::MAX);
        assert_eq!(&b[12..16], b"ds64");
        assert_eq!(u32le(&b, 16), 28);
        assert_eq!(u64le(&b, 20), b.len() as u64 - 8);
        assert_eq!(u64le(&b, 28), 30, "data bytes");
        assert_eq!(u64le(&b, 36), 5, "frames");
        assert_eq!(u32le(&b, 100), u32::MAX);
    }

    #[test]
    fn create_fails_cleanly() {
        let d = TempDir::new("wav");
        assert!(WavWriter::create(&d.join("missing/x.wav"), 48000, 1, 24, false).is_err());
    }
}
