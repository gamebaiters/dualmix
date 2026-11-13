//! Low-latency native Windows Core Audio + WASAPI engine: virtual-device router, per-app session engine,
//! endpoint configuration (IPolicyConfig), per-app routing (AudioPolicyConfig) and driver setup.

pub mod process_tuning;

#[cfg(windows)]
pub mod app_routing;
#[cfg(windows)]
pub mod com;
#[cfg(windows)]
pub mod devices;
#[cfg(windows)]
pub mod driver_setup;
#[cfg(windows)]
pub mod endpoint_volume;
pub mod format;
#[cfg(windows)]
pub mod process_info;
#[cfg(windows)]
pub mod router;
#[cfg(windows)]
pub mod sessions;

use dualmix_core::settings::LatencyMode;

/// Buffering used by the router for a latency preset (port of C# `LatencyProfile`).
#[derive(Debug, Clone, Copy)]
pub struct LatencyProfile {
    /// Capture packets kept in each source buffer on top of the block being played.
    pub source_packets: f64,
    /// Extra safety margin in each source buffer.
    pub source_margin_ms: u32,
    /// Audio kept queued in the output buffer, in device periods (≥ 1).
    pub padding_periods: f64,
    /// Output buffer capacity to ask Windows for.
    pub buffer_ms: u32,
    /// Output queue added on top, learned from past dropouts on this PC.
    pub extra_queue_ms: f64,
    /// Source reserve added on top, learned from past dropouts on this PC.
    pub extra_source_ms: f64,
}

impl LatencyProfile {
    pub const fn for_mode(m: LatencyMode) -> Self {
        match m {
            LatencyMode::Minima => Self { source_packets: 1.0, source_margin_ms: 2, padding_periods: 1.5, buffer_ms: 100, extra_queue_ms: 0.0, extra_source_ms: 0.0 },
            LatencyMode::Bilanciata => Self { source_packets: 2.0, source_margin_ms: 4, padding_periods: 2.0, buffer_ms: 120, extra_queue_ms: 0.0, extra_source_ms: 0.0 },
            LatencyMode::Sicura => Self { source_packets: 3.0, source_margin_ms: 10, padding_periods: 4.0, buffer_ms: 160, extra_queue_ms: 0.0, extra_source_ms: 0.0 },
        }
    }
}
