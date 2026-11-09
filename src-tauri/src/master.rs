//! Auto master: makes every take loud and clear without anyone touching a knob.
//!
//! Two passes over the finished take. [`Analyzer`] measures integrated loudness
//! (ITU-R BS.1770-4 / EBU R128: K-weighting, 400 ms blocks, absolute and
//! relative gates) and a robust peak level. [`gain_db`] turns that into one
//! gain. [`Master`] then runs: 20 Hz high-pass -> gain -> look-ahead limiter
//! with a 4x oversampled true-peak detector, so nothing goes over -1 dBTP.
//!
//! The limiter is built so it can't overshoot: the needed gain is held for the
//! look-ahead window, released (fast for hits, slow for loud passages), then
//! averaged over the same window. That average is never above what the peak
//! needs, and the audio is delayed to line up with it.

use std::collections::VecDeque;

/// Where takes end up: what streaming services normalize to, loud on a phone.
pub const TARGET_LUFS: f64 = -14.0;
/// True-peak ceiling. -1 dBTP leaves room for MP3 encoding.
pub const CEILING_DBTP: f64 = -1.0;
/// Most the limiter may take off the (robust) peaks. More would squash.
const MAX_LIMIT_DB: f64 = 6.0;
/// Never more gain than this, whatever the meter says.
const MAX_GAIN_DB: f64 = 30.0;
/// Quieter than this there's nothing worth making louder (room tone, no take).
const SILENCE_LUFS: f64 = -50.0;
/// Rumble below this eats headroom and nobody hears it.
const HIGHPASS_HZ: f64 = 20.0;
const LOOKAHEAD_S: f64 = 0.005;

pub(crate) fn db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

