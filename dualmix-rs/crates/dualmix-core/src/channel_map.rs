//! Speaker-position channel mapping (source → output), 1:1 port of C# `DualMix.Audio.ChannelMap`.
//! Matched speakers are copied unchanged (no normalisation); missing speakers are folded down ITU-R BS.775 style.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormatDesc {
    pub channels: usize,
    pub sample_rate: u32,
    pub bits: u16,
    pub valid_bits: u16,
    pub is_float: bool,
    pub channel_mask: u32,
}

impl AudioFormatDesc {
    pub fn default_mask(channels: usize) -> u32 {
        match channels {
            1 => 0x4,
            2 => 0x3,
            3 => 0x7,
            4 => 0x33,
            5 => 0x37,
            6 => 0x3F,
            7 => 0x13F,
            8 => 0x63F,
            _ => 0,
        }
    }

    /// Same format as 32-bit float (the shared-mode mix format of the Windows engine).
    pub fn as_float_mix(self) -> Self {
        Self { bits: 32, valid_bits: 32, is_float: true, ..self }
    }

    pub fn same_layout(&self, o: &Self) -> bool {
        self.channels == o.channels && self.sample_rate == o.sample_rate && self.channel_mask == o.channel_mask
    }
}

const FL: u32 = 0x1;
const FR: u32 = 0x2;
const FC: u32 = 0x4;
const LFE: u32 = 0x8;
const BL: u32 = 0x10;
const BR: u32 = 0x20;
const FLC: u32 = 0x40;
const FRC: u32 = 0x80;
const BC: u32 = 0x100;
const SL: u32 = 0x200;
const SR: u32 = 0x400;

#[derive(Debug, Clone)]
pub struct ChannelMap {
    routes: Vec<(usize, usize, f32)>,
    pub is_identity: bool,
}

fn positions(f: &AudioFormatDesc) -> Vec<u32> {
    // No mask (e.g. a 16-channel "direct out" device): the first channels follow the standard order
    // FL FR FC LFE BL BR SL SR; the rest carry nothing.
    let mask = if f.channel_mask == 0 { AudioFormatDesc::default_mask(f.channels.min(8)) } else { f.channel_mask };
    let mut list = vec![0u32; f.channels];
    let mut n = 0;
    for b in 0..32 {
        if n >= f.channels {
            break;
        }
        let bit = 1u32 << b;
        if mask & bit != 0 {
            list[n] = bit;
            n += 1;
        }
    }
    list
}

impl ChannelMap {
    pub fn create(input: AudioFormatDesc, output: AudioFormatDesc) -> Self {
        let in_pos = positions(&input);
        let out_pos = positions(&output);
        let out_index = |bit: u32| out_pos.iter().position(|&p| p == bit);
        let mut routes: Vec<(usize, usize, f32)> = Vec::new();
        const M3: f32 = 0.707_106_77;

        if input.channels == 1 {
            // Mono: centre if available, otherwise both front speakers at full level (like Windows).
            match out_index(FC) {
                Some(c) if output.channels > 2 => routes.push((c, 0, 1.0)),
                _ => {
                    let l = out_index(FL);
                    let r = out_index(FR);
                    if let Some(l) = l {
                        routes.push((l, 0, 1.0));
                    }
                    if let Some(r) = r {
                        routes.push((r, 0, 1.0));
                    }
                    if l.is_none() && r.is_none() {
                        routes.push((0, 0, 1.0));
                    }
                }
            }
            return Self::build(routes, &input, &output);
        }

        for (i, &bit) in in_pos.iter().enumerate() {
            if bit == 0 {
                continue;
            }
            if let Some(o) = out_index(bit) {
                routes.push((o, i, 1.0));
                continue;
            }
            let mut to = |target: u32, w: f32| {
                if let Some(t) = out_index(target) {
                    routes.push((t, i, w));
                }
            };
            let has = |b: u32| out_pos.contains(&b);
            match bit {
                FC => {
                    to(FL, M3);
                    to(FR, M3);
                }
                LFE => {} // dropped (as Windows does on outputs without a subwoofer)
                BL => {
                    if has(SL) { to(SL, 1.0) } else { to(FL, M3) }
                }
                BR => {
                    if has(SR) { to(SR, 1.0) } else { to(FR, M3) }
                }
                SL => {
                    if has(BL) { to(BL, 1.0) } else { to(FL, M3) }
                }
                SR => {
                    if has(BR) { to(BR, 1.0) } else { to(FR, M3) }
                }
                FLC => to(FL, 1.0),
                FRC => to(FR, 1.0),
                BC => {
                    if has(BL) && has(BR) {
                        to(BL, M3);
                        to(BR, M3);
                    } else if has(SL) && has(SR) {
                        to(SL, M3);
                        to(SR, M3);
                    } else {
                        to(FL, 0.5);
                        to(FR, 0.5);
                    }
                }
                FL | FR => to(FC, 1.0), // mono output
                _ => {
                    to(FL, 0.5);
                    to(FR, 0.5);
                }
            }
        }
        // Positionless outputs (mask 0 / unknown): straight 1:1 copy.
        if routes.is_empty() {
            for i in 0..input.channels.min(output.channels) {
                routes.push((i, i, 1.0));
            }
        }
        Self::build(routes, &input, &output)
    }

