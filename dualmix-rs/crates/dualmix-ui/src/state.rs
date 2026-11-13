//! Shared serializable state and commands between the Rust controller and the WebView2 Fluent UI.

use dualmix_core::settings::{
    Channel, EngineMode, KnobStartFunction, KnobTarget, LanguagePreference, LatencyMode, ThemePreference, UpdateMode,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StatusSeverity {
    Success,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceChoice {
    pub id: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiAppRow {
    pub key: String,
    pub display_name: String,
    pub initial: String,
    pub subtitle: String,
    pub hint: String,
    pub channel: Channel,
    pub is_explicit: bool,
    pub is_system: bool,
    pub active: bool,
    pub fixed_output: bool,
    pub volume_percent: i32,
    pub peak: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupBannerInfo {
    pub severity: StatusSeverity,
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiState {
    pub mode: String, // "main" | "wizard"
    pub wizard_step: u8,
    pub active_tab: u8,
    pub dark: bool,
    pub lang: String, // "en" | "it"
    pub version: String,
    pub data_dir: String,

    // Status
    pub severity: StatusSeverity,
    pub status_title: String,
    pub status_message: String,
    pub needs_driver: bool,
    pub needs_repair: bool,
    pub setup_busy: bool,
    pub setup_banner: Option<SetupBannerInfo>,

    // Engine & devices
    pub engine_mode: EngineMode,
    pub virtual_active: bool,
    pub router_running: bool,
    pub router_idle: bool,
    pub virtual_cables: usize,
    pub game_via_bus: bool,
    pub engine_chip: String,
    pub engine_text: String,
    pub output_name: String,
    pub format_text: String,
    pub latency_text: String,
    pub game_device_line: String,
    pub chat_device_line: String,
    pub game_device_name: String,
    pub chat_device_name: String,
    pub technical_details: String,
    pub quality_summary: String,
    pub output_choices: Vec<DeviceChoice>,
    pub selected_output_id: Option<String>,
    pub set_windows_defaults: bool,
    pub route_apps_to_devices: bool,
    pub game_direct: bool,
    pub defaults_text: String,
    pub latency: LatencyMode,
    pub no_ducking: bool,
    pub vb_cables_present: bool,

    // Mix & volumes
    pub mix_percent: i32,
    pub mix_text: String,
    pub game_level_text: String,
    pub chat_level_text: String,
    pub game_volume_percent: i32,
    pub chat_volume_percent: i32,
    pub game_muted: bool,
    pub chat_muted: bool,
    pub output_volume_available: bool,
    pub output_volume_percent: i32,
    pub output_muted: bool,
    pub game_peak: f32,
    pub chat_peak: f32,
    pub output_peak: f32,

    // Apps
    pub apps: Vec<UiAppRow>,

    // Knob
    pub knob_target: KnobTarget,
    pub knob_target_text: String,
    pub knob_hotkey_text: String,
    pub knob_cycle: Vec<KnobTarget>,
    pub knob_cycle_summary: String,
    pub knob_double_press: bool,
    pub knob_ctrl_press: bool,
    pub knob_ctrl_alt: bool,
    pub knob_enabled: bool,
    pub knob_invert: bool,
    pub knob_mix_press_mutes: bool,
    pub knob_start_function: KnobStartFunction,
    pub knob_step_percent: i32,
    pub key_mix_game_name: String,
    pub key_mix_chat_name: String,
    pub key_mix_reset_name: String,
    pub capturing_target: Option<String>,
    pub capture_prompt: Option<String>,
    pub keyboard_status: String,
    pub last_key_seen: String,

    // General & updates
    pub autostart: bool,
    pub start_minimized: bool,
    pub show_osd: bool,
    pub restore_volumes_on_exit: bool,
    pub theme: ThemePreference,
    pub language_pref: LanguagePreference,
    pub update_mode: UpdateMode,
    pub update_title: String,
    pub update_text: String,
    pub update_button_text: String,
    pub update_busy: bool,
    pub diag_text: String,
    pub diag_busy: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum UiCommand {
    Ready,
    HideWindow,
    SetTab { tab: u8 },
    SetWizardStep { step: u8 },
    SetMix { percent: f64 },
    ResetMix,
    SetGameVolume { percent: f64 },
    SetChatVolume { percent: f64 },
    SetGameMuted { muted: bool },
    SetChatMuted { muted: bool },
    SetOutputVolume { percent: f64 },
    SetOutputMuted { muted: bool },
    SetAppChannel { key: String, channel: Option<Channel> },
    SetSystemChannel { channel: Channel },
    ResetAppAuto { key: String },
    SetAppVolume { key: String, percent: f64 },
    PickAddApp,
    SetKnobTarget { target: KnobTarget },
    SetKnobCycle { cycle: Vec<KnobTarget> },
    SetKnobOptions {
        double_press: bool,
        ctrl_press: bool,
        ctrl_alt: bool,
        enabled: bool,
        invert: bool,
        mix_press_mutes: bool,
    },
    SetKnobStartFunction { func: KnobStartFunction },
    SetKnobStep { percent: f64 },
    CaptureHotkey,
    ResetHotkey,
    CaptureKey { which: String },
    SetOutputDevice { id: Option<String> },
    SetEngineMode { mode: EngineMode },
    SetAudioOptions {
        set_defaults: bool,
        route_apps: bool,
        game_direct: bool,
    },
    SetLatency { latency: LatencyMode },
    SetDuckingDisabled { disabled: bool },
    DriverAction,
    DismissSetupBanner,
    RepairAudio,
    SetAutostart { enabled: bool },
    SetStartMinimized { enabled: bool },
    SetShowOsd { enabled: bool },
    SetRestoreVolumes { enabled: bool },
    SetTheme { theme: ThemePreference },
    SetLanguage { lang: LanguagePreference },
    SetUpdateMode { mode: UpdateMode },
    UpdateAction,
    OpenWizard,
    TestOsd,
    RunDiag,
    OpenDataFolder,
    OpenUrl { url: String },
    WizardLang { lang: LanguagePreference },
    WizardFinish {
        skip_driver: bool,
        output_id: Option<String>,
        mode: EngineMode,
        start_with_windows: bool,
        disable_ducking: bool,
    },
    BannerRepair,
    BannerInstall,
}