pub(crate) fn lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Direct form I biquad with its own state per channel.
#[derive(Clone)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: Vec<[f64; 4]>,
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 2], ch: usize) -> Biquad {
        Biquad { b, a, z: vec![[0.0; 4]; ch] }
    }

    /// RBJ cookbook high-pass.
    fn highpass(rate: f64, f: f64, q: f64, ch: usize) -> Biquad {
        let w = 2.0 * std::f64::consts::PI * f / rate;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        Biquad::new([(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0], [-2.0 * c / a0, (1.0 - alpha) / a0], ch)
    }

    /// BS.1770 K-weighting, stage 1: the head's high shelf (for any rate, as in libebur128).
    fn k_shelf(rate: f64, ch: usize) -> Biquad {
        let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
        let k = (std::f64::consts::PI * f0 / rate).tan();
        let vh = 10f64.powf(g / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        Biquad::new([(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], ch)
    }

    /// BS.1770 K-weighting, stage 2: the RLB high-pass.
    fn k_highpass(rate: f64, ch: usize) -> Biquad {
        let (f0, q) = (38.13547087602444, 0.5003270373238773);
        let k = (std::f64::consts::PI * f0 / rate).tan();
        let a0 = 1.0 + k / q + k * k;
        Biquad::new([1.0, -2.0, 1.0], [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], ch)
    }

    #[inline]
    fn run(&mut self, c: usize, x: f64) -> f64 {
        let z = &mut self.z[c];
        let y = self.b[0] * x + self.b[1] * z[0] + self.b[2] * z[1] - self.a[0] * z[2] - self.a[1] * z[3];
        *z = [x, z[0], y, z[2]];
        y
    }
}

/// What the analysis pass found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Analysis {
    /// Integrated loudness (LUFS), `-inf` for silence.
    pub lufs: f64,
    /// Level only 0.1 % of 10 ms windows go above (dBFS): peaks without the odd click.
    pub peak_db: f64,
}

/// Measures a whole take, one block of interleaved frames at a time.
pub struct Analyzer {
    ch: usize,
    hp: Biquad,
    k1: Biquad,
    k2: Biquad,
    /// Mono counts double: it plays on both speakers.
    weight: f64,
    seg_len: usize,
    seg_pos: usize,
    seg_sum: f64,
    /// Mean square of every 100 ms segment.
    segs: Vec<f64>,
    win_len: usize,
    win_pos: usize,
    win_peak: f64,
    /// 10 ms window peaks, 0.25 dB bins from -120 dBFS up.
    hist: Vec<u64>,
}

const HIST_FLOOR: f64 = -120.0;
const HIST_STEP: f64 = 0.25;

impl Analyzer {
    pub fn new(rate: u32, ch: usize) -> Analyzer {
        let r = rate.max(1) as f64;
        let ch = ch.max(1);
        Analyzer {
            ch,
            hp: Biquad::highpass(r, HIGHPASS_HZ, std::f64::consts::FRAC_1_SQRT_2, ch),
            k1: Biquad::k_shelf(r, ch),
            k2: Biquad::k_highpass(r, ch),
            weight: if ch == 1 { 2.0 } else { 1.0 },
            seg_len: (r * 0.1).round().max(1.0) as usize,
            seg_pos: 0,
            seg_sum: 0.0,
            segs: Vec::new(),
            win_len: (r * 0.01).round().max(1.0) as usize,
            win_pos: 0,
            win_peak: 0.0,
            hist: vec![0; ((12.0 - HIST_FLOOR) / HIST_STEP) as usize + 1],
        }
    }

    pub fn push(&mut self, frames: &[f32]) {
        for fr in frames.chunks_exact(self.ch) {
            let mut e = 0.0;
            for (c, &x) in fr.iter().enumerate() {
                let h = self.hp.run(c, x as f64);
                self.win_peak = self.win_peak.max(h.abs());
                let k = self.k2.run(c, self.k1.run(c, h));
                e += k * k;
            }
            self.seg_sum += e * self.weight;
            self.seg_pos += 1;
            if self.seg_pos == self.seg_len {
                self.segs.push(self.seg_sum / self.seg_len as f64);
                self.seg_pos = 0;
                self.seg_sum = 0.0;
            }
            self.win_pos += 1;
            if self.win_pos == self.win_len {
                let bin = ((db(self.win_peak) - HIST_FLOOR) / HIST_STEP).clamp(0.0, (self.hist.len() - 1) as f64) as usize;
                self.hist[bin] += 1;
                self.win_pos = 0;
                self.win_peak = 0.0;
            }
        }
    }

    pub fn finish(self) -> Analysis {
        // 400 ms blocks, 75 % overlap = 4 segments, hop 1
        let blocks: Vec<f64> = self.segs.windows(4).map(|w| w.iter().sum::<f64>() / 4.0).collect();
        let loud = |z: f64| -0.691 + 10.0 * z.max(1e-20).log10();
        let gated = |thr: f64| -> Vec<f64> { blocks.iter().copied().filter(|&z| loud(z) > thr).collect() };
        let abs = gated(-70.0);
        let lufs = if abs.is_empty() {
            f64::NEG_INFINITY
        } else {
            let rel = loud(abs.iter().sum::<f64>() / abs.len() as f64) - 10.0;
            let both: Vec<f64> = abs.into_iter().filter(|&z| loud(z) > rel).collect();
            loud(both.iter().sum::<f64>() / both.len() as f64)
        };
        let total: u64 = self.hist.iter().sum();
        let allowed = total / 1000; // 0.1 %
        let mut above = 0;
        let mut peak_db = f64::NEG_INFINITY;
        for (i, &n) in self.hist.iter().enumerate().rev() {
            above += n;
            if n > 0 && above > allowed {
                peak_db = HIST_FLOOR + (i + 1) as f64 * HIST_STEP;
                break;
            }
        }
        Analysis { lufs, peak_db }
    }
}

/// How much louder to make it. Up to the target, never more than the limiter
/// can handle cleanly, never louder than [`MAX_GAIN_DB`], never pumps up
/// silence, never turns down a take that is loud already (the limiter keeps
/// its peaks in check).
pub fn gain_db(a: &Analysis) -> f64 {
    // NaN and -inf (nothing measured) count as silence too
    if a.lufs.is_nan() || a.lufs <= SILENCE_LUFS {
        return 0.0;
    }
    let peak_room = CEILING_DBTP - a.peak_db + MAX_LIMIT_DB;
    let want = TARGET_LUFS - a.lufs;
    if want <= 0.0 {
        return peak_room.min(0.0);
    }
    want.min(peak_room).min(MAX_GAIN_DB)
}

/// 4x oversampling for peak detection only: what a DAC will really output
/// between samples. 64-tap windowed sinc, split into 4 phases.
struct TruePeak {
    phases: [[f64; 16]; 4],
    hist: Vec<[f64; 16]>,
    pos: usize,
}

/// Detection lags the input by this many samples.
const TP_DELAY: usize = 8;

impl TruePeak {
    fn new(ch: usize) -> TruePeak {
        let n = 64;
        let fc = 0.125 * 0.92; // just under the original Nyquist, at 4x
        let mut phases = [[0.0; 16]; 4];
        for k in 0..n {
            let t = k as f64 - (n - 1) as f64 / 2.0;
            let sinc = if t == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t) };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (n - 1) as f64).cos() + 0.08 * (4.0 * std::f64::consts::PI * k as f64 / (n - 1) as f64).cos();
            phases[k % 4][k / 4] = sinc * w;
        }
        for p in phases.iter_mut() {
            let s: f64 = p.iter().sum();
            p.iter_mut().for_each(|c| *c /= s);
        }
        TruePeak { phases, hist: vec![[0.0; 16]; ch], pos: 0 }
    }

    /// Feed one frame, get the highest true peak (all channels) around `TP_DELAY` frames ago.
    fn push(&mut self, frame: &[f64]) -> f64 {
        self.pos = (self.pos + 1) % 16;
        let mut peak = 0f64;
        for (c, &x) in frame.iter().enumerate() {
            let h = &mut self.hist[c];
            h[self.pos] = x;
            peak = peak.max(h[(self.pos + 16 - TP_DELAY) % 16].abs());
            for ph in &self.phases {
                let mut y = 0.0;
                for (j, &cf) in ph.iter().enumerate() {
                    y += cf * h[(self.pos + 16 - j) % 16];
                }
                peak = peak.max(y.abs());
            }
        }
        peak
    }
}

