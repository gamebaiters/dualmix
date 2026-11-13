//! Low-latency native Windows Core Audio + WASAPI audio router & session controls.

pub mod devices;
pub mod process_tuning;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use dualmix_core::settings::LatencyMode;

/// Latency profile parameters matching `DualMix.Audio.LatencyProfile`.
#[derive(Debug, Clone, Copy)]
pub struct LatencyProfile {
    pub source_packets: f64,
    pub source_margin_ms: u32,
    pub padding_periods: f64,
    pub buffer_ms: u32,
}

impl LatencyProfile {
    pub const fn for_mode(m: LatencyMode) -> Self {
        match m {
            LatencyMode::Minima => Self { source_packets: 1.0, source_margin_ms: 2, padding_periods: 1.5, buffer_ms: 100 },
            LatencyMode::Bilanciata => Self { source_packets: 2.0, source_margin_ms: 4, padding_periods: 2.0, buffer_ms: 120 },
            LatencyMode::Sicura => Self { source_packets: 3.0, source_margin_ms: 10, padding_periods: 4.0, buffer_ms: 160 },
        }
    }
}

/// Lock-free atomic state shared between the UI thread and the MMCSS `"Pro Audio"` thread.
#[derive(Debug)]
pub struct SharedRouterMetrics {
    pub game_gain_bits: AtomicU32,
    pub chat_gain_bits: AtomicU32,
    pub underruns: AtomicU64,
    pub running: AtomicBool,
}

impl SharedRouterMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            game_gain_bits: AtomicU32::new(1.0f32.to_bits()),
            chat_gain_bits: AtomicU32::new(1.0f32.to_bits()),
            underruns: AtomicU64::new(0),
            running: AtomicBool::new(false),
        })
    }

    #[inline]
    pub fn set_gains(&self, game: f32, chat: f32) {
        self.game_gain_bits.store(game.to_bits(), Ordering::Release);
        self.chat_gain_bits.store(chat.to_bits(), Ordering::Release);
    }

    #[inline]
    pub fn gains(&self) -> (f32, f32) {
        (
            f32::from_bits(self.game_gain_bits.load(Ordering::Acquire)),
            f32::from_bits(self.chat_gain_bits.load(Ordering::Acquire)),
        )
    }
}
