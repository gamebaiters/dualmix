//! High-quality streaming resampler: polyphase Kaiser-windowed sinc, 160 taps,
//! 2048 interpolated phases (Kaiser β = 14 ≈ -135 dB stopband, bit-exact at ratio 1:1).
//! Zero allocations on `read`/`advance`; planar internal storage for SIMD-friendly dot products.

use std::f64::consts::PI;

pub const TAPS: usize = 160;
const HALF: usize = TAPS / 2;
const PHASES: usize = 2048;
const KAISER_BETA: f64 = 14.0;
const BASE_CUTOFF: f64 = 0.472;

#[derive(Debug, Clone)]
pub struct SincResampler {
    channels: usize,
    cutoff: f64,
    table: Vec<f32>,      // (PHASES + 1) * TAPS
    coef: [f32; TAPS],
    buf: Vec<Vec<f32>>,   // per-channel planar history
    len: usize,
    pos: f64,
}

impl SincResampler {
    pub const FILTER_DELAY: usize = HALF;

    pub fn new(channels: usize, max_step: f64) -> Self {
        let channels = channels.max(1);
        let cutoff = BASE_CUTOFF * (1.0 / max_step.max(1e-6)).min(1.0);
        let table = build_table(cutoff);
        let buf = (0..channels).map(|_| vec![0.0f32; 65536]).collect();
        let mut r = Self {
            channels,
            cutoff,
            table,
            coef: [0.0; TAPS],
            buf,
            len: 0,
            pos: 0.0,
        };
        r.reset();
        r
    }

    #[inline]
    pub fn channels(&self) -> usize {
        self.channels
    }

    #[inline]
    pub fn cutoff(&self) -> f64 {
        self.cutoff
    }

    #[inline]
    pub fn pending(&self) -> f64 {
        self.len as f64 - (TAPS - 1) as f64 - self.pos
    }

    #[inline]
    pub fn buffered(&self) -> usize {
        self.len
    }

    pub fn reset(&mut self) {
        self.len = HALF - 1;
        for ch in &mut self.buf {
            ch[..TAPS].fill(0.0);
        }
        self.pos = 0.0;
    }

    pub fn skip(&mut self, frames: usize) {
        let avail = self.pending().max(0.0) as usize;
        let f = frames.min(avail);
        self.pos += f as f64;
        self.compact();
    }

    pub fn write_interleaved(&mut self, interleaved: &[f32], frames: usize) {
        self.ensure_capacity(self.len + frames);
        let ch = self.channels;
        debug_assert!(interleaved.len() >= frames * ch);
        for c in 0..ch {
            let dst = &mut self.buf[c][self.len..self.len + frames];
            let mut idx = c;
            for d in dst.iter_mut() {
                *d = interleaved[idx];
                idx += ch;
            }
        }
        self.len += frames;
    }

    pub fn write_silence(&mut self, frames: usize) {
        self.ensure_capacity(self.len + frames);
        for ch in &mut self.buf {
            ch[self.len..self.len + frames].fill(0.0);
        }
        self.len += frames;
    }

    /// Produces up to `frames` planar output samples (`output[channel][..frames]`).
    pub fn read(&mut self, output: &mut [Vec<f32>], frames: usize, step: f64) -> usize {
        let mut produced = 0usize;
        let ch = self.channels;
        while produced < frames {
            let p = self.pos;
            let start = p as usize;
            if start + TAPS > self.len {
                break;
            }
            let frac = p - start as f64;
            if frac == 0.0 {
                // Exact impulse alignment: bit-identical copy.
                for c in 0..ch {
                    output[c][produced] = self.buf[c][start + HALF - 1];
                }
            } else {
                let ph = frac * PHASES as f64;
                let p0 = ph as usize;
                let t = (ph - p0 as f64) as f32;
                interpolate_row(&self.table, p0 * TAPS, t, &mut self.coef);
                for c in 0..ch {
                    output[c][produced] = dot_160(&self.buf[c][start..start + TAPS], &self.coef);
                }
            }
            produced += 1;
            self.pos = p + step;
        }
        self.compact();
        produced
    }

    pub fn advance(&mut self, frames: usize, step: f64) -> usize {
        let mut produced = 0usize;
        while produced < frames {
            if (self.pos as usize) + TAPS > self.len {
                break;
            }
            produced += 1;
            self.pos += step;
        }
        self.compact();
        produced
    }

    fn compact(&mut self) {
        let drop = self.pos as usize;
        if drop < 16384 {
            return;
        }
        let keep = self.len - drop;
        for ch in &mut self.buf {
            ch.copy_within(drop..drop + keep, 0);
        }
        self.len = keep;
        self.pos -= drop as f64;
    }

    fn ensure_capacity(&mut self, needed: usize) {
        let cur = self.buf[0].len();
        if cur >= needed {
            return;
        }
        let next = needed.max(cur * 2);
        for ch in &mut self.buf {
            ch.resize(next, 0.0);
        }
    }
}

