//! Central application state, 1:1 port of C# `DualMix.Core.Controller`.
//!
//! Lives on the UI thread and is only ever touched from the main message loop. Everything that happens on other
//! threads (endpoint volume callbacks, device notifications, knob keys, session engine, elevated setup, updater)
//! is posted into the [`Inbox`] and handled here, one message at a time: no re-entrancy, no shared borrows.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dualmix_audio::devices::{self, DeviceInfo, Flow, Role};
use dualmix_audio::driver_setup::{self, SetupResult};
use dualmix_audio::endpoint_volume::{DeviceEvent, DeviceNotifications, EndpointVolume};
use dualmix_audio::format;
use dualmix_audio::router::{AudioRouter, RouterState};
use dualmix_audio::sessions::{AudioAppInfo, EngineEvent, SessionEngine, SessionParams};
use dualmix_audio::LatencyProfile;
use dualmix_core::knob::{self, KnobAction, KnobActionKind, KnobConfig};
use dualmix_core::loc;
use dualmix_core::log;
use dualmix_core::math::{self, ChannelGains};
use dualmix_core::rules::RuleSet;
use dualmix_core::settings::{AppRule, Channel, DefaultDevicesBackup, EngineMode, HotkeyModifiers, KnobTarget, Settings, SettingsStore};
use dualmix_input::{KeyboardHook, KnobController};
use dualmix_ui::osd::{OsdInfo, OsdKind};

const CLASS_NOT_REGISTERED: i32 = 0x8004_0154u32 as i32;

/// Message posted to the host window when the inbox has work.
pub const WM_APP_WAKE: u32 = 0x8020;

/// What the knob / shortcut capture is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    Hotkey,
    KeyGame,
    KeyChat,
    KeyReset,
}

/// Work posted from other threads.
#[derive(Debug)]
pub enum Ev {
    OutputVolumeChanged,
    FrontVolumeChanged,
    ChatVolumeChanged,
    Device(DeviceEvent),
    Knob(KnobAction),
    KeySeen(String),
    HotkeysApplied,
    Captured { target: CaptureTarget, vk: i32, mods: HotkeyModifiers },
    SetupDone(SetupResult),
    UpdaterChanged,
    DiagDone(Option<bool>),
}

/// Thread-safe queue + wake-up of the UI thread (the equivalent of `Dispatcher.BeginInvoke`).
pub struct Inbox {
    queue: Mutex<VecDeque<Ev>>,
    hwnd: AtomicIsize,
    pending: AtomicBool,
}

