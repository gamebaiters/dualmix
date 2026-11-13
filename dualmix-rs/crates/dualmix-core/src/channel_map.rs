//! Speaker-position channel mapping (Mono, Stereo, Quad, 5.1, 7.1) for planar float buffers.

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
}

#[derive(Debug, Clone)]
pub struct ChannelMap {
    src_channels: usize,
    dst_channels: usize,
    pub is_identity: bool,
    /// Matrix entries `weights[dst_ch][src_ch]` stored flattened (`dst * src_channels + src`).
    weights: Vec<f32>,
}

fn speaker_bits(channels: usize, mask: u32) -> Vec<u32> {
    let m = if mask == 0 { AudioFormatDesc::default_mask(channels) } else { mask };
    let mut bits = Vec::with_capacity(channels);
    for bit in 0..32u32 {
        let flag = 1u32 << bit;
        if (m & flag) != 0 {
            bits.push(flag);
            if bits.len() == channels {
                break;
            }
        }
    }
    while bits.len() < channels {
        bits.push(0);
    }
    bits
}

const FL: u32 = 0x1;
const FR: u32 = 0x2;
const FC: u32 = 0x4;
const BL: u32 = 0x10;
const BR: u32 = 0x20;
const SL: u32 = 0x200;
const SR: u32 = 0x400;

impl ChannelMap {
    pub fn create(src: AudioFormatDesc, dst: AudioFormatDesc) -> Self {
        let s_bits = speaker_bits(src.channels, src.channel_mask);
        let d_bits = speaker_bits(dst.channels, dst.channel_mask);
        if src.channels == dst.channels && s_bits == d_bits {
            let mut weights = vec![0.0f32; dst.channels * src.channels];
            for c in 0..src.channels {
                weights[c * src.channels + c] = 1.0;
            }
            return Self {
                src_channels: src.channels,
                dst_channels: dst.channels,
                is_identity: true,
                weights,
            };
        }

        let mut weights = vec![0.0f32; dst.channels * src.channels];
        let set_w = |w: &mut [f32], d: usize, s: usize, val: f32| {
            w[d * src.channels + s] = val;
        };

        let dst_idx = |bit: u32| -> Option<usize> { d_bits.iter().position(|&b| b == bit) };

        for (si, &sbit) in s_bits.iter().enumerate() {
            if let Some(di) = dst_idx(sbit) {
                set_w(&mut weights, di, si, 1.0);
                continue;
            }
            match sbit {
                // Mono / center fallback to FL + FR at -3 dB if no center speaker
                FC => {
                    if let (Some(fl), Some(fr)) = (dst_idx(FL), dst_idx(FR)) {
                        let g = std::f32::consts::FRAC_1_SQRT_2;
                        set_w(&mut weights, fl, si, g);
                        set_w(&mut weights, fr, si, g);
                    }
                }
                // Side L/R fold into Back L/R (e.g. 7.1 -> 5.1) or Front L/R (-> stereo)
                SL => {
                    if let Some(bl) = dst_idx(BL) {
                        set_w(&mut weights, bl, si, 1.0);
                    } else if let Some(fl) = dst_idx(FL) {
                        set_w(&mut weights, fl, si, std::f32::consts::FRAC_1_SQRT_2);
                    }
                }
                SR => {
                    if let Some(br) = dst_idx(BR) {
                        set_w(&mut weights, br, si, 1.0);
                    } else if let Some(fr) = dst_idx(FR) {
                        set_w(&mut weights, fr, si, std::f32::consts::FRAC_1_SQRT_2);
                    }
                }
                BL => {
                    if let Some(sl) = dst_idx(SL) {
                        set_w(&mut weights, sl, si, 1.0);
                    } else if let Some(fl) = dst_idx(FL) {
                        set_w(&mut weights, fl, si, std::f32::consts::FRAC_1_SQRT_2);
                    }
                }
                BR => {
                    if let Some(sr) = dst_idx(SR) {
                        set_w(&mut weights, sr, si, 1.0);
                    } else if let Some(fr) = dst_idx(FR) {
                        set_w(&mut weights, fr, si, std::f32::consts::FRAC_1_SQRT_2);
                    }
                }
                _ => {}
            }
        }

        Self {
            src_channels: src.channels,
            dst_channels: dst.channels,
            is_identity: false,
            weights,
        }
    }

    /// Mixes `frames` planar source samples into interleaved output buffer with smooth linear gain ramp.
    pub fn mix_into(
        &self,
        planar_src: &[Vec<f32>],
        interleaved_dst: &mut [f32],
        frames: usize,
        gain_start: f32,
        gain_end: f32,
    ) {
        let step = if frames > 1 {
            (gain_end - gain_start) / frames as f32
        } else {
            0.0
        };
        if self.is_identity {
            let ch = self.dst_channels;
            for f in 0..frames {
                let g = gain_start + step * f as f32;
                let base = f * ch;
                for c in 0..ch {
                    interleaved_dst[base + c] += planar_src[c][f] * g;
                }
            }
            return;
        }
        let dch = self.dst_channels;
        let sch = self.src_channels;
        for f in 0..frames {
            let g = gain_start + step * f as f32;
            let base = f * dch;
            for dc in 0..dch {
                let row = &self.weights[dc * sch..(dc + 1) * sch];
                let mut acc = 0.0f32;
                for sc in 0..sch {
                    let w = row[sc];
                    if w != 0.0 {
                        acc += planar_src[sc][f] * w;
                    }
                }
                interleaved_dst[base + dc] += acc * g;
            }
        }
    }
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
        ChannelMap::create(st, s51).mix_into(&planar, &mut mix, 1, 1.0, 1.0);
        assert_eq!(mix, vec![0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);

        let p71: Vec<Vec<f32>> = (1..=8).map(|i| vec![0.1 * i as f32]).collect();
        let mut mix51 = vec![0.0f32; 6];
        ChannelMap::create(s71, s51).mix_into(&p71, &mut mix51, 1, 1.0, 1.0);
        assert!(mix51[4] > 0.5 && mix51[5] > 0.6);
    }
}
