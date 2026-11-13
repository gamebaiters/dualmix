//! Perceptual taper, ChatMix balance curve, and decibel formatting.

use crate::settings::{Channel, Settings};

/// Linear amplitude gains applied to Game and Chat channels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelGains {
    pub game: f32,
    pub chat: f32,
}

impl ChannelGains {
    #[inline]
    pub fn for_channel(self, ch: Channel) -> f32 {
        match ch {
            Channel::Game => self.game,
            Channel::Chat => self.chat,
            Channel::Ignore => 1.0,
        }
    }
}

/// Perceptual square-law taper (50% ≈ -12 dB, 0% = digital silence, 100% = bit-exact 1.0).
#[inline]
pub fn taper(position: f64) -> f64 {
    let p = position.clamp(0.0, 1.0);
    p * p
}

/// SteelSeries-style ChatMix balance curve returning slider-scale `(game, chat)` in `0.0..=1.0`.
#[inline]
pub fn balance(mix: f64, floor: f64) -> (f64, f64) {
    let mix = mix.clamp(-1.0, 1.0);
    let floor = floor.clamp(0.0, 1.0);
    let game = if mix <= 0.0 { 1.0 } else { 1.0 - mix * (1.0 - floor) };
    let chat = if mix >= 0.0 { 1.0 } else { 1.0 + mix * (1.0 - floor) };
    (game, chat)
}

/// Computes linear amplitude gains from active mixer settings.
#[inline]
pub fn compute(s: &Settings) -> ChannelGains {
    let (g, c) = balance(s.mix, s.mix_floor);
    let game = if s.game_muted { 0.0 } else { taper(s.game_volume * g) };
    let chat = if s.chat_muted { 0.0 } else { taper(s.chat_volume * c) };
    ChannelGains {
        game: game as f32,
        chat: chat as f32,
    }
}

/// Returns perceived levels (`0.0..=1.0` slider scale) for UI display.
#[inline]
pub fn levels(s: &Settings) -> (f64, f64) {
    let (g, c) = balance(s.mix, s.mix_floor);
    let game = if s.game_muted { 0.0 } else { s.game_volume * g };
    let chat = if s.chat_muted { 0.0 } else { s.chat_volume * c };
    (game, chat)
}

/// Snaps mix values very close to center to `0.0` (tactile detent).
#[inline]
pub fn snap_center(mix: f64) -> f64 {
    if mix.abs() < 0.005 {
        0.0
    } else {
        (mix * 1000.0).round() / 1000.0
    }
}

pub fn percent(v: f64) -> String {
    format!("{}%", (v * 100.0).round() as i32)
}

pub fn decibels(amplitude: f64) -> String {
    if amplitude <= 0.00001 {
        return "−∞ dB".to_string();
    }
    let db = 20.0 * amplitude.log10();
    if db.abs() < 0.05 {
        "0 dB".to_string()
    } else if db > 0.0 {
        format!("+{db:.1} dB")
    } else {
        format!("−{:.1} dB", -db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn balance_extremes_and_floor() {
        let c = balance(0.0, 0.0);
        assert!(near(c.0, 1.0) && near(c.1, 1.0));

        let g = balance(-1.0, 0.0);
        assert!(near(g.0, 1.0) && near(g.1, 0.0));

        let ch = balance(1.0, 0.0);
        assert!(near(ch.0, 0.0) && near(ch.1, 1.0));

        let fl = balance(-1.0, 0.25);
        assert!(near(fl.0, 1.0) && near(fl.1, 0.25));
    }

    #[test]
    fn taper_and_compute_sanity() {
        assert!(near(taper(1.0), 1.0));
        assert!(near(taper(0.5), 0.25));
        assert!(near(taper(0.0), 0.0));

        let mut s = Settings::default();
        s.game_muted = true;
        let gains = compute(&s);
        assert_eq!(gains.game, 0.0);
        assert_eq!(gains.chat, 1.0);
        assert_eq!(snap_center(0.002), 0.0);
        assert_eq!(snap_center(-0.5), -0.5);
    }
}
