//! Core domain logic for DualMix: zero-allocation audio DSP primitives,
//! settings schema (1:1 compatible with DualMix v2.x JSON), mix math,
//! app classification catalog, knob state machine, localization, and update verification.

pub mod channel_map;
pub mod knob;
pub mod loc;
pub mod log;
pub mod math;
pub mod resampler;
pub mod ringbuf;
pub mod rules;
pub mod settings;
pub mod updater;