#[inline]
fn interpolate_row(table: &[f32], row: usize, t: f32, dst: &mut [f32; TAPS]) {
    let a = &table[row..row + TAPS];
    let b = &table[row + TAPS..row + 2 * TAPS];
    for i in 0..TAPS {
        dst[i] = a[i] + (b[i] - a[i]) * t;
    }
}

#[inline]
fn dot_160(x: &[f32], c: &[f32; TAPS]) -> f32 {
    // Unrolled 8-way accumulator so LLVM emits wide FMA/AVX instructions cleanly.
    let mut s0 = 0.0f32;
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    let mut s3 = 0.0f32;
    let mut s4 = 0.0f32;
    let mut s5 = 0.0f32;
    let mut s6 = 0.0f32;
    let mut s7 = 0.0f32;
    let x = &x[..TAPS];
    let mut i = 0usize;
    while i < TAPS {
        s0 += x[i] * c[i];
        s1 += x[i + 1] * c[i + 1];
        s2 += x[i + 2] * c[i + 2];
        s3 += x[i + 3] * c[i + 3];
        s4 += x[i + 4] * c[i + 4];
        s5 += x[i + 5] * c[i + 5];
        s6 += x[i + 6] * c[i + 6];
        s7 += x[i + 7] * c[i + 7];
        i += 8;
    }
    ((s0 + s4) + (s1 + s5)) + ((s2 + s6) + (s3 + s7))
}

fn build_table(fc: f64) -> Vec<f32> {
    let mut table = vec![0.0f32; (PHASES + 1) * TAPS];
    let i0_beta = bessel_i0(KAISER_BETA);
    let mut row = [0.0f64; TAPS];
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let mut sum = 0.0f64;
        for (k, slot) in row.iter_mut().enumerate() {
            let u = k as f64 - (HALF as f64 - 1.0) - frac;
            let sinc = if u == 0.0 {
                2.0 * fc
            } else {
                (2.0 * PI * fc * u).sin() / (PI * u)
            };
            let r = u / HALF as f64;
            let w = if r.abs() >= 1.0 {
                0.0
            } else {
                bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta
            };
            *slot = sinc * w;
            sum += *slot;
        }
        let inv = if sum.abs() > 1e-12 { 1.0 / sum } else { 1.0 };
        let base = p * TAPS;
        for k in 0..TAPS {
            table[base + k] = (row[k] * inv) as f32;
        }
    }
    table
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x2 = 0.25 * x * x;
    for k in 1..=64 {
        let kf = k as f64;
        term *= half_x2 / (kf * kf);
        sum += term;
        if term < sum * 1e-16 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_snr_across_frequencies_and_drift() {
        const RATE: usize = 48_000;
        let cases = [
            (997.0, 1.00002),
            (997.0, 0.9998),
            (10_000.0, 1.00005),
            (15_000.0, 1.0002),
            (20_000.0, 0.9997),
        ];
        for (freq, step) in cases {
            let mut r = SincResampler::new(1, 1.01);
            let total = RATE * 2;
            let input: Vec<f32> = (0..total)
                .map(|i| (0.5 * (2.0 * PI * freq * i as f64 / RATE as f64).sin()) as f32)
                .collect();
            let mut out = vec![vec![0.0f32; 512]];
            let (mut err_sq, mut sig_sq) = (0.0f64, 0.0f64);
            let (mut n, mut fed) = (0usize, 0usize);
            while fed < total {
                let chunk = (480).min(total - fed);
                r.write_interleaved(&input[fed..fed + chunk], chunk);
                fed += chunk;
                let got = r.read(&mut out, 480, step);
                for &sample in &out[0][..got] {
                    let pos = n as f64 * step;
                    if n >= 2000 && pos <= (total - 200) as f64 {
                        let ideal = 0.5 * (2.0 * PI * freq * pos / RATE as f64).sin();
                        let e = sample as f64 - ideal;
                        err_sq += e * e;
                        sig_sq += ideal * ideal;
                    }
                    n += 1;
                }
            }
            let snr = 10.0 * (sig_sq / err_sq.max(1e-30)).log10();
            assert!(snr > 120.0, "freq {freq} step {step}: SNR {snr:.2} dB <= 120 dB");
        }
    }

    #[test]
    fn unity_ratio_is_bit_exact() {
        let mut r = SincResampler::new(2, 1.01);
        let mut data = vec![0.0f32; 2000];
        let mut x = 1234567u32;
        for d in &mut data {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *d = (x as f32 / u32::MAX as f32) * 2.0 - 1.0;
        }
        r.write_interleaved(&data, 1000);
        let mut out = vec![vec![0.0f32; 1000], vec![0.0f32; 1000]];
        let produced = r.read(&mut out, 1000, 1.0);
        assert!(produced > 800);
        for i in 0..produced {
            assert_eq!(out[0][i], data[2 * i]);
            assert_eq!(out[1][i], data[2 * i + 1]);
        }
    }
}
