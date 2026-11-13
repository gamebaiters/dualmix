//! Plain description of a PCM / float wave format (WAVEFORMATEXTENSIBLE), port of C# `AudioFormat`.

use dualmix_core::channel_map::AudioFormatDesc;
use dualmix_core::loc;

pub type AudioFormat = AudioFormatDesc;

const SUBTYPE_TAIL: [u8; 12] = [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];

/// "48 kHz · 24-bit · 5.1" (localized).
pub fn describe(f: &AudioFormat) -> String {
    let bits = if f.is_float { loc::get("Format.BitsFloat") } else { loc::fmt("Format.BitsInt", &[&f.valid_bits.to_string()]) };
    let khz = f.sample_rate as f64 / 1000.0;
    let rate = if (khz - khz.round()).abs() < 1e-9 { format!("{}", khz.round() as i64) } else { format!("{khz:.1}") };
    let rate = if loc::is_italian() { rate.replace('.', ",") } else { rate };
    format!("{rate} kHz · {bits} · {}", layout_name(f.channels, f.channel_mask))
}

pub fn layout_name(channels: usize, _mask: u32) -> String {
    match channels {
        1 => loc::get("Layout.Mono"),
        2 => loc::get("Layout.Stereo"),
        4 => loc::get("Layout.Quad"),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => loc::fmt("Layout.Channels", &[&n.to_string()]),
    }
}

/// Serialises to a WAVEFORMATEXTENSIBLE (40 bytes).
pub fn to_bytes(f: &AudioFormat) -> [u8; 40] {
    let mut b = [0u8; 40];
    let block = (f.channels as u32 * f.bits as u32 / 8) as u16;
    b[0..2].copy_from_slice(&0xFFFEu16.to_le_bytes());
    b[2..4].copy_from_slice(&(f.channels as u16).to_le_bytes());
    b[4..8].copy_from_slice(&f.sample_rate.to_le_bytes());
    b[8..12].copy_from_slice(&(f.sample_rate * block as u32).to_le_bytes());
    b[12..14].copy_from_slice(&block.to_le_bytes());
    b[14..16].copy_from_slice(&f.bits.to_le_bytes());
    b[16..18].copy_from_slice(&22u16.to_le_bytes());
    b[18..20].copy_from_slice(&f.valid_bits.to_le_bytes());
    let mask = if f.channel_mask == 0 { AudioFormatDesc::default_mask(f.channels) } else { f.channel_mask };
    b[20..24].copy_from_slice(&mask.to_le_bytes());
    let sub: u32 = if f.is_float { 3 } else { 1 };
    b[24..28].copy_from_slice(&sub.to_le_bytes());
    b[28..40].copy_from_slice(&SUBTYPE_TAIL);
    b
}

pub fn from_bytes(b: &[u8]) -> Option<AudioFormat> {
    if b.len() < 16 {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let tag = u16_at(0);
    let ch = u16_at(2) as usize;
    let rate = u32_at(4);
    let bits = u16_at(14);
    if tag == 0xFFFE && b.len() >= 40 {
        let valid = u16_at(18);
        let mask = u32_at(20);
        let sub = u32_at(24);
        return Some(AudioFormat {
            channels: ch,
            sample_rate: rate,
            bits,
            valid_bits: if valid == 0 { bits } else { valid },
            is_float: sub == 3,
            channel_mask: if mask == 0 { AudioFormatDesc::default_mask(ch) } else { mask },
        });
    }
    Some(AudioFormat { channels: ch, sample_rate: rate, bits, valid_bits: bits, is_float: tag == 3, channel_mask: AudioFormatDesc::default_mask(ch) })
}

/// Reads a WAVEFORMATEX(TENSIBLE) from memory.
///
/// # Safety
/// `p` must point to a valid WAVEFORMATEX structure (or be null).
pub unsafe fn from_ptr(p: *const u8) -> Option<AudioFormat> {
    if p.is_null() {
        return None;
    }
    unsafe {
        let tag = u16::from_le_bytes([*p, *p.add(1)]);
        let cb = u16::from_le_bytes([*p.add(16), *p.add(17)]) as usize;
        let size = if tag == 0xFFFE { 18 + cb.max(22) } else { 18 };
        let slice = std::slice::from_raw_parts(p, size);
        from_bytes(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_extensible() {
        let f = AudioFormat { channels: 6, sample_rate: 48000, bits: 32, valid_bits: 24, is_float: false, channel_mask: 0x3F };
        let b = to_bytes(&f);
        assert_eq!(from_bytes(&b), Some(f));
        let g = f.as_float_mix();
        assert_eq!(from_bytes(&to_bytes(&g)), Some(g));
        loc::set_language(dualmix_core::settings::LanguagePreference::English);
        assert_eq!(describe(&f), "48 kHz · 24-bit · 5.1");
    }
}