/// The processing pass: high-pass, gain, true-peak limiter. Output is as long
/// as the input and lined up with it (the look-ahead delay is taken back out).
pub struct Master {
    ch: usize,
    hp: Biquad,
    gain: f64,
    ceil: f64,
    tp: TruePeak,
    /// Sliding minimum of the needed gain over the hold window: (index, gain).
    hold: VecDeque<(u64, f64)>,
    hold_len: u64,
    fast: f64,
    slow: f64,
    rel_fast: f64,
    att_slow: f64,
    rel_slow: f64,
    boxw: VecDeque<f64>,
    box_len: usize,
    box_sum: f64,
    /// Audio waiting for its gain, interleaved.
    delay: VecDeque<f64>,
    delay_frames: usize,
    /// Leading output frames that are only delay, dropped.
    skip: usize,
    n: u64,
    frame: Vec<f64>,
}

impl Master {
    pub fn new(rate: u32, ch: usize, gain_db: f64) -> Master {
        let r = rate.max(1) as f64;
        let ch = ch.max(1);
        let l = (r * LOOKAHEAD_S).round().max(1.0) as usize;
        let coef = |s: f64| (-1.0 / (s * r)).exp();
        let delay_frames = TP_DELAY + l - 1;
        Master {
            ch,
            hp: Biquad::highpass(r, HIGHPASS_HZ, std::f64::consts::FRAC_1_SQRT_2, ch),
            gain: lin(gain_db),
            ceil: lin(CEILING_DBTP),
            tp: TruePeak::new(ch),
            hold: VecDeque::new(),
            hold_len: l as u64 + 2, // a little wider: detection is only sample accurate
            fast: 1.0,
            slow: 1.0,
            rel_fast: coef(0.05),
            att_slow: coef(0.08),
            rel_slow: coef(0.6),
            boxw: VecDeque::from(vec![1.0; l]),
            box_len: l,
            box_sum: l as f64,
            delay: VecDeque::from(vec![0.0; delay_frames * ch]),
            delay_frames,
            skip: delay_frames,
            n: 0,
            frame: vec![0.0; ch],
        }
    }