impl Inbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { queue: Mutex::new(VecDeque::new()), hwnd: AtomicIsize::new(0), pending: AtomicBool::new(false) })
    }

    pub fn set_hwnd(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
    }

    pub fn post(&self, ev: Ev) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(ev);
        }
        self.wake();
    }

    pub fn wake(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            unsafe {
                use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(HWND(h as *mut _), WM_APP_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Takes everything queued so far (called by the UI thread).
    pub fn drain(&self) -> Vec<Ev> {
        self.pending.store(false, Ordering::Release);
        self.queue.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Success,
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Success => "success",
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// Requests for the host (windows, dialogs, tray) produced while handling a message.
#[derive(Debug, Clone)]
pub enum HostAction {
    Osd(OsdInfo, bool),
    #[allow(dead_code)]
    Balloon(String, String),
    ShowMain,
    OpenWizard,
    PickPack,
    PickApp,
    CloseWizard,
    LanguageChanged,
    ThemeChanged,
    Exit,
}

#[derive(Clone, Copy)]
enum UndoPress {
    Mix(f64),
    OutputMute,
    GameMute,
    ChatMute,
}

/// Result of the last driver setup, shown on the Audio page (C# `SetupInfo` InfoBar).
#[derive(Debug, Clone)]
pub struct SetupInfo {
    pub severity: Severity,
    pub title: String,
    pub message: String,
}

pub struct Controller {
    pub settings: Settings,
    pub dry_run: bool,
    inbox: Arc<Inbox>,
    rules: Arc<RuleSet>,
    rules_gen: u64,
    sessions: SessionEngine,
    pub router: AudioRouter,
    keyboard: Arc<KeyboardHook>,
    pub knob: Arc<KnobController>,
    output_volume: EndpointVolume,
    master_proxy: EndpointVolume,
    chat_volume: EndpointVolume,
    game_amp: f64,
    chat_amp: f64,
    notifications: Option<DeviceNotifications>,
    meters_enabled: bool,
    save_due: Option<Instant>,
    virtual_active: bool,
    dual: bool,
    last_press_at: i64,
    undo_press: Option<UndoPress>,
    next_volume_note: i64,
    shutdown: bool,
    configured_endpoints: bool,
    auto_setup_tried: bool,
    elevated_config_tried: bool,
    pack_tried: bool,
    router_failures: u32,
    next_evaluate: i64,
    next_stats_log: i64,
    devices_changed_at: Option<i64>,
    defaults_drifted: bool,
    format_changed: bool,
    pending_setup: Option<(bool, bool, Option<PathBuf>)>,
    evaluate_at: Option<i64>,
    // status
    pub severity: Severity,
    pub status_title: String,
    pub status_message: String,
    pub needs_driver: bool,
    pub needs_repair: bool,
    pub setup_busy: bool,
    output_name: Option<String>,
    output_format: Option<format::AudioFormat>,
    underruns_seen: i64,
    next_underrun_log: i64,
    game_name: Option<String>,
    chat_name: Option<String>,
    pub virtual_cables: usize,
    known_level: f64,
    known_muted: bool,
    last_mute_toggle_at: i64,
    // ui
    pub actions: Vec<HostAction>,
    pub state_dirty: bool,
    pub apps_dirty: bool,
    pub last_key_seen: String,
    pub capture: Option<CaptureTarget>,
    pub capture_message: Option<String>,
    pub setup_info: Option<SetupInfo>,
    pub diag_text: Option<String>,
    pub diag_running: bool,
    pub updater: crate::updater::Updater,
    /// Bumped every time the devices were looked at again (the UI caches the device list by it).
    pub devices_gen: u64,
    pub autostart_enabled: bool,
    #[allow(dead_code)]
    started: Instant,
}

/// Milliseconds since the process started (`Environment.TickCount64` differences).
pub fn now_ms() -> i64 {
    dualmix_audio::router::tick_ms()
}

pub fn target_name(t: KnobTarget) -> String {
    match t {
        KnobTarget::Mix => loc::get("Target.Mix"),
        KnobTarget::Volume => loc::get("Target.Volume"),
        KnobTarget::Game => loc::get("Target.Game"),
        KnobTarget::Chat => loc::get("Target.Chat"),
    }
}

/// Default shortcuts for "next knob function", in order of preference.
pub const SHORTCUT_DEFAULTS: [(i32, HotkeyModifiers); 5] = [
    (0x4D, HotkeyModifiers(1 | 2)), // Ctrl + Alt + M
    (0x4B, HotkeyModifiers(1 | 2)), // Ctrl + Alt + K
    (0x52, HotkeyModifiers(1 | 2)), // Ctrl + Alt + R
    (0x7B, HotkeyModifiers(1 | 4)), // Ctrl + Shift + F12
    (0x7B, HotkeyModifiers(1 | 2)), // Ctrl + Alt + F12
];

fn round_to(v: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (v * f).round() / f
}

impl Controller {
    pub fn new(mut settings: Settings, dry_run: bool, inbox: Arc<Inbox>) -> Self {
        // Every start begins on the same knob function (by default the general volume), whatever was left selected.
        if !dry_run {
            if let Some(start) = settings.start_target() {
                if settings.knob_target != start {
                    log::info(format!("Knob function at start: {start:?} (was {:?})", settings.knob_target));
                    settings.knob_target = start;
                }
            }
        }
        let rules = Arc::new(RuleSet::from_settings(&settings));
        let game_amp = math::taper(settings.game_volume);
        let chat_amp = math::taper(settings.chat_volume);

        let ib = Arc::clone(&inbox);
        let knob = KnobController::new(KnobConfig::from_settings(&settings), Arc::new(move |a| ib.post(Ev::Knob(a))));
        let k1 = Arc::clone(&knob);
        let k2 = Arc::clone(&knob);
        let keyboard = KeyboardHook::new(Arc::new(move |vk, down, inj| k1.handle_key(vk, down, inj)), Arc::new(move |id| k2.handle_hotkey(id)));
        keyboard.accept_injected.store(settings.accept_injected_keys, Ordering::Relaxed);
        let ib = Arc::clone(&inbox);
        keyboard.on_hotkeys_applied(Arc::new(move || ib.post(Ev::HotkeysApplied)));
        let ib = Arc::clone(&inbox);
        knob.set_key_seen(Some(Arc::new(move |s| ib.post(Ev::KeySeen(s)))));

        let ib = Arc::clone(&inbox);
        let output_volume = EndpointVolume::new(Arc::new(move || ib.post(Ev::OutputVolumeChanged)));
        let ib = Arc::clone(&inbox);
        let master_proxy = EndpointVolume::new(Arc::new(move || ib.post(Ev::FrontVolumeChanged)));
        let ib = Arc::clone(&inbox);
        let chat_volume = EndpointVolume::new(Arc::new(move || ib.post(Ev::ChatVolumeChanged)));

        let updater = crate::updater::Updater::new(settings.update_feed.as_deref());
        let initial = SessionParams {
            gains: math::compute(&settings),
            rules: Arc::clone(&rules),
            rules_gen: 1,
            route_devices: false,
            game_route_id: None,
            chat_route_id: None,
            direct_route_id: None,
            game_bus_id: None,
            chat_bus_id: None,
            meters_enabled: false,
        };
        let c = Self {
            settings,
            dry_run,
            inbox,
            rules,
            rules_gen: 1,
            sessions: SessionEngine::new(initial),
            router: AudioRouter::new(),
            keyboard,
            knob,
            output_volume,
            master_proxy,
            chat_volume,
            game_amp,
            chat_amp,
            notifications: None,
            meters_enabled: false,
            save_due: None,
            virtual_active: false,
            dual: false,
            last_press_at: 0,
            undo_press: None,
            next_volume_note: 0,
            shutdown: false,
            configured_endpoints: false,
            auto_setup_tried: false,
            elevated_config_tried: false,
            pack_tried: false,
            router_failures: 0,
            next_evaluate: 0,
            next_stats_log: 0,
            devices_changed_at: None,
            defaults_drifted: false,
            format_changed: false,
            pending_setup: None,
            evaluate_at: None,
            severity: Severity::Info,
            status_title: loc::get("Status.Starting"),
            status_message: String::new(),
            needs_driver: false,
            needs_repair: false,
            setup_busy: false,
            output_name: None,
            output_format: None,
            underruns_seen: 0,
            next_underrun_log: 0,
            game_name: None,
            chat_name: None,
            virtual_cables: 0,
            known_level: -1.0,
            known_muted: false,
            last_mute_toggle_at: 0,
            actions: Vec::new(),
            state_dirty: true,
            apps_dirty: true,
            last_key_seen: String::new(),
            capture: None,
            capture_message: None,
            setup_info: None,
            diag_text: None,
            diag_running: false,
            updater,
            devices_gen: 0,
            autostart_enabled: false,
            started: Instant::now(),
        };
        c.sessions.update(c.build_params());
        c
    }

    pub fn mark_auto_setup_handled(&mut self) {
        self.auto_setup_tried = true;
    }

    pub fn inbox(&self) -> Arc<Inbox> {
        Arc::clone(&self.inbox)
    }

    // ===================================================================== lifecycle

    pub fn start(&mut self) {
        self.sessions.start();
        if !self.dry_run {
            self.recover_from_previous_run();
            self.knob.attach(Arc::clone(&self.keyboard));
            self.keyboard.start();
        }
        let ib = Arc::clone(&self.inbox);
        self.notifications = DeviceNotifications::register(Arc::new(move |e| ib.post(Ev::Device(e))));
        self.evaluate();
        log::info(format!(
            "Controller started, mode {:?}, mix {:.2}, knob {:?}",
            self.settings.mode, self.settings.mix, self.settings.knob_target
        ));
    }

    /// Undo leftovers of a crash/forced shutdown while the virtual devices were active.
    fn recover_from_previous_run(&mut self) {
        if self.settings.defaults_backup.is_some() {
            log::info("Restoring default devices left over by a previous run");
            self.restore_windows_defaults();
        }
        if !self.settings.routed_apps.is_empty() {
            log::info("Resetting per-app devices left over by a previous run");
            let keys = self.settings.routed_apps.clone();
            let rx = self.sessions.unroute_all(keys);
            if rx.recv_timeout(std::time::Duration::from_millis(2500)).unwrap_or(false) {
                self.settings.routed_apps.clear();
                self.schedule_save();
            }
        }
    }

    pub fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }
        self.shutdown = true;
        if self.dry_run {
            self.router.stop();
            self.sessions.stop(false);
            return;
        }
        self.keyboard.stop();
        self.notifications = None;
        self.push_params(); // routing off from now on
        if self.virtual_active || !self.settings.routed_apps.is_empty() {
            let rx = self.sessions.unroute_all(self.settings.routed_apps.clone());
            if rx.recv_timeout(std::time::Duration::from_millis(2500)).unwrap_or(false) {
                self.settings.routed_apps.clear();
            }
        }
        self.restore_windows_defaults();
        // The mixer stops only after the apps are back on the real device.
        std::thread::sleep(std::time::Duration::from_millis(150));
        self.router.stop();
        self.sessions.stop(self.settings.restore_volumes_on_exit);
        self.output_volume.detach();
        self.master_proxy.detach();
        self.chat_volume.detach();
        SettingsStore::save(&self.settings);
        log::info("Controller shut down");
    }

    // ===================================================================== inbox

    /// Handles everything posted by other threads. Returns true when something was processed.
    pub fn handle_inbox(&mut self) -> bool {
        let evs = self.inbox.drain();
        let any = !evs.is_empty();
        for ev in evs {
            if self.shutdown {
                break;
            }
            match ev {
                Ev::OutputVolumeChanged => self.on_output_volume_changed(),
                Ev::FrontVolumeChanged => self.on_front_volume_changed(),
                Ev::ChatVolumeChanged => self.pull_cable_volumes(),
                Ev::Device(d) => self.on_device_event(d),
                Ev::Knob(a) => self.on_knob_action(a),
                Ev::KeySeen(s) => {
                    self.last_key_seen = format!("{}  {s}", time_hms());
                }
                Ev::HotkeysApplied => {
                    self.ensure_shortcut_usable();
                    self.state_dirty = true;
                }
                Ev::Captured { target, vk, mods } => self.on_captured(target, vk, mods),
                Ev::SetupDone(r) => self.on_setup_done(r),
                Ev::UpdaterChanged => {
                    self.state_dirty = true;
                }
                Ev::DiagDone(ok) => {
                    self.diag_running = false;
                    self.diag_text = Some(match ok {
                        Some(true) => loc::get("General.DiagOk"),
                        Some(false) => loc::get("General.DiagIssues"),
                        None => loc::fmt("General.DiagFail", &["DualMix.exe --diag"]),
                    });
                    let file = log::data_dir().join("diag.txt");
                    if file.exists() {
                        dualmix_ui::dialogs::shell_open("notepad.exe", Some(&format!("\"{}\"", file.display())));
                    }
                    self.state_dirty = true;
                }
            }
        }
        any
    }

    fn on_device_event(&mut self, e: DeviceEvent) {
        match e {
            DeviceEvent::Changed => {
                if self.devices_changed_at.is_none() {
                    self.devices_changed_at = Some(now_ms());
                }
            }
            DeviceEvent::DefaultChanged { role, id } => {
                if !self.virtual_active {
                    return;
                }
                let s = &self.settings;
                let expected = if role == Role::Communications as i32 {
                    s.chat_device_id.clone()
                } else if self.game_via_bus() {
                    s.game_device_id.clone()
                } else {
                    s.output_device_id.clone()
                };
                if !expected.is_some_and(|e| e.eq_ignore_ascii_case(&id)) {
                    self.defaults_drifted = true;
                }
            }
            DeviceEvent::FormatChanged { id } => {
                if Some(&id) == self.settings.output_device_id.as_ref() {
                    self.format_changed = true;
                    if self.devices_changed_at.is_none() {
                        self.devices_changed_at = Some(now_ms());
                    }
                }
            }
        }
    }

    // ===================================================================== gains / levels

    /// Linear gains: channel volume × balance, exactly ×1 at 100% / centre. A channel with a virtual device uses
    /// the Windows volume of that device (its own dB curve); otherwise the perceptual taper of DualMix.
    pub fn gains(&self) -> ChannelGains {
        if !self.virtual_active {
            return math::compute(&self.settings);
        }
        let (g, c) = math::balance(self.settings.mix, self.settings.mix_floor);
        let game = if self.settings.game_muted { 0.0 } else { self.game_amp * math::taper(g) };
        let chat = if self.settings.chat_muted { 0.0 } else { self.chat_amp * math::taper(c) };
        ChannelGains { game: game as f32, chat: chat as f32 }
    }

    pub fn levels(&self) -> (f64, f64) {
        math::levels(&self.settings)
    }

    pub fn game_level_text(&self) -> String {
        math::percent(self.levels().0)
    }

    pub fn chat_level_text(&self) -> String {
        math::percent(self.levels().1)
    }

    pub fn mix_text(&self) -> String {
        let m = self.settings.mix;
        if m.abs() < 0.001 {
            loc::get("Common.Balanced")
        } else if m < 0.0 {
            loc::fmt("Mixer.MixText.GameBias", &[&self.chat_level_text()])
        } else {
            loc::fmt("Mixer.MixText.ChatBias", &[&self.game_level_text()])
        }
    }

    // ---- Windows volume of the virtual devices

    /// Windows changed the volume/mute of "DualMix Chat": adopt it.
    fn pull_cable_volumes(&mut self) {
        if !self.virtual_active || self.shutdown {
            return;
        }
        if self.chat_volume.is_available() {
            self.settings.chat_volume = round_to(self.chat_volume.level(), 3);
            self.settings.chat_muted = self.chat_volume.muted();
            self.chat_amp = self.chat_volume.amplitude();
        }
        self.mix_state_changed();
    }

    /// DualMix is the authority when the virtual devices come up: write its state to the devices.
    fn push_cable_volumes(&mut self) {
        self.game_amp = math::taper(self.settings.game_volume);
        if self.chat_volume.is_available() {
            self.chat_volume.set_level(self.settings.chat_volume);
            self.chat_volume.set_muted(self.settings.chat_muted);
            self.chat_amp = self.chat_volume.amplitude();
        } else {
            self.chat_amp = math::taper(self.settings.chat_volume);
        }
        self.mirror_output_to_windows();
    }

    fn front_is_proxy(&self) -> bool {
        self.master_proxy.is_available()
    }

    fn front_level(&self) -> f64 {
        if self.front_is_proxy() { self.master_proxy.level() } else { self.output_volume.level() }
    }

    fn front_muted(&self) -> bool {
        if self.front_is_proxy() { self.master_proxy.muted() } else { self.output_volume.muted() }
    }

    fn set_front(&self, level: f64, muted: bool) {
        if self.front_is_proxy() {
            self.master_proxy.set_level(level);
            self.master_proxy.set_muted(muted);
        } else {
            self.output_volume.set_level(level);
            self.output_volume.set_muted(muted);
        }
    }

    /// The Windows default device shows the volume of the real output.
    fn mirror_output_to_windows(&mut self) {
        if !self.output_volume.is_available() {
            return;
        }
        if self.master_proxy.is_available() {
            self.master_proxy.set_level(self.output_volume.level());
            self.master_proxy.set_muted(self.output_volume.muted());
        }
        self.known_level = self.output_volume.level();
        self.known_muted = self.output_volume.muted();
    }

    /// The Windows volume changed without DualMix doing it. If the knob is on another function, a key-sized step is
    /// the knob: the Windows volume is put back and the step goes where it belongs. Otherwise it is a real change.
    fn on_front_volume_changed(&mut self) {
        if self.shutdown || !self.output_volume.is_available() {
            return;
        }
        let level = self.front_level();
        let muted = self.front_muted();
        if self.known_level < 0.0 {
            self.known_level = self.output_volume.level();
            self.known_muted = self.output_volume.muted();
        }
        let prev_level = self.known_level;
        let prev_muted = self.known_muted;
        let mute_toggled = muted != prev_muted;
        let level_moved = (level - prev_level).abs() > 0.004;
        if !mute_toggled && !level_moved {
            return;
        }
        let tick = now_ms();
        let mouse = KeyboardHook::is_down(0x01); // a volume slider is being dragged
        let ctrl = KeyboardHook::is_down(0x11);
        let s = &self.settings;
        if s.knob_enabled && !mouse {
            let mut f = s.knob_target;
            if ctrl && s.knob_ctrl_alternate {
                f = if f == KnobTarget::Mix { KnobTarget::Volume } else { KnobTarget::Mix };
            }
            if mute_toggled && !level_moved {
                if ctrl && s.knob_ctrl_press_cycles {
                    self.set_front(prev_level, prev_muted);
                    self.cycle_knob_target();
                    return;
                }
                if f != KnobTarget::Volume {
                    self.set_front(prev_level, prev_muted);
                    log::info("Knob press reached Windows instead of DualMix: corrected");
                    self.on_knob_press(f);
                    return;
                }
                // Function "Volume": the press is the normal Windows mute. Two in a row switch function instead,
                // and the sound is left unmuted, as it was before the first press.
                if s.knob_double_press_cycles {
                    if tick - self.last_mute_toggle_at <= 600 {
                        self.last_mute_toggle_at = 0;
                        if muted {
                            self.set_front(level, false);
                        }
                        self.apply_front(level, false);
                        self.cycle_knob_target();
                        return;
                    }
                    self.last_mute_toggle_at = tick;
                }
            } else if !mute_toggled && f != KnobTarget::Volume {
                let steps = (level - prev_level) / 0.02; // one key press = 2%
                let k = steps.round() as i32;
                if k != 0 && k.abs() <= 6 && (steps - k as f64).abs() < 0.26 {
                    self.set_front(prev_level, prev_muted);
                    if tick >= self.next_volume_note {
                        self.next_volume_note = tick + 5000;
                        log::info(format!("Knob turn reached Windows instead of DualMix: corrected (function {f:?})"));
                    }
                    for _ in 0..k.abs() {
                        self.on_knob_action(KnobAction::rotate(k.signum(), f));
                    }
                    return;
                }
            }
        }
        self.apply_front(level, muted);
    }

    /// A real change of the Windows volume: the sound card follows.
    fn apply_front(&mut self, level: f64, muted: bool) {
        if self.master_proxy.is_available() {
            self.output_volume.set_level(level);
            self.output_volume.set_muted(muted);
        }
        self.known_level = level;
        self.known_muted = muted;
        self.state_dirty = true;
    }

    /// The real output changed (sound card software, or Windows when it is the default device).
    fn on_output_volume_changed(&mut self) {
        if self.shutdown {
            return;
        }
        if self.master_proxy.is_available() {
            self.mirror_output_to_windows();
            self.state_dirty = true;
        } else {
            self.on_front_volume_changed();
        }
    }

    pub fn output_volume_available(&self) -> bool {
        self.output_volume.is_available()
    }

    pub fn output_volume_percent(&self) -> f64 {
        (self.output_volume.level() * 100.0).round()
    }

    pub fn set_output_volume_percent(&mut self, value: f64) {
        self.output_volume.set_level(value / 100.0);
        if value > 0.0 && self.output_volume.muted() {
            self.output_volume.set_muted(false);
        }
        self.mirror_output_to_windows();
        self.state_dirty = true;
    }

    pub fn output_muted(&self) -> bool {
        self.output_volume.muted()
    }

    pub fn set_output_muted(&mut self, v: bool) {
        self.output_volume.set_muted(v);
        self.mirror_output_to_windows();
        self.state_dirty = true;
    }

    // ---- meters

    pub fn game_peak(&self) -> f32 {
        if self.game_via_bus() { self.router.game_peak().max(self.sessions.game_peak()) } else { self.sessions.game_peak() }
    }

    pub fn chat_peak(&self) -> f32 {
        if self.virtual_active { self.router.chat_peak() } else { self.sessions.chat_peak() }
    }

    pub fn output_peak(&self) -> f32 {
        let r = if self.virtual_active { self.router.output_peak() } else { 0.0 };
        r.max(self.game_peak().max(self.chat_peak()))
    }

    pub fn apps(&self) -> Arc<Vec<AudioAppInfo>> {
        self.sessions.snapshot()
    }

    #[allow(dead_code)]
    pub fn meters_enabled(&self) -> bool {
        self.meters_enabled
    }

    pub fn set_meters_enabled(&mut self, v: bool) {
        if self.meters_enabled == v {
            return;
        }
        self.meters_enabled = v;
        self.push_params();
    }

    // ---- status

    pub fn virtual_active(&self) -> bool {
        self.virtual_active
    }

    #[allow(dead_code)]
    pub fn has_game_bus(&self) -> bool {
        self.virtual_active && self.dual
    }

    pub fn game_via_bus(&self) -> bool {
        self.virtual_active && self.dual && !self.settings.game_direct
    }

    pub fn output_name(&self) -> String {
        self.output_name.clone().unwrap_or_else(|| loc::get("Common.NoOutput"))
    }

    pub fn game_device_name(&self) -> String {
        self.game_name.clone().unwrap_or_else(|| "—".into())
    }

    pub fn chat_device_name(&self) -> String {
        self.chat_name.clone().unwrap_or_else(|| "—".into())
    }

    pub fn router_running(&self) -> bool {
        self.virtual_active && self.router.state() == RouterState::Running
    }

    pub fn router_idle(&self) -> bool {
        self.router_running() && self.router.suspended()
    }

    /// Format of the real output device (what the sound card receives).
    pub fn format_text(&self) -> String {
        self.output_format.as_ref().map(format::describe).unwrap_or_else(|| "—".into())
    }

    pub fn latency_text(&self) -> String {
        if !self.router_running() {
            return "—".into();
        }
        let l = loc::int0(self.router.latency_ms());
        if self.game_via_bus() { loc::fmt("Latency.Both", &[&l]) } else { loc::fmt("Latency.ChatOnly", &[&l]) }
    }

    pub fn engine_text(&self) -> String {
        if self.virtual_active {
            if self.game_via_bus() { loc::get("Engine.BothVirtual") } else { loc::get("Engine.ChatVirtual") }
        } else {
            loc::get("Engine.PerApp")
        }
    }

    pub fn technical_details(&self) -> String {
        if !self.router_running() {
            return loc::get("Status.Tech.NotRunning");
        }
        let r = &self.router;
        let game = if self.game_via_bus() {
            loc::fmt(
                "Status.Tech.GameBus",
                &[&r.game_format().map(|f| format::describe(&f)).unwrap_or_default(), &loc::signed1(r.game_drift_ppm())],
            )
        } else {
            loc::get("Status.Tech.GameDirect")
        };
        let extra = if r.underruns() > 0 { format!(" ({})", r.dropout_detail()) } else { String::new() };
        let idle = if r.suspended() { loc::get("Status.Tech.Idle") } else { String::new() };
        let rt = if r.real_time_scheduling() { String::new() } else { loc::get("Status.Tech.NoRt") };
        loc::fmt(
            "Status.Tech.Summary",
            &[
                &r.output_format().map(|f| format::describe(&f)).unwrap_or_default(),
                &game,
                &r.chat_format().map(|f| format::describe(&f)).unwrap_or_default(),
                &loc::signed1(r.chat_drift_ppm()),
                &r.underruns().to_string(),
                &extra,
                &idle,
                &rt,
            ],
        )
    }

    /// Text and severity of the small chip on the Mixer page.
    pub fn engine_chip(&self) -> String {
        if self.router_running() {
            if self.router_idle() { loc::get("Mixer.ChipActiveIdle") } else { loc::fmt("Mixer.ChipActiveLat", &[&self.latency_text()]) }
        } else if self.virtual_active {
            loc::get("Mixer.ChipStarting")
        } else {
            self.engine_text()
        }
    }

    // ===================================================================== mix actions

    pub fn set_mix(&mut self, value: f64, from_knob: bool) {
        let v = math::snap_center(value.clamp(-1.0, 1.0));
        if (v - self.settings.mix).abs() < 1e-6 && !from_knob {
            return;
        }
        self.settings.mix = v;
        self.mix_state_changed();
        if from_knob {
            log::info(format!("Knob: mix {v:+.2} (Game {}, Chat {})", self.game_level_text(), self.chat_level_text()));
            self.show_mix_osd();
        }
    }

    /// Slider value −100..100 (gentle magnet around the centre so "balanced" is easy to hit with the mouse).
    pub fn set_mix_percent(&mut self, value: f64) {
        self.set_mix(if value.abs() < 3.0 { 0.0 } else { value / 100.0 }, false);
    }

    pub fn nudge(&mut self, delta: f64) {
        let current = self.settings.mix;
        let mut next = current + delta;
        // The knob stops at the center for one notch when crossing it, so "balanced" is easy to find.
        if current != 0.0 && next.signum() != current.signum() && next != 0.0 {
            next = 0.0;
        }
        self.set_mix(next, true);
    }

    pub fn reset_mix(&mut self, from_knob: bool) {
        self.set_mix(0.0, from_knob);
    }

    pub fn set_channel_volume(&mut self, ch: Channel, value: f64, from_knob: bool) {
        let value = round_to(value.clamp(0.0, 1.0), 3);
        if ch == Channel::Game {
            self.settings.game_volume = value;
            if self.settings.game_muted && value > 0.0 && from_knob {
                self.settings.game_muted = false;
            }
            self.game_amp = math::taper(value);
        } else {
            self.settings.chat_volume = value;
            if self.settings.chat_muted && value > 0.0 && from_knob {
                self.settings.chat_muted = false;
            }
            if self.virtual_active && self.chat_volume.is_available() {
                self.chat_volume.set_level(value);
                self.chat_volume.set_muted(self.settings.chat_muted);
                self.chat_amp = self.chat_volume.amplitude();
            } else {
                self.chat_amp = math::taper(value);
            }
        }
        self.mix_state_changed();
        if from_knob {
            log::info(format!("Knob: {ch:?} volume {:.0}%", value * 100.0));
            self.show_channel_osd(ch);
        }
    }

    pub fn set_game_muted(&mut self, v: bool) {
        self.settings.game_muted = v;
        self.mix_state_changed();
    }

    pub fn set_chat_muted(&mut self, v: bool) {
        self.settings.chat_muted = v;
        if self.virtual_active && self.chat_volume.is_available() {
            self.chat_volume.set_muted(v);
        }
        self.mix_state_changed();
    }

    /// Recomputes and pushes gains after a setting that affects them changed.
    #[allow(dead_code)]
    pub fn refresh_gains(&mut self) {
        self.mix_state_changed();
    }

    fn mix_state_changed(&mut self) {
        self.push_gains();
        self.state_dirty = true;
        self.schedule_save();
    }

    fn push_gains(&mut self) {
        self.router.set_gains(self.gains());
        self.push_params();
    }

    // ===================================================================== knob

    pub fn knob_hotkey_text(&self) -> String {
        knob::chord_name(self.settings.knob_hotkey_key, self.settings.knob_hotkey_modifiers)
    }

    /// A default shortcut that another program already registered would only half work: move on to the next free
    /// default (a shortcut chosen by the user is never changed).
    fn ensure_shortcut_usable(&mut self) {
        let s = &self.settings;
        if self.shutdown || s.knob_hotkey_custom || !s.knob_enabled || s.knob_hotkey_key == 0 {
            return;
        }
        if self.keyboard.is_hotkey_active(s.knob_hotkey_key, s.knob_hotkey_modifiers) {
            return;
        }
        let Some(i) = SHORTCUT_DEFAULTS.iter().position(|&(vk, m)| vk == s.knob_hotkey_key && m == s.knob_hotkey_modifiers) else { return };
        if i + 1 >= SHORTCUT_DEFAULTS.len() {
            return;
        }
        let old = self.knob_hotkey_text();
        let (vk, m) = SHORTCUT_DEFAULTS[i + 1];
        self.settings.knob_hotkey_key = vk;
        self.settings.knob_hotkey_modifiers = m;
        log::info(format!("Shortcut {old} is already used by another program: using {} instead", self.knob_hotkey_text()));
        self.knob_settings_changed(); // registers again, and this check runs once more with the result
    }

    /// How the keys of the knob reach DualMix, for the settings page.
    pub fn keyboard_status(&self) -> String {
        if !self.settings.knob_enabled {
            return loc::get("Knob.Status.Disabled");
        }
        let total = self.knob.bindings().len();
        let failed = self.keyboard.failed_hotkeys();
        let active = self.keyboard.active_hotkeys();
        if failed.is_empty() {
            loc::fmt("Knob.Status.AllActive", &[&active.to_string()])
        } else {
            loc::fmt("Knob.Status.SomeFailed", &[&active.to_string(), &total.to_string(), &failed.join(", ")])
        }
    }

    fn on_knob_action(&mut self, a: KnobAction) {
        let step = self.settings.knob_step;
        match a.kind {
            KnobActionKind::Rotate => {
                let dir = if self.settings.knob_invert { -a.direction } else { a.direction } as f64;
                match a.target {
                    // clockwise = towards Chat, like the GameDAC dial
                    KnobTarget::Mix => self.nudge(dir * step),
                    KnobTarget::Volume => self.adjust_output_volume(dir * step),
                    KnobTarget::Game => self.set_channel_volume(Channel::Game, self.settings.game_volume + dir * step, true),
                    KnobTarget::Chat => self.set_channel_volume(Channel::Chat, self.settings.chat_volume + dir * step, true),
                }
            }
            KnobActionKind::Press => self.on_knob_press(a.target),
            KnobActionKind::CycleFunction => {
                self.undo_press = None;
                self.cycle_knob_target();
            }
            KnobActionKind::MixTowardsGame => self.nudge(-step),
            KnobActionKind::MixTowardsChat => self.nudge(step),
            KnobActionKind::MixReset => self.reset_mix(true),
        }
    }

    /// A press acts at once (mute; on the mix: mute everything, or balanced mix when KnobMixPressMutes is off). A
    /// second press right after it means "double press": the first one is taken back and the knob switches function.
    fn on_knob_press(&mut self, target: KnobTarget) {
        let now = now_ms();
        if self.settings.knob_double_press_cycles && self.undo_press.is_some() && now - self.last_press_at <= 450 {
            let undo = self.undo_press.take();
            self.last_press_at = 0;
            match undo {
                Some(UndoPress::Mix(before)) => self.set_mix(before, false),
                Some(UndoPress::OutputMute) => {
                    let m = !self.output_muted();
                    self.set_output_muted(m);
                }
                Some(UndoPress::GameMute) => self.set_game_muted(!self.settings.game_muted),
                Some(UndoPress::ChatMute) => self.set_chat_muted(!self.settings.chat_muted),
                None => {}
            }
            self.cycle_knob_target();
            return;
        }
        self.last_press_at = now;
        match target {
            KnobTarget::Mix if !self.settings.knob_mix_press_mutes => {
                let before = self.settings.mix;
                self.reset_mix(true);
                self.undo_press = Some(UndoPress::Mix(before));
            }
            KnobTarget::Mix | KnobTarget::Volume => {
                if !self.output_volume.is_available() {
                    self.undo_press = None;
                    return;
                }
                let m = !self.output_muted();
                self.set_output_muted(m);
                log::info(format!("Knob: output {}", if m { "muted" } else { "unmuted" }));
                self.show_volume_osd();
                self.undo_press = Some(UndoPress::OutputMute);
            }
            KnobTarget::Game => {
                self.set_game_muted(!self.settings.game_muted);
                self.show_channel_osd(Channel::Game);
                self.undo_press = Some(UndoPress::GameMute);
            }
            KnobTarget::Chat => {
                self.set_chat_muted(!self.settings.chat_muted);
                self.show_channel_osd(Channel::Chat);
                self.undo_press = Some(UndoPress::ChatMute);
            }
        }
    }

    pub fn cycle_knob_target(&mut self) {
        let cycle = if self.settings.knob_cycle.is_empty() { vec![KnobTarget::Mix, KnobTarget::Volume] } else { self.settings.knob_cycle.clone() };
        let i = cycle.iter().position(|&t| t == self.settings.knob_target);
        let next = match i {
            Some(i) => cycle[(i + 1) % cycle.len()],
            None => cycle[0],
        };
        self.set_knob_target(next, true);
    }

    pub fn set_knob_target(&mut self, target: KnobTarget, show_osd: bool) {
        self.settings.knob_target = target;
        self.knob.update_config(KnobConfig::from_settings(&self.settings));
        self.state_dirty = true;
        self.schedule_save();
        log::info(format!("Knob function: {target:?}"));
        if show_osd {
            self.osd(OsdInfo { kind: OsdKind::Function, title: loc::get("Common.Wheel"), value_text: target_name(target), value: 0.0, muted: false }, false);
        }
    }

    /// Master volume from DualMix itself (the function "Volume" normally leaves the keys to Windows).
    fn adjust_output_volume(&mut self, delta: f64) {
        if !self.output_volume.is_available() {
            return;
        }
        let v = (((self.output_volume.level() + delta) * 100.0).round() / 100.0).clamp(0.0, 1.0);
        self.output_volume.set_level(v);
        if delta > 0.0 && self.output_volume.muted() {
            self.output_volume.set_muted(false);
        }
        self.mirror_output_to_windows();
        self.state_dirty = true;
        log::info(format!("Knob: output volume {:.0}%", v * 100.0));
        self.show_volume_osd();
    }

    fn osd(&mut self, info: OsdInfo, force: bool) {
        if self.settings.show_osd || force {
            self.actions.push(HostAction::Osd(info, force));
        }
    }

    pub fn mix_osd_info(&self) -> OsdInfo {
        let text = if self.settings.mix.abs() < 0.001 {
            loc::get("Common.Balanced")
        } else {
            format!("Game {} · Chat {}", self.game_level_text(), self.chat_level_text())
        };
        OsdInfo { kind: OsdKind::Mix, title: loc::get("Common.Mix"), value_text: text, value: self.settings.mix, muted: false }
    }

    fn show_mix_osd(&mut self) {
        let i = self.mix_osd_info();
        self.osd(i, false);
    }

    fn show_volume_osd(&mut self) {
        let muted = self.output_volume.muted();
        let lvl = self.output_volume.level();
        self.osd(
            OsdInfo {
                kind: OsdKind::Volume,
                title: loc::get("Common.Volume"),
                value_text: if muted { loc::get("Common.Muted") } else { math::percent(lvl) },
                value: lvl,
                muted,
            },
            false,
        );
    }

    fn show_channel_osd(&mut self, ch: Channel) {
        let game = ch == Channel::Game;
        let vol = if game { self.settings.game_volume } else { self.settings.chat_volume };
        let muted = if game { self.settings.game_muted } else { self.settings.chat_muted };
        self.osd(
            OsdInfo {
                kind: if game { OsdKind::Game } else { OsdKind::Chat },
                title: if game { loc::get("Common.Game") } else { loc::get("Common.Chat") },
                value_text: if muted { loc::get("Common.Muted") } else { math::percent(vol) },
                value: vol,
                muted,
            },
            false,
        );
    }

    pub fn set_accept_injected(&mut self, v: bool) {
        self.keyboard.accept_injected.store(v, Ordering::Relaxed);
    }

    pub fn knob_settings_changed(&mut self) {
        self.knob.update_config(KnobConfig::from_settings(&self.settings));
        self.state_dirty = true;
        self.schedule_save();
    }

    /// Starts capturing the next key (settings page); the result arrives as `Ev::Captured`.
    pub fn begin_capture(&mut self, target: CaptureTarget) {
        self.capture = Some(target);
        self.capture_message = None;
        let ib = Arc::clone(&self.inbox);
        self.knob.set_capture_next(Some(Box::new(move |vk, mods| ib.post(Ev::Captured { target, vk, mods }))));
        self.state_dirty = true;
    }

    pub fn cancel_capture(&mut self) {
        self.knob.set_capture_next(None);
        self.capture = None;
        self.state_dirty = true;
    }

    fn on_captured(&mut self, target: CaptureTarget, vk: i32, mods: HotkeyModifiers) {
        self.capture = None;
        self.state_dirty = true;
        match target {
            CaptureTarget::Hotkey => {
                if vk == 0x1B {
                    return; // Esc: cancel
                }
                if (mods == HotkeyModifiers::NONE || mods == HotkeyModifiers::SHIFT) && !knob::usable_alone(vk) {
                    self.capture_message = Some(loc::get("Knob.HotkeyNeedMod"));
                    return;
                }
                self.settings.knob_hotkey_key = vk;
                self.settings.knob_hotkey_modifiers = mods;
                self.settings.knob_hotkey_custom = true;
                self.knob_settings_changed();
            }
            CaptureTarget::KeyGame | CaptureTarget::KeyChat | CaptureTarget::KeyReset => {
                let vk = if vk == 0x1B { 0 } else { vk };
                match target {
                    CaptureTarget::KeyGame => self.settings.key_mix_game = vk,
                    CaptureTarget::KeyChat => self.settings.key_mix_chat = vk,
                    _ => self.settings.key_mix_reset = vk,
                }
                self.knob_settings_changed();
            }
        }
    }

    pub fn reset_hotkey(&mut self) {
        self.cancel_capture();
        let (vk, m) = SHORTCUT_DEFAULTS[0];
        self.settings.knob_hotkey_key = vk;
        self.settings.knob_hotkey_modifiers = m;
        self.settings.knob_hotkey_custom = false; // DualMix picks the first default no other program is using
        self.knob_settings_changed();
    }

    // ===================================================================== apps / rules

    pub fn set_app_channel(&mut self, key: &str, channel: Option<Channel>) {
        let rule = self.settings.apps.entry(key.to_string()).or_insert_with(AppRule::default);
        rule.channel = channel;
        self.rules_changed(true);
    }

    pub fn set_app_volume(&mut self, key: &str, volume: f64) {
        let rule = self.settings.apps.entry(key.to_string()).or_insert_with(AppRule::default);
        rule.volume = volume.clamp(0.0, 1.0);
        self.rules_changed(false);
    }

    pub fn forget_app(&mut self, key: &str) {
        if self.settings.apps.remove(key).is_some() {
            self.rules_changed(true);
        }
    }

    pub fn add_app_from_path(&mut self, path: &std::path::Path) {
        let key = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if key.is_empty() {
            return;
        }
        if !self.settings.apps.contains_key(&key) {
            let mut r = AppRule::default();
            r.display_name = Some(dualmix_audio::process_info::friendly_name(Some(&path.to_string_lossy()), &key));
            r.exe_path = Some(path.to_string_lossy().into_owned());
            self.settings.apps.insert(key, r);
        }
        self.rules_changed(true);
    }

    pub fn rules_changed(&mut self, raise_apps: bool) {
        self.rules = Arc::new(RuleSet::from_settings(&self.settings));
        self.rules_gen += 1;
        self.push_params();
        self.schedule_save();
        if raise_apps {
            self.apps_dirty = true;
        }
    }

    pub fn rescan_now(&self) {
        self.sessions.request_rescan();
    }

    // ===================================================================== engine

    /// Called after any device/engine setting changed.
    pub fn engine_settings_changed(&mut self, restart_router: bool) {
        if restart_router {
            self.router.stop();
        }
        self.configured_endpoints = false;
        self.router_failures = 0;
        self.evaluate();
        self.schedule_save();
    }

    fn find_output(&self, render: &[DeviceInfo]) -> Option<DeviceInfo> {
        let s = &self.settings;
        let mut output = None;
        if s.output_chosen_by_user {
            output = render.iter().find(|d| Some(&d.id) == s.output_device_id.as_ref() && !d.is_vb_cable()).cloned();
        }
        let default_mm = s.defaults_backup.as_ref().and_then(|b| b.multimedia.clone()).or_else(|| devices::get_default_id(Role::Multimedia, Flow::Render));
        if output.is_none() {
            output = devices::find_preferred_output(render, default_mm.as_deref()).cloned();
        }
        if output.is_none() {
            output = render.iter().find(|d| Some(&d.id) == s.output_device_id.as_ref() && !d.is_vb_cable()).cloned();
        }
        output
    }

    /// Looks at the devices and decides how to run: virtual devices when a cable and the real output are healthy,
    /// otherwise per-app volumes with every Windows setting restored (fail-open).
    pub fn evaluate(&mut self) {
        if self.shutdown {
            return;
        }
        self.devices_changed_at = None;
        self.devices_gen += 1;
        let mut render = devices::list_render();
        let output = self.find_output(&render);
        if let Some(o) = &output {
            if Some(&o.id) != self.settings.output_device_id.as_ref() && !self.settings.output_chosen_by_user {
                self.settings.output_device_id = Some(o.id.clone());
                self.schedule_save();
            }
        }
        self.output_name = output.as_ref().map(|o| o.name.clone());
        self.output_format = output.as_ref().and_then(|o| devices::get_device_format(&o.id));
        self.output_volume.attach(output.as_ref().map(|o| o.id.as_str()));

        let probe = output.as_ref().and_then(|o| devices::probe_open(&o.id));
        self.needs_repair = probe == Some(CLASS_NOT_REGISTERED);

        let want_virtual = self.settings.mode == EngineMode::VirtualDevices;
        let (mut game, mut chat) = devices::find_cables(&render);
        let driver_present = chat.is_some() || driver_setup::cables_present();

        if !self.dry_run && want_virtual && driver_present && output.is_some() && probe.is_none() && (!self.configured_endpoints || self.format_changed) {
            self.format_changed = false;
            self.configured_endpoints = true;
            let result = driver_setup::configure(&self.settings, false, None);
            if result.changed {
                self.router.stop();
                render = devices::list_render();
                (game, chat) = devices::find_cables(&render);
            }
            let asked_recently = log::hours_since(self.settings.elevated_config_at.as_deref()).is_some_and(|h| h < 24.0);
            if result.failed && !self.elevated_config_tried && !self.setup_busy && !asked_recently {
                // Some endpoint settings need administrator rights: finish elevated (at most one request a day).
                self.elevated_config_tried = true;
                self.settings.elevated_config_at = Some(log::now_iso());
                self.schedule_save();
                log::info("Endpoint configuration needs elevation");
                self.pending_setup = Some((false, false, None));
            }
        }

        let chat_ok = chat.is_some();
        self.virtual_cables = devices::find_buses(&render).len();
        self.needs_driver = !chat_ok;
        self.game_name = game.as_ref().map(|g| g.name.clone());
        self.chat_name = chat.as_ref().map(|c| c.name.clone());

        if want_virtual && chat_ok && output.is_some() && probe.is_none() && !self.router.blocked() {
            let chat = chat.clone().unwrap();
            self.settings.chat_device_id = Some(chat.id.clone());
            self.settings.game_device_id = game.as_ref().map(|g| g.id.clone());
            // "DualMix Game" stands in for the real output in the Windows volume interface while it is the default.
            let proxy = if game.is_some() && !self.settings.game_direct && self.settings.set_windows_defaults { game.as_ref().map(|g| g.id.clone()) } else { None };
            self.master_proxy.attach(proxy.as_deref());
            self.chat_volume.attach(Some(&chat.id));
            self.activate_virtual();
        } else {
            self.deactivate_virtual();
        }

        self.update_status(want_virtual, chat_ok, output.as_ref(), probe);
        self.push_gains();
        self.state_dirty = true;

        if self.router.blocked() {
            self.set_status(Severity::Error, loc::get("Status.Blocked.Title"), loc::get("Status.Blocked.Body"));
            self.next_evaluate = now_ms() + 60_000;
        }
        if self.dry_run {
            return;
        }
        let declined_long_ago = log::hours_since(self.settings.driver_setup_declined_at.as_deref()).is_none_or(|h| h > 12.0);
        // A second cable was installed before from a purchased pack and is gone: put it back automatically.
        if self.settings.first_run_done && want_virtual && chat_ok && game.is_none() && !self.settings.game_direct && !self.pack_tried && !self.setup_busy && probe.is_none() {
            self.pack_tried = true;
            if let Some(cached) = driver_setup::cached_pack() {
                if declined_long_ago && self.pending_setup.is_none() {
                    self.pending_setup = Some((false, false, Some(cached)));
                }
            }
        }
        // Install the driver / repair the audio service automatically, once per start (after the setup wizard).
        if self.settings.first_run_done && !self.auto_setup_tried && !self.setup_busy && ((want_virtual && !chat_ok) || self.needs_repair) {
            self.auto_setup_tried = true;
            if declined_long_ago && self.pending_setup.is_none() {
                self.pending_setup = Some((want_virtual && !chat_ok, self.needs_repair, None));
            }
        }
    }

    fn activate_virtual(&mut self) {
        let was_active = self.virtual_active;
        self.virtual_active = true;
        self.dual = self.settings.game_device_id.is_some();
        if !self.dry_run {
            self.push_cable_volumes();
        }
        self.router.set_gains(self.gains());
        // The mixer must be running before any audio is sent to the virtual devices.
        let router_game = if self.game_via_bus() { self.settings.game_device_id.clone() } else { None };
        let same = self.router.game_id() == router_game && self.router.chat_id() == self.settings.chat_device_id && self.router.output_id() == self.settings.output_device_id;
        if !self.router.is_running() || !same {
            self.start_router();
            let deadline = Instant::now() + std::time::Duration::from_millis(1500);
            while self.router.state() == RouterState::Starting && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        if !self.dry_run {
            if self.settings.set_windows_defaults {
                self.apply_windows_defaults();
            } else {
                self.restore_windows_defaults();
            }
            if !self.settings.route_apps_to_devices && !self.settings.routed_apps.is_empty() {
                self.unroute_apps(500);
            }
        }
        self.push_params();
        if !was_active {
            log::info(format!(
                "Virtual devices active ({}) → {}",
                if self.game_via_bus() { "Game + Chat" } else { "Chat; Game direct" },
                self.output_name()
            ));
        }
    }

    fn deactivate_virtual(&mut self) {
        let was_active = self.virtual_active;
        self.virtual_active = false;
        self.dual = false;
        self.game_amp = math::taper(self.settings.game_volume);
        self.chat_amp = math::taper(self.settings.chat_volume);
        self.master_proxy.detach();
        self.chat_volume.detach();
        self.push_params();
        // Give the audio back to the real device first; the mixer stops last, so nothing falls into silence.
        if was_active || !self.settings.routed_apps.is_empty() {
            self.unroute_apps(500);
        }
        self.restore_windows_defaults();
        self.router.stop();
        if was_active {
            log::info("Virtual devices inactive (per-app volumes)");
        }
    }

    fn start_router(&mut self) {
        self.router.set_gains(self.gains());
        let mut p = LatencyProfile::for_mode(self.settings.latency);
        p.extra_queue_ms = self.settings.learned_queue_ms.clamp(0.0, 40.0);
        p.extra_source_ms = self.settings.learned_source_ms.clamp(0.0, 20.0);
        let game = if self.game_via_bus() { self.settings.game_device_id.clone() } else { None };
        let (Some(chat), Some(out)) = (self.settings.chat_device_id.clone(), self.settings.output_device_id.clone()) else { return };
        if let Err(e) = self.router.start(game.as_deref(), &chat, &out, p) {
            log::error(format!("Router start failed | {e}"));
        }
    }

    fn update_status(&mut self, want_virtual: bool, chat_ok: bool, output: Option<&DeviceInfo>, probe: Option<i32>) {
        let (sev, title, msg) = if self.setup_busy {
            (Severity::Info, loc::get("Status.SetupBusy.Title"), loc::get("Status.SetupBusy.Body"))
        } else if output.is_none() {
            (Severity::Error, loc::get("Status.NoOutput.Title"), loc::get("Status.NoOutput.Body"))
        } else if probe == Some(CLASS_NOT_REGISTERED) {
            (Severity::Error, loc::get("Status.AudioLost.Title"), loc::get("Status.CardNoPlay.Body"))
        } else if let Some(p) = probe {
            (Severity::Error, loc::get("Status.OutputUnusable.Title"), loc::fmt("Status.OutputUnusable.Body", &[&output.unwrap().name, &format!("{:08X}", p as u32)]))
        } else if want_virtual && !chat_ok {
            (Severity::Warning, loc::get("Status.NoDriver.Title"), loc::get("Status.NoDriver.Body"))
        } else if !want_virtual {
            (Severity::Info, loc::get("Status.PerAppMode.Title"), loc::fmt("Status.PerAppMode.Body", &[&output.unwrap().name]))
        } else if self.game_via_bus() {
            (Severity::Success, loc::get("Status.ReadyDual.Title"), loc::fmt("Status.ReadyDual.Body", &[&output.unwrap().name]))
        } else {
            (Severity::Success, loc::get("Status.ReadySingle.Title"), loc::fmt("Status.ReadySingle.Body", &[&output.unwrap().name]))
        };
        self.set_status(sev, title, msg);
    }

    fn set_status(&mut self, sev: Severity, title: String, message: String) {
        self.severity = sev;
        self.status_title = title;
        self.status_message = message;
        self.state_dirty = true;
    }

    /// Runs the elevated helper (UAC prompt) on a worker thread; the result arrives as `Ev::SetupDone`.
    pub fn run_setup(&mut self, install: bool, repair: bool, pack: Option<PathBuf>) {
        if self.setup_busy {
            return;
        }
        self.setup_busy = true;
        self.update_status(true, true, None, None);
        if repair {
            // Restarting the audio service kills every stream: put Windows back to normal first.
            self.deactivate_virtual();
        } else {
            // The endpoints are about to be reconfigured: release them.
            self.router.stop();
        }
        let Ok(exe) = std::env::current_exe() else {
            self.setup_busy = false;
            return;
        };
        let ib = Arc::clone(&self.inbox);
        std::thread::spawn(move || {
            let _com = dualmix_audio::com::ComGuard::mta();
            let r = driver_setup::run_elevated_blocking(&exe, install, repair, pack.as_deref());
            ib.post(Ev::SetupDone(r));
        });
    }

    fn on_setup_done(&mut self, r: SetupResult) {
        self.setup_busy = false;
        self.settings.driver_setup_declined_at = if r.declined { Some(log::now_iso()) } else { None };
        self.schedule_save();
        self.configured_endpoints = false;
        self.router_failures = 0;
        self.setup_info = Some(if r.declined {
            SetupInfo { severity: Severity::Warning, title: loc::get("Audio.SetupInfo.DeclinedTitle"), message: loc::get("Audio.SetupInfo.DeclinedBody") }
        } else {
            let failed = !r.ok || r.pack_rejected;
            SetupInfo {
                severity: if failed { Severity::Error } else if r.reboot_recommended { Severity::Warning } else { Severity::Success },
                title: if failed {
                    loc::get("Audio.SetupInfo.FailedTitle")
                } else if r.reboot_recommended {
                    loc::get("Audio.SetupInfo.RebootTitle")
                } else {
                    loc::get("Audio.SetupInfo.DoneTitle")
                },
                message: if r.message.trim().is_empty() { loc::get("Audio.SetupInfo.VerifiedBody") } else { r.message.clone() },
            }
        });
        // Give Windows a moment to publish new endpoints / restart its engine.
        self.evaluate_at = Some(now_ms() + 1500);
        self.state_dirty = true;
    }

    // ---- Windows defaults

    /// Playback default: the real output (or "DualMix Game"). Communications default: "DualMix Chat".
    fn apply_windows_defaults(&mut self) {
        let (Some(chat_id), Some(out_id)) = (self.settings.chat_device_id.clone(), self.settings.output_device_id.clone()) else { return };
        let playback = if self.game_via_bus() { self.settings.game_device_id.clone().unwrap_or(out_id.clone()) } else { out_id.clone() };
        if self.settings.defaults_backup.is_none() {
            let mut b = DefaultDevicesBackup {
                console: devices::get_default_id(Role::Console, Flow::Render),
                multimedia: devices::get_default_id(Role::Multimedia, Flow::Render),
                communications: devices::get_default_id(Role::Communications, Flow::Render),
            };
            // Never back up our own virtual devices as "the real default".
            let ours = |id: &Option<String>| id.as_ref().is_some_and(|i| Some(i) == self.settings.game_device_id.as_ref() || Some(i) == self.settings.chat_device_id.as_ref());
            if ours(&b.console) {
                b.console = Some(out_id.clone());
            }
            if ours(&b.multimedia) {
                b.multimedia = Some(out_id.clone());
            }
            if ours(&b.communications) {
                b.communications = Some(out_id.clone());
            }
            self.settings.defaults_backup = Some(b);
            SettingsStore::save(&self.settings);
        }
        let mut changed = false;
        if devices::get_default_id(Role::Multimedia, Flow::Render).as_deref() != Some(&playback) || devices::get_default_id(Role::Console, Flow::Render).as_deref() != Some(&playback) {
            devices::set_default(&playback, Role::Console);
            devices::set_default(&playback, Role::Multimedia);
            changed = true;
        }
        if devices::get_default_id(Role::Communications, Flow::Render).as_deref() != Some(&chat_id) {
            devices::set_default(&chat_id, Role::Communications);
            changed = true;
        }
        if changed {
            log::info(format!("Windows defaults: playback → {}, communications → {}", devices::name_of(Some(&playback)), devices::name_of(Some(&chat_id))));
        }
    }

    fn restore_windows_defaults(&mut self) {
        let Some(b) = self.settings.defaults_backup.clone() else { return };
        // Fall back to the real output DualMix was using when a saved device is gone.
        let out = self.settings.output_device_id.clone();
        let pick = |saved: Option<String>| -> Option<String> {
            if devices::is_active(saved.as_deref()) {
                saved
            } else if devices::is_active(out.as_deref()) {
                out.clone()
            } else {
                None
            }
        };
        for (id, role) in [(pick(b.console), Role::Console), (pick(b.multimedia), Role::Multimedia), (pick(b.communications), Role::Communications)] {
            if let Some(id) = id {
                if devices::get_default_id(role, Flow::Render).as_deref() != Some(&id) {
                    devices::set_default(&id, role);
                }
            }
        }
        self.settings.defaults_backup = None;
        SettingsStore::save(&self.settings);
        log::info("Windows default devices restored");
    }

    /// Resets the per-app devices DualMix set (waits at most `wait_ms` for the session engine).
    fn unroute_apps(&mut self, wait_ms: u64) {
        let keys = self.settings.routed_apps.clone();
        let rx = self.sessions.unroute_all(keys.clone());
        if rx.recv_timeout(std::time::Duration::from_millis(wait_ms)).unwrap_or(false) {
            self.settings.routed_apps.retain(|k| !keys.iter().any(|x| x.eq_ignore_ascii_case(k)));
            self.schedule_save();
        }
    }

    fn build_params(&self) -> SessionParams {
        let s = &self.settings;
        let dual = self.virtual_active && self.dual;
        let via_bus = self.game_via_bus();
        // With "DualMix Game" as the Windows default, Game apps land there by themselves and "Direct" apps must be
        // sent to the real output explicitly; otherwise nothing needs routing except Chat.
        let game_is_default = via_bus && s.set_windows_defaults;
        SessionParams {
            gains: self.gains(),
            rules: Arc::clone(&self.rules),
            rules_gen: self.rules_gen,
            route_devices: self.virtual_active && s.route_apps_to_devices && !self.dry_run && !self.shutdown,
            game_route_id: if via_bus && !game_is_default { s.game_device_id.clone() } else { None },
            chat_route_id: if self.virtual_active { s.chat_device_id.clone() } else { None },
            direct_route_id: if game_is_default { s.output_device_id.clone() } else { None },
            game_bus_id: if dual { s.game_device_id.clone() } else { None },
            chat_bus_id: if self.virtual_active { s.chat_device_id.clone() } else { None },
            meters_enabled: self.meters_enabled,
        }
    }

    fn push_params(&mut self) {
        let p = self.build_params();
        self.sessions.update(p);
    }

    // ===================================================================== periodic work (every 250 ms)

    pub fn tick(&mut self) {
        if self.shutdown {
            return;
        }
        let now = now_ms();

        // Engine events (new apps, adopted volumes, routed apps)
        let mut rules_changed = false;
        while let Some(ev) = self.sessions.try_dequeue_event() {
            match ev {
                EngineEvent::AppDiscovered { key, display_name, exe_path, volume } => {
                    match self.settings.apps.get_mut(&key) {
                        None => {
                            let mut r = AppRule::default();
                            r.volume = volume;
                            r.display_name = Some(display_name);
                            r.exe_path = exe_path;
                            r.last_seen = Some(log::now_iso());
                            self.settings.apps.insert(key, r);
                            rules_changed = true;
                        }
                        Some(r) => {
                            r.display_name = Some(display_name);
                            if exe_path.is_some() {
                                r.exe_path = exe_path;
                            }
                            r.last_seen = Some(log::now_iso());
                        }
                    }
                }
                EngineEvent::AppVolumeAdopted { key, volume } => {
                    self.settings.apps.entry(key).or_insert_with(AppRule::default).volume = volume;
                    rules_changed = true;
                }
                EngineEvent::AppRouted { key } => {
                    if !self.settings.routed_apps.iter().any(|k| k.eq_ignore_ascii_case(&key)) {
                        self.settings.routed_apps.push(key);
                        SettingsStore::save(&self.settings); // persist right away: needed for crash recovery
                    }
                }
            }
        }
        if rules_changed {
            self.rules_changed(true);
        }

        if let Some(at) = self.evaluate_at {
            if now >= at {
                self.evaluate_at = None;
                self.evaluate();
            }
        }

        // Device changes (debounced)
        if let Some(at) = self.devices_changed_at {
            if now - at > 800 && !self.setup_busy {
                log::info("Audio devices changed: re-evaluating");
                self.evaluate();
            }
        }

        // Someone moved the Windows defaults (e.g. a headset was plugged in): the output must stay where it is.
        if self.defaults_drifted && !self.setup_busy {
            self.defaults_drifted = false;
            if self.virtual_active && self.settings.set_windows_defaults {
                self.apply_windows_defaults();
            }
        }

        // Router supervision
        if self.virtual_active && !self.setup_busy {
            let st = self.router.state();
            let hung = st == RouterState::Starting && now - self.router.started_at() > 6000;
            if st == RouterState::Error || hung || (!self.router.is_running() && st != RouterState::Starting) {
                self.router_failures += 1;
                let reason = if hung { loc::get("Status.RouterHungReason") } else { self.router.last_error().unwrap_or_else(|| loc::get("Status.RouterStoppedReason")) };
                log::warn(format!("Router failure #{}: {reason}", self.router_failures));
                if hung || self.router.last_error_code() == CLASS_NOT_REGISTERED || self.router_failures >= 3 {
                    // Fail-open: give the apps back to the real device while we wait.
                    if hung {
                        log::error(format!("Router start is hanging at '{}'", self.router.stage()));
                    }
                    self.deactivate_virtual();
                    if self.router.blocked() {
                        self.evaluate();
                    } else {
                        self.set_status(Severity::Warning, loc::get("Status.VirtualPaused.Title"), loc::fmt("Status.VirtualPaused.Problem", &[&reason]));
                        self.next_evaluate = now + 60_000i64.min(5000i64 << (self.router_failures - 1).min(4));
                    }
                } else {
                    self.start_router();
                }
            } else if st == RouterState::Running && now - self.router.started_at() > 30_000 {
                self.router_failures = 0;
            }
        } else if self.next_evaluate > 0 && now >= self.next_evaluate && !self.setup_busy {
            self.next_evaluate = 0;
            self.evaluate();
        }

        // Dropouts are worth knowing about (at most one line every 10 s), with their cause
        if self.router_running() {
            let u = self.router.underruns();
            if u < self.underruns_seen {
                self.underruns_seen = u;
            }
            if u > self.underruns_seen && now >= self.next_underrun_log {
                log::warn(format!("Router: {} dropout(s) (total {u}: {})", u - self.underruns_seen, self.router.dropout_detail()));
                self.underruns_seen = u;
                self.next_underrun_log = now + 10_000;
            }
            if let Some(note) = self.router.take_note() {
                log::info(format!("Router: {note}"));
                // Remember what this PC needs, so the next start begins with it.
                let (q, src) = (self.router.learned_queue_ms(), self.router.learned_source_ms());
                if q > self.settings.learned_queue_ms + 0.5 || src > self.settings.learned_source_ms + 0.5 {
                    self.settings.learned_queue_ms = self.settings.learned_queue_ms.max(round_to(q, 1));
                    self.settings.learned_source_ms = self.settings.learned_source_ms.max(round_to(src, 1));
                    self.schedule_save();
                }
            }
        }

        // Health line in the log every 10 minutes while audio is flowing
        if self.router_running() && !self.router.suspended() && now >= self.next_stats_log {
            self.next_stats_log = now + 600_000;
            log::info(format!(
                "Router ok: clock correction chat {:+.1} ppm{}, underruns {}, added latency {:.0} ms",
                self.router.chat_drift_ppm(),
                if self.game_via_bus() { format!(", game {:+.1} ppm", self.router.game_drift_ppm()) } else { String::new() },
                self.router.underruns(),
                self.router.latency_ms()
            ));
        }

        // Elevated setup requested by Evaluate (runs outside of it, like the C# BeginInvoke)
        if let Some((install, repair, pack)) = self.pending_setup.take() {
            if !self.setup_busy {
                self.run_setup(install, repair, pack);
            }
        }

        // Debounced save
        if let Some(due) = self.save_due {
            if Instant::now() >= due {
                self.save_due = None;
                SettingsStore::save(&self.settings);
            }
        }
    }

    pub fn schedule_save(&mut self) {
        self.save_due = Some(Instant::now() + std::time::Duration::from_millis(1500));
    }

    pub fn save_now(&mut self) {
        self.save_due = None;
        SettingsStore::save(&self.settings);
    }

    #[allow(dead_code)]
    pub fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn tray_tooltip(&self) -> String {
        let problem = if matches!(self.severity, Severity::Error | Severity::Warning) { " · ⚠" } else { "" };
        let t = format!("DualMix · Game {} · Chat {}{problem}", self.game_level_text(), self.chat_level_text());
        t.chars().take(63).collect()
    }
}

pub fn time_hms() -> String {
    let t = log::local_now();
    format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second)
}
