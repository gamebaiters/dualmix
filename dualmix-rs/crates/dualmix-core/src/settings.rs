//! DualMix settings schema — JSON-compatible with v2.x C# `Settings`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Channel {
    Game,
    Chat,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineMode {
    PerApp,
    VirtualDevices,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KnobTarget {
    Mix,
    Volume,
    Game,
    Chat,
}

/// Bitmask matching `DualMix.Core.HotkeyModifiers` (serializes cleanly as integer or flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HotkeyModifiers(pub u32);

impl HotkeyModifiers {
    pub const NONE: Self = Self(0);
    pub const CTRL: Self = Self(1);
    pub const ALT: Self = Self(2);
    pub const SHIFT: Self = Self(4);
    pub const WIN: Self = Self(8);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl Serialize for HotkeyModifiers {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut parts = Vec::new();
        if self.contains(Self::CTRL) { parts.push("Ctrl"); }
        if self.contains(Self::ALT) { parts.push("Alt"); }
        if self.contains(Self::SHIFT) { parts.push("Shift"); }
        if self.contains(Self::WIN) { parts.push("Win"); }
        if parts.is_empty() {
            s.serialize_str("None")
        } else {
            s.serialize_str(&parts.join(", "))
        }
    }
}

impl<'de> Deserialize<'de> for HotkeyModifiers {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = HotkeyModifiers;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("HotkeyModifiers flags string or integer")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(HotkeyModifiers(v as u32))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(HotkeyModifiers(v as u32))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let mut mask = 0u32;
                for tok in v.split(',') {
                    match tok.trim().to_ascii_lowercase().as_str() {
                        "" | "none" | "0" => {}
                        "ctrl" | "control" => mask |= 1,
                        "alt" => mask |= 2,
                        "shift" => mask |= 4,
                        "win" => mask |= 8,
                        other => {
                            if let Ok(n) = other.parse::<u32>() {
                                mask |= n;
                            }
                        }
                    }
                }
                Ok(HotkeyModifiers(mask))
            }
        }
        d.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnobStartFunction {
    Last,
    Mix,
    Volume,
    Game,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateMode {
    Off,
    Notify,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LatencyMode {
    Minima,
    Bilanciata,
    Sicura,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguagePreference {
    Auto,
    English,
    Italian,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AppRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<Channel>,
    #[serde(default = "one")]
    pub volume: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

impl Default for AppRule {
    fn default() -> Self {
        Self {
            channel: None,
            volume: 1.0,
            display_name: None,
            exe_path: None,
            last_seen: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DefaultDevicesBackup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multimedia: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub communications: Option<String>,
}

const fn one() -> f64 { 1.0 }
const fn default_true() -> bool { true }
const fn default_version() -> i32 { 2 }
const fn default_hotkey_key() -> i32 { 0x4D }
const fn default_knob_step() -> f64 { 0.05 }
const fn default_mode() -> EngineMode { EngineMode::VirtualDevices }
const fn default_game_ch() -> Channel { Channel::Game }
const fn default_latency() -> LatencyMode { LatencyMode::Bilanciata }
const fn default_knob_target() -> KnobTarget { KnobTarget::Volume }
const fn default_knob_start() -> KnobStartFunction { KnobStartFunction::Volume }
const fn default_theme() -> ThemePreference { ThemePreference::System }
const fn default_lang() -> LanguagePreference { LanguagePreference::Auto }
const fn default_update() -> UpdateMode { UpdateMode::Auto }
const fn default_hotkey_mods() -> HotkeyModifiers { HotkeyModifiers(1 | 2) }
fn default_knob_cycle() -> Vec<KnobTarget> { vec![KnobTarget::Mix, KnobTarget::Volume] }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    pub version: i32,
    pub mode: EngineMode,
    pub mix: f64,
    pub game_volume: f64,
    pub chat_volume: f64,
    pub game_muted: bool,
    pub chat_muted: bool,
    pub mix_floor: f64,

    pub apps: HashMap<String, AppRule>,
    pub default_channel: Channel,
    pub system_sounds_channel: Channel,
    pub extra_chat_apps: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device_id: Option<String>,
    pub output_chosen_by_user: bool,
    pub game_direct: bool,
    pub set_windows_defaults: bool,
    pub route_apps_to_devices: bool,
    pub latency: LatencyMode,
    pub learned_queue_ms: f64,
    pub learned_source_ms: f64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults_backup: Option<DefaultDevicesBackup>,
    pub routed_apps: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevated_config_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_setup_declined_at: Option<String>,

    pub knob_enabled: bool,
    pub knob_target: KnobTarget,
    pub knob_start_function: KnobStartFunction,
    pub knob_mix_press_mutes: bool,
    pub knob_cycle: Vec<KnobTarget>,
    pub knob_hotkey_key: i32,
    pub knob_hotkey_modifiers: HotkeyModifiers,
    pub knob_hotkey_custom: bool,
    pub knob_ctrl_alternate: bool,
    pub knob_ctrl_press_cycles: bool,
    pub knob_double_press_cycles: bool,
    pub knob_step: f64,
    pub knob_invert: bool,
    pub accept_injected_keys: bool,
    pub key_mix_game: i32,
    pub key_mix_chat: i32,
    pub key_mix_reset: i32,

    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub show_osd: bool,
    pub restore_volumes_on_exit: bool,
    pub theme: ThemePreference,
    pub language: LanguagePreference,
    pub first_run_done: bool,
    pub tray_hint_shown: bool,

    pub update_mode: UpdateMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_feed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_next_check: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: default_version(),
            mode: default_mode(),
            mix: 0.0,
            game_volume: 1.0,
            chat_volume: 1.0,
            game_muted: false,
            chat_muted: false,
            mix_floor: 0.0,
            apps: HashMap::new(),
            default_channel: default_game_ch(),
            system_sounds_channel: default_game_ch(),
            extra_chat_apps: Vec::new(),
            game_device_id: None,
            chat_device_id: None,
            output_device_id: None,
            output_chosen_by_user: false,
            game_direct: false,
            set_windows_defaults: default_true(),
            route_apps_to_devices: default_true(),
            latency: default_latency(),
            learned_queue_ms: 0.0,
            learned_source_ms: 0.0,
            defaults_backup: None,
            routed_apps: Vec::new(),
            elevated_config_at: None,
            driver_setup_declined_at: None,
            knob_enabled: default_true(),
            knob_target: default_knob_target(),
            knob_start_function: default_knob_start(),
            knob_mix_press_mutes: default_true(),
            knob_cycle: default_knob_cycle(),
            knob_hotkey_key: default_hotkey_key(),
            knob_hotkey_modifiers: default_hotkey_mods(),
            knob_hotkey_custom: false,
            knob_ctrl_alternate: default_true(),
            knob_ctrl_press_cycles: default_true(),
            knob_double_press_cycles: default_true(),
            knob_step: default_knob_step(),
            knob_invert: false,
            accept_injected_keys: false,
            key_mix_game: 0,
            key_mix_chat: 0,
            key_mix_reset: 0,
            start_with_windows: default_true(),
            start_minimized: default_true(),
            show_osd: default_true(),
            restore_volumes_on_exit: default_true(),
            theme: default_theme(),
            language: default_lang(),
            first_run_done: false,
            tray_hint_shown: false,
            update_mode: default_update(),
            update_feed: None,
            update_next_check: None,
        }
    }
}

fn clamp_nan(v: f64, lo: f64, hi: f64) -> f64 {
    if v.is_nan() { lo } else { v.clamp(lo, hi) }
}

impl Settings {
    pub fn start_target(&self) -> Option<KnobTarget> {
        match self.knob_start_function {
            KnobStartFunction::Mix => Some(KnobTarget::Mix),
            KnobStartFunction::Volume => Some(KnobTarget::Volume),
            KnobStartFunction::Game => Some(KnobTarget::Game),
            KnobStartFunction::Chat => Some(KnobTarget::Chat),
            KnobStartFunction::Last => None,
        }
    }

    pub fn normalize(&mut self) -> &mut Self {
        let mut dedup = Vec::with_capacity(self.knob_cycle.len().max(2));
        for &t in &self.knob_cycle {
            if !dedup.contains(&t) {
                dedup.push(t);
            }
        }
        if dedup.is_empty() {
            dedup.extend_from_slice(&[KnobTarget::Mix, KnobTarget::Volume]);
        }
        self.knob_cycle = dedup;
        self.mix = clamp_nan(self.mix, -1.0, 1.0);
        self.game_volume = clamp_nan(self.game_volume, 0.0, 1.0);
        self.chat_volume = clamp_nan(self.chat_volume, 0.0, 1.0);
        self.mix_floor = clamp_nan(self.mix_floor, 0.0, 0.9);
        self.knob_step = clamp_nan(self.knob_step, 0.01, 0.25);
        if self.update_feed.as_deref().is_some_and(|f| f.trim().is_empty()) {
            self.update_feed = None;
        }
        for rule in self.apps.values_mut() {
            rule.volume = clamp_nan(rule.volume, 0.0, 1.0);
        }
        self
    }

    pub fn migrate(&mut self, had_version_field: bool) -> &mut Self {
        if !had_version_field || self.version < 2 {
            self.mode = EngineMode::VirtualDevices;
            self.knob_enabled = true;
            self.knob_target = KnobTarget::Mix;
            self.knob_cycle = vec![KnobTarget::Mix, KnobTarget::Volume];
            self.knob_hotkey_key = 0x4D;
            self.knob_hotkey_modifiers = HotkeyModifiers::CTRL.union(HotkeyModifiers::ALT);
            self.system_sounds_channel = Channel::Game;
            self.output_chosen_by_user = false;
            self.version = 2;
        }
        self
    }

    pub fn from_json_str(json: &str) -> Result<Self, serde_json::Error> {
        let mut s: Self = serde_json::from_str(json)?;
        let has_version = json.contains("\"Version\"");
        s.migrate(has_version);
        s.normalize();
        Ok(s)
    }

    pub fn load_file(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(raw) => match Self::from_json_str(&raw) {
                Ok(s) => s,
                Err(_) => {
                    let bad = path.with_extension("json.bad");
                    let _ = fs::copy(path, bad);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save_file(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

static READ_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct SettingsStore;

impl SettingsStore {
    pub fn set_read_only(v: bool) {
        READ_ONLY.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_read_only() -> bool {
        READ_ONLY.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn load() -> Settings {
        Settings::load_file(&crate::log::settings_file())
    }

    pub fn save(s: &Settings) {
        if Self::is_read_only() {
            return;
        }
        if let Err(e) = s.save_file(&crate::log::settings_file()) {
            crate::log::error(format!("Cannot save settings | {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csharp_v230_json() {
        let sample = r#"{
          "Version": 2,
          "Mode": "PerApp",
          "Mix": -0.35,
          "KnobTarget": "Volume",
          "KnobStartFunction": "Volume",
          "KnobMixPressMutes": true,
          "KnobHotkeyModifiers": "Ctrl, Alt",
          "Language": "Auto",
          "FirstRunDone": true
        }"#;
        let s = Settings::from_json_str(sample).unwrap();
        assert_eq!(s.mode, EngineMode::PerApp);
        assert_eq!(s.knob_target, KnobTarget::Volume);
        assert_eq!(s.knob_hotkey_modifiers, HotkeyModifiers(3));
        assert!(s.first_run_done);
        assert!((s.mix - (-0.35)).abs() < 1e-9);
    }

    #[test]
    fn clamps_and_migrates_v1() {
        let v1 = r#"{ "Mix": 9.0, "KnobStep": 0.0001 }"#;
        let s = Settings::from_json_str(v1).unwrap();
        assert_eq!(s.version, 2);
        assert_eq!(s.mix, 1.0);
        assert_eq!(s.knob_step, 0.01);
        assert_eq!(s.mode, EngineMode::VirtualDevices);
    }
}