    fn step(&mut self, input: Option<&[f32]>, out: &mut Vec<f32>) {
        for c in 0..self.ch {
            let x = input.map(|f| f[c] as f64).unwrap_or(0.0);
            self.frame[c] = if input.is_some() { self.hp.run(c, x) * self.gain } else { 0.0 };
        }
        let peak = self.tp.push(&self.frame);
        let need = if peak > self.ceil { self.ceil / peak } else { 1.0 };
        // sliding minimum over the hold window
        while self.hold.back().is_some_and(|&(_, g)| g >= need) {
            self.hold.pop_back();
        }
        self.hold.push_back((self.n, need));
        while self.hold.front().is_some_and(|&(i, _)| i + self.hold_len <= self.n) {
            self.hold.pop_front();
        }
        let h = self.hold.front().map(|&(_, g)| g).unwrap_or(1.0);
        // release: fast for hits, slow for loud passages; attack is instant either way
        self.fast = h.min(1.0 - (1.0 - self.fast) * self.rel_fast);
        let a = if h < self.slow { self.att_slow } else { self.rel_slow };
        self.slow = h + (self.slow - h) * a;
        let g = self.fast.min(self.slow);
        // average over the look-ahead: arrives exactly when the peak does
        self.box_sum += g - self.boxw.pop_front().unwrap_or(1.0);
        self.boxw.push_back(g);
        let gain = (self.box_sum / self.box_len as f64).min(1.0);
        self.delay.extend(self.frame.iter().copied());
        if self.skip > 0 {
            self.skip -= 1;
            self.delay.drain(..self.ch);
        } else {
            for _ in 0..self.ch {
                let y = self.delay.pop_front().unwrap_or(0.0) * gain;
                out.push(y.clamp(-self.ceil, self.ceil) as f32);
            }
        }
        self.n += 1;
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        for fr in input.chunks_exact(self.ch) {
            self.step(Some(fr), out);
        }
    }

    /// The last frames still in the delay line.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        out.clear();
        for _ in 0..self.delay_frames {
            self.step(None, out);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sine(rate: u32, ch: usize, secs: f64, freq: f64, amp: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n).flat_map(|i| std::iter::repeat_n((amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin()) as f32, ch)).collect()
    }

    pub fn measure(rate: u32, ch: usize, x: &[f32]) -> Analysis {
        let mut a = Analyzer::new(rate, ch);
        for b in x.chunks(4096 * ch) {
            a.push(b);
        }
        a.finish()
    }

    pub fn run(rate: u32, ch: usize, gain: f64, x: &[f32]) -> Vec<f32> {
        let mut m = Master::new(rate, ch, gain);
        let (mut out, mut buf) = (Vec::new(), Vec::new());
        for b in x.chunks(1000 * ch) {
            m.process(b, &mut buf);
            out.extend_from_slice(&buf);
        }
        m.finish(&mut buf);
        out.extend_from_slice(&buf);
        out
    }

    /// 4x true peak of a finished signal, measured independently (fine sinc interpolation).
    pub fn true_peak(x: &[f32], ch: usize) -> f64 {
        let mut p = 0f64;
        for c in 0..ch {
            let s: Vec<f64> = x.iter().skip(c).step_by(ch).map(|&v| v as f64).collect();
            for i in 16..s.len().saturating_sub(16) {
                for k in 0..8 {
                    let t = i as f64 + k as f64 / 8.0;
                    let v: f64 = (i - 15..i + 16).map(|j| {
                        let d = t - j as f64;
                        let sinc = if d == 0.0 { 1.0 } else { (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d) };
                        s[j] * sinc * (0.5 + 0.5 * (std::f64::consts::PI * d / 16.0).cos())
                    }).sum();
                    p = p.max(v.abs());
                }
            }
        }
        db(p)
    }

