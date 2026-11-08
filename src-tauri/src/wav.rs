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

    pub fn finalize(mut self) -> std::io::Result<u64> {
        let pad = self.data_bytes & 1;
        if pad == 1 {
            self.out.write_all(&[0])?;
        }
        let riff = self.data_start - 8 + self.data_bytes + pad;
        let frames = self.data_bytes / (self.channels as u64 * self.bits as u64 / 8).max(1);
        let mut f = self.out.into_inner().map_err(|e| e.into_error())?;
        if riff <= u32::MAX as u64 && self.data_bytes <= u32::MAX as u64 {
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
        f.sync_all()?;
        Ok(self.data_start + self.data_bytes + pad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_valid_header() {
        let dir = std::env::temp_dir().join(format!("psr-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        let mut w = WavWriter::create(&p, 48000, 2, 24, false).unwrap();
        w.write(&[0.5, -0.5, 0.25, -0.25, 1.0, -1.0]).unwrap();
        let len = w.finalize().unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(b.len() as u64, len);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize, b.len() - 8);
        assert_eq!(&b[96..100], b"data");
        assert_eq!(u32::from_le_bytes(b[100..104].try_into().unwrap()), 18);
        let s0 = i32::from_le_bytes([0, b[104], b[105], b[106]]) >> 8;
        assert_eq!(s0, 4_194_304);
        std::fs::remove_dir_all(dir).ok();
    }
}