    fn build(routes: Vec<(usize, usize, f32)>, input: &AudioFormatDesc, output: &AudioFormatDesc) -> Self {
        let identity = input.channels == output.channels
            && routes.len() == input.channels
            && routes.iter().all(|&(o, i, w)| o == i && w == 1.0);
        Self { routes, is_identity: identity }
    }

    /// Adds the planar source (with a linear gain ramp) into the interleaved mix. Returns the post-gain peak.
    pub fn mix_into(&self, planar: &[Vec<f32>], mix: &mut [f32], frames: usize, out_ch: usize, from_gain: f32, to_gain: f32) -> f32 {
        let mut peak = 0f32;
        let ramp = from_gain != to_gain;
        if !ramp && to_gain == 0.0 {
            return 0.0;
        }
        let step = if ramp && frames > 0 { (to_gain - from_gain) / frames as f32 } else { 0.0 };
        for &(o, i, w) in &self.routes {
            let src = &planar[i];
            if !ramp && to_gain == 1.0 && w == 1.0 {
                // Unity: exact copy-add, no arithmetic on the samples.
                let mut idx = o;
                for &s in &src[..frames] {
                    mix[idx] += s;
                    let a = s.abs();
                    if a > peak {
                        peak = a;
                    }
                    idx += out_ch;
                }
            } else {
                let mut g = from_gain;
                let mut idx = o;
                for &x in &src[..frames] {
                    let s = x * (if ramp { g } else { to_gain }) * w;
                    mix[idx] += s;
                    let a = s.abs();
                    if a > peak {
                        peak = a;
                    }
                    g += step;
                    idx += out_ch;
                }
            }
        }
        peak
    }
}

/// Largest absolute sample value.
#[inline]
pub fn peak(data: &[f32]) -> f32 {
    data.iter().fold(0f32, |p, &s| p.max(s.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_surround_folding() {
        let s51 = AudioFormatDesc { channels: 6, sample_rate: 48000, bits: 32, valid_bits: 32, is_float: true, channel_mask: 0x3F };
        let st = AudioFormatDesc { channels: 2, sample_rate: 48000, bits: 32, valid_bits: 32, is_float: true, channel_mask: 0x3 };
        let s71 = AudioFormatDesc { channels: 8, sample_rate: 48000, bits: 32, valid_bits: 32, is_float: true, channel_mask: 0x63F };

        assert!(ChannelMap::create(s51, s51).is_identity);
        assert!(ChannelMap::create(st, st).is_identity);

        let planar = vec![vec![0.25f32], vec![-0.5f32]];
        let mut mix = vec![0.0f32; 6];
        let p = ChannelMap::create(st, s51).mix_into(&planar, &mut mix, 1, 6, 1.0, 1.0);
        assert_eq!(mix, vec![0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(p, 0.5);

        let p71: Vec<Vec<f32>> = (1..=8).map(|i| vec![0.1 * i as f32]).collect();
        let mut mix51 = vec![0.0f32; 6];
        ChannelMap::create(s71, s51).mix_into(&p71, &mut mix51, 1, 6, 1.0, 1.0);
        assert!(mix51[4] > 0.5 && mix51[5] > 0.6);

        // 5.1 -> stereo: centre folded at -3 dB, LFE dropped.
        let p51: Vec<Vec<f32>> = vec![vec![0.0], vec![0.0], vec![1.0], vec![1.0], vec![0.0], vec![0.0]];
        let mut st_mix = vec![0.0f32; 2];
        ChannelMap::create(s51, st).mix_into(&p51, &mut st_mix, 1, 2, 1.0, 1.0);
        assert!((st_mix[0] - 0.707_106_77).abs() < 1e-6 && (st_mix[1] - 0.707_106_77).abs() < 1e-6);

        // Gain ramp and mute.
        let mut m = vec![0.0f32; 2];
        assert_eq!(ChannelMap::create(st, st).mix_into(&planar, &mut m, 1, 2, 0.0, 0.0), 0.0);
    }
}