    #[test]
    fn loudness_matches_ebu_reference() {
        // EBU Tech 3341: stereo 1 kHz sine at -23 dBFS reads -23.0 LUFS
        for rate in [44_100, 48_000, 96_000] {
            let a = measure(rate, 2, &sine(rate, 2, 20.0, 1000.0, lin(-23.0)));
            assert!((a.lufs - -23.0).abs() < 0.1, "{rate}: {}", a.lufs);
            assert!((a.peak_db - -23.0).abs() < 0.3, "{rate}: {}", a.peak_db);
        }
        // -33 dBFS reads -33 (the relative gate doesn't care about steady tones)
        assert!((measure(48_000, 2, &sine(48_000, 2, 10.0, 1000.0, lin(-33.0))).lufs - -33.0).abs() < 0.1);
        // mono plays on both speakers: same reading as the stereo version
        assert!((measure(48_000, 1, &sine(48_000, 1, 10.0, 1000.0, lin(-23.0))).lufs - -23.0).abs() < 0.1);
        // silence and too-short takes have no loudness
        assert_eq!(measure(48_000, 2, &vec![0.0; 96_000]).lufs, f64::NEG_INFINITY);
        assert_eq!(measure(48_000, 2, &sine(48_000, 2, 0.3, 1000.0, 0.5)).lufs, f64::NEG_INFINITY);
    }

    #[test]
    fn gating_ignores_the_quiet_bits() {
        // 10 s of music, 20 s of near silence: loudness is the music's
        let mut x = sine(48_000, 2, 10.0, 440.0, lin(-20.0));
        x.extend(sine(48_000, 2, 20.0, 440.0, lin(-60.0)));
        let a = measure(48_000, 2, &x);
        let music = measure(48_000, 2, &sine(48_000, 2, 10.0, 440.0, lin(-20.0)));
        assert!((a.lufs - music.lufs).abs() < 0.3, "{} vs {}", a.lufs, music.lufs);
    }

    #[test]
    fn robust_peak_ignores_a_single_click() {
        let mut x = sine(48_000, 2, 10.0, 440.0, lin(-30.0));
        x[48_000 * 2] = 1.0; // one stick hits the mic
        let a = measure(48_000, 2, &x);
        assert!(a.peak_db < -29.0, "{}", a.peak_db);
    }

    #[test]
    fn gain_decisions() {
        let g = |lufs, peak_db| gain_db(&Analysis { lufs, peak_db });
        // quiet take, plenty of peak room: all the way to -14
        assert!((g(-30.0, -20.0) - 16.0).abs() < 1e-9);
        // peaky take: the limiter takes 6 dB at most
        assert!((g(-30.0, -6.0) - 11.0).abs() < 1e-9);
        // very quiet: capped
        assert_eq!(g(-49.0, -45.0), 30.0);
        // silence / room tone / nothing: left alone
        assert_eq!(g(-55.0, -40.0), 0.0);
        assert_eq!(g(f64::NEG_INFINITY, f64::NEG_INFINITY), 0.0);
        assert_eq!(g(f64::NAN, -10.0), 0.0);
        // loud already: not turned down, unless peaks are way past what the limiter should do
        assert_eq!(g(-10.0, -3.0), 0.0);
        assert!((g(-8.0, 9.0) - -4.0).abs() < 1e-9);
    }

    #[test]
    fn master_hits_target_and_never_clips() {
        let rate = 48_000;
        // a quiet, dynamic take: steady guitar-ish tone plus drum-ish bursts
        let mut x = sine(rate, 2, 12.0, 220.0, lin(-32.0));
        for hit in 0..24 {
            let at = hit * rate as usize / 2 * 2;
            for i in 0..2400 {
                let env = (-(i as f64) / 300.0).exp();
                let v = (env * lin(-14.0) * (2.0 * std::f64::consts::PI * 90.0 * i as f64 / rate as f64).sin()) as f32;
                x[at + i * 2] += v;
                x[at + i * 2 + 1] += v;
            }
        }
        let a = measure(rate, 2, &x);
        let g = gain_db(&a);
        assert!(g > 5.0, "a quiet take gets louder: {g}");
        let y = run(rate, 2, g, &x);
        assert_eq!(y.len(), x.len(), "same length");
        let after = measure(rate, 2, &y);
        assert!(after.lufs > a.lufs + 4.0, "{} -> {}", a.lufs, after.lufs);
        assert!(after.lufs < TARGET_LUFS + 0.5, "{}", after.lufs);
        let sp = y.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(db(sp as f64) <= CEILING_DBTP + 1e-6, "sample peak {}", db(sp as f64));
        assert!(true_peak(&y, 2) <= CEILING_DBTP + 0.3, "true peak {}", true_peak(&y, 2));
    }

    #[test]
    fn limiter_catches_intersample_peaks() {
        // a full-scale tone near fs/4 with 45 degree phase: samples at 0.707, true peak 1.0
        let rate = 48_000;
        let x: Vec<f32> = (0..rate).map(|i| (0.99 * (std::f64::consts::PI / 2.0 * i as f64 + std::f64::consts::FRAC_PI_4).sin()) as f32).collect();
        let y = run(rate, 1, 0.0, &x);
        assert!(true_peak(&y, 1) <= CEILING_DBTP + 0.3, "{}", true_peak(&y, 1));
        assert!(true_peak(&x, 1) > -0.5, "the test signal really has intersample peaks");
    }

    #[test]
    fn output_lines_up_with_input() {
        // a click stays where it was (after the gentle high-pass)
        let rate = 48_000;
        let mut x = vec![0f32; 4800];
        x[2000] = 0.1;
        let y = run(rate, 1, 0.0, &x);
        let at = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert_eq!(at, 2000);
        assert!((y[2000] - 0.1).abs() < 0.01, "untouched below the ceiling: {}", y[2000]);
        // below the ceiling and no gain: the limiter does nothing
        let s = sine(rate, 2, 1.0, 1000.0, 0.5);
        let y = run(rate, 2, 0.0, &s);
        let rms = |v: &[f32]| db((v.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt());
        assert!((rms(&y[4000..]) - rms(&s[4000..])).abs() < 0.05, "level unchanged");
        assert!(y[4000..].iter().fold(0f32, |m, v| m.max(v.abs())) < 0.51);
    }

    #[test]
    fn steady_parts_dont_pump() {
        // a loud sustained chord with a kick every half second, mastered hard:
        // the chord's level between kicks must stay put, not breathe with them
        let rate = 48_000;
        let mut x = sine(rate, 1, 8.0, 330.0, lin(-26.0));
        let kick = rate as usize / 2;
        for k in 0..16 {
            for i in 0..1500 {
                x[k * kick + i] += ((-(i as f64) / 250.0).exp() * lin(-8.0) * (2.0 * std::f64::consts::PI * 60.0 * i as f64 / rate as f64).sin()) as f32;
            }
        }
        let g = gain_db(&measure(rate, 1, &x));
        assert!(g > 8.0, "{g}");
        let y = run(rate, 1, g, &x);
        // chord level in 10 ms windows from 40 ms after a kick to the next one, last 4 s
        let mut levels = vec![];
        for k in 8..15 {
            let mut at = k * kick + rate as usize / 25;
            while at + 480 < (k + 1) * kick {
                let w = &y[at..at + 480];
                levels.push(db((w.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / 480.0).sqrt()));
                at += 480;
            }
        }
        let (lo, hi) = levels.iter().fold((f64::MAX, f64::MIN), |(a, b), &l| (a.min(l), b.max(l)));
        assert!(hi - lo < 1.5, "chord level wobbles {:.2} dB", hi - lo);
    }

    #[test]
    fn rumble_goes() {
        let rate = 48_000;
        let x = sine(rate, 1, 2.0, 5.0, 0.5);
        let y = run(rate, 1, 0.0, &x);
        let rms = |v: &[f32]| (v.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        assert!(rms(&y[48_000..]) < rms(&x[48_000..]) * 0.1);
    }
}
