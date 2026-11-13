//! Core application controller (1:1 port of C# `DualMix.Core.Controller`).
//!
//! Coordinates WASAPI devices, `AudioRouter`, `SessionEngine`, `EndpointVolume`, `KeyboardHook`,
//! `KnobController`, `Updater`, `AutoStart`, OSD notifications, and UI state snapshots.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use dualmix_audio::devices::{self, DeviceInfo, Flow, Role};
use dualmix_audio::driver_setup::{self, SetupResult};
use dualmix_audio::endpoint_volume::{DeviceEvent, DeviceNotifications, EndpointVolume};
use dualmix_audio::format;
use dualmix_audio::router::{AudioRouter, RouterState};
use dualmix_audio::sessions::{EngineEvent, SessionEngine, SessionParams};
use dualmix_audio::LatencyProfile;
use dualmix_core::knob::{self, KnobAction, KnobActionKind, KnobConfig};
use dualmix_core::loc;
use dualmix_core::log;
use dualmix_core::math;
use dualmix_core::rules::{RuleSet, SYSTEM_SOUNDS_KEY};
use dualmix_core::settings::{
    AppRule, Channel, DefaultDevicesBackup, EngineMode, HotkeyModifiers, KnobStartFunction, KnobTarget,
    LanguagePreference, Settings, SettingsStore,
};
use dualmix_input::{KeyboardHook, KnobController};
use dualmix_ui::dialogs;
use dualmix_ui::osd::{OsdInfo, OsdKind};
use dualmix_ui::state::{DeviceChoice, SetupBannerInfo, StatusSeverity, UiAppRow, UiCommand, UiState};
use dualmix_ui::tray::TrayStatus;
use dualmix_ui::webview::LivePatch;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::system_integration;
use crate::updater::Updater;

pub const WM_APP_WAKE: u32 = 0x8020;

#[derive(Debug, Clone)]
pub enum ControllerEvent {
    Osd(OsdInfo),
    StateChanged,
    LiveChanged,
    WizardClosed,
    ShowMain,
    OpenWizard,
    Balloon { title: String, body: String },
}

pub struct WakeHandle {
    hwnd: std::sync::atomic::AtomicIsize,
}

impl WakeHandle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { hwnd: std::sync::atomic::AtomicIsize::new(0) })
    }
    pub fn set_hwnd(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::Release);
    }
    pub fn wake(&self) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let hwnd = HWND(raw as *mut _);
            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

struct MuteBackup {
    at: Instant,
    game_muted: bool,
    chat_muted: bool,
    output_muted: bool,
}

pub struct Controller {
    pub settings: Settings,
    pub dry_run: bool,
    pub version: String,

    rules: Arc<RuleSet>,
    rules_gen: u64,

    router: AudioRouter,
    sessions: SessionEngine,
    endpoint_vol: EndpointVolume,
    _dev_notify: Option<DeviceNotifications>,
    keyboard: Arc<KeyboardHook>,
    knob: Arc<KnobController>,
    pub updater: Arc<Updater>,
    wake: Arc<WakeHandle>,

    // Thread-safe event flags/queues drained on UI thread in `tick()`
    ep_vol_dirty: Arc<AtomicBool>,
    dev_events: Arc<Mutex<VecDeque<DeviceEvent>>>,
    knob_actions: Arc<Mutex<VecDeque<KnobAction>>>,
    ui_events: Mutex<VecDeque<ControllerEvent>>,
    last_key_seen: Arc<Mutex<String>>,
    capture_prompt: Mutex<Option<String>>,
    capturing_target: Mutex<Option<String>>,
    captured_input: Arc<Mutex<Option<(i32, HotkeyModifiers)>>>,

    // Async setup / diag worker results
    setup_busy: Arc<AtomicBool>,
    setup_result: Arc<Mutex<Option<(SetupResult, bool)>>>,
    setup_banner: Option<SetupBannerInfo>,
    diag_busy: Arc<AtomicBool>,
    diag_text: Arc<Mutex<String>>,

    // State tracking
    virtual_active: bool,
    virtual_cables: usize,
    game_bus: Option<DeviceInfo>,
    chat_bus: Option<DeviceInfo>,
    active_output: Option<DeviceInfo>,
    render_cache: Vec<DeviceInfo>,
    windows_defaults_applied: bool,

    // Startup volumes for restore-on-exit
    start_ep_vol: Option<(String, f64, bool)>,

    // Runtime & persistence timers
    save_dirty_since: Option<Instant>,
    last_mute_backup: Option<MuteBackup>,
    last_auto_restart: Option<Instant>,
    auto_restart_count: u32,
    pending_re_evaluate: Option<Instant>,
    pending_reconfigure: bool,
    last_health_log: Instant,
    meters_enabled: bool,
    mode_wizard: bool,
    wizard_step: u8,
    active_tab: u8,
}

impl Controller {
    pub fn new(dry_run: bool, accept_injected_cli: bool, lang_cli: Option<LanguagePreference>, wake: Arc<WakeHandle>) -> Self {
        if dry_run {
            SettingsStore::set_read_only(true);
        }
        let mut settings = SettingsStore::load();
        if let Some(l) = lang_cli {
            settings.language = l;
        }
        loc::initialize(
            settings.language,
            match lang_cli {
                Some(LanguagePreference::English) => Some("en"),
                Some(LanguagePreference::Italian) => Some("it"),
                _ => None,
            },
        );

        // Enforce start function on launch (default: general Windows Volume)
        Self::apply_start_function_to(&mut settings);

        let rules = Arc::new(RuleSet::from_settings(&settings));
        let gains = math::compute(&settings);

        let initial_params = SessionParams {
            gains,
            rules: rules.clone(),
            rules_gen: 1,
            route_devices: false,
            game_route_id: None,
            chat_route_id: None,
            direct_route_id: None,
            game_bus_id: None,
            chat_bus_id: None,
            meters_enabled: false,
        };
        let sessions = SessionEngine::new(initial_params);

        let ep_vol_dirty = Arc::new(AtomicBool::new(false));
        let ep_dirty_cb = ep_vol_dirty.clone();
        let wake_ep = wake.clone();
        let endpoint_vol = EndpointVolume::new(Arc::new(move || {
            ep_dirty_cb.store(true, Ordering::Release);
            wake_ep.wake();
        }));

        let dev_events = Arc::new(Mutex::new(VecDeque::new()));
        let dev_cb_q = dev_events.clone();
        let wake_dev = wake.clone();
        let dev_notify = if dry_run {
            None
        } else {
            DeviceNotifications::register(Arc::new(move |ev| {
                if let Ok(mut q) = dev_cb_q.lock() {
                    q.push_back(ev);
                }
                wake_dev.wake();
            }))
        };

        let knob_actions = Arc::new(Mutex::new(VecDeque::new()));
        let ka_cb = knob_actions.clone();
        let wake_knob = wake.clone();
        let knob = KnobController::new(
            KnobConfig::from_settings(&settings),
            Arc::new(move |act| {
                if let Ok(mut q) = ka_cb.lock() {
                    q.push_back(act);
                }
                wake_knob.wake();
            }),
        );

        let knob_slot = Arc::new(OnceLock::<Arc<KnobController>>::new());
        let _ = knob_slot.set(knob.clone());
        let k_key = knob_slot.clone();
        let k_hot = knob_slot.clone();
        let keyboard = KeyboardHook::new(
            Arc::new(move |vk, down, injected| {
                if let Some(kc) = k_key.get() {
                    kc.handle_key(vk, down, injected)
                } else {
                    false
                }
            }),
            Arc::new(move |id| {
                if let Some(kc) = k_hot.get() {
                    kc.handle_hotkey(id);
                }
            }),
        );
        if settings.accept_injected_keys || accept_injected_cli {
            keyboard.accept_injected.store(true, Ordering::Release);
        }

        let last_key_seen = Arc::new(Mutex::new(String::from("-")));
        let lks = last_key_seen.clone();
        let wake_ks = wake.clone();
        knob.set_key_seen(Some(Arc::new(move |desc| {
            if let Ok(mut g) = lks.lock() {
                *g = desc;
            }
            wake_ks.wake();
        })));
        knob.attach(keyboard.clone());

        let version = env!("CARGO_PKG_VERSION").to_string();
        let updater = Arc::new(Updater::new(version.clone()));

        let mut c = Self {
            settings,
            dry_run,
            version,
            rules,
            rules_gen: 1,
            router: AudioRouter::new(),
            sessions,
            endpoint_vol,
            _dev_notify: dev_notify,
            keyboard,
            knob,
            updater,
            wake,
            ep_vol_dirty,
            dev_events,
            knob_actions,
            ui_events: Mutex::new(VecDeque::new()),
            last_key_seen,
            capture_prompt: Mutex::new(None),
            capturing_target: Mutex::new(None),
            captured_input: Arc::new(Mutex::new(None)),
            setup_busy: Arc::new(AtomicBool::new(false)),
            setup_result: Arc::new(Mutex::new(None)),
            setup_banner: None,
            diag_busy: Arc::new(AtomicBool::new(false)),
            diag_text: Arc::new(Mutex::new(String::new())),
            virtual_active: false,
            virtual_cables: 0,
            game_bus: None,
            chat_bus: None,
            active_output: None,
            render_cache: Vec::new(),
            windows_defaults_applied: false,
            start_ep_vol: None,
            save_dirty_since: None,
            last_mute_backup: None,
            last_auto_restart: None,
            auto_restart_count: 0,
            pending_re_evaluate: None,
            pending_reconfigure: false,
            last_health_log: Instant::now(),
            meters_enabled: false,
            mode_wizard: false,
            wizard_step: 1,
            active_tab: 0,
        };

        if !dry_run {
            c.keyboard.start();
            c.sessions.start();
        }
        c.evaluate(true);
        c
    }

    pub fn set_wizard_mode(&mut self, wizard: bool) {
        self.mode_wizard = wizard;
        self.wizard_step = 1;
    }

    pub fn set_meters_enabled(&mut self, enabled: bool) {
        if self.meters_enabled != enabled {
            self.meters_enabled = enabled;
            self.push_session_params();
        }
    }

    pub fn pop_event(&mut self) -> Option<ControllerEvent> {
        self.ui_events.lock().ok().and_then(|mut q| q.pop_front())
    }

    fn push_event(&self, ev: ControllerEvent) {
        if let Ok(mut q) = self.ui_events.lock() {
            q.push_back(ev);
        }
        self.wake.wake();
    }

    pub fn push_event_balloon(&self, title: String, body: String) {
        self.push_event(ControllerEvent::Balloon { title, body });
    }

    fn apply_start_function_to(s: &mut Settings) {
        let wanted = match s.knob_start_function {
            KnobStartFunction::Last => return,
            KnobStartFunction::Mix => KnobTarget::Mix,
            KnobStartFunction::Volume => KnobTarget::Volume,
            KnobStartFunction::Game => KnobTarget::Game,
            KnobStartFunction::Chat => KnobTarget::Chat,
        };
        if s.knob_cycle.contains(&wanted) {
            s.knob_target = wanted;
        } else {
            s.knob_target = KnobTarget::Volume;
        }
    }

    fn mark_dirty(&mut self) {
        if self.save_dirty_since.is_none() {
            self.save_dirty_since = Some(Instant::now());
        }
    }

    pub fn save_now(&mut self) {
        self.save_dirty_since = None;
        if !self.dry_run {
            SettingsStore::save(&self.settings);
        }
    }

    pub fn shutdown(&mut self) {
        if self.settings.restore_volumes_on_exit && !self.dry_run {
            if let Some((ref id, lvl, muted)) = self.start_ep_vol {
                if self.endpoint_vol.device_id() == Some(id.as_str()) {
                    self.endpoint_vol.set_level(lvl);
                    self.endpoint_vol.set_muted(muted);
                }
            }
        }
        self.save_now();
        self.router.stop();
        if !self.dry_run {
            let _ = self
                .sessions
                .unroute_all(self.settings.routed_apps.clone())
                .recv_timeout(Duration::from_millis(800));
            self.sessions.stop(self.settings.restore_volumes_on_exit);
            self.restore_windows_defaults();
            self.keyboard.stop();
        }
        self.endpoint_vol.detach();
    }

    pub fn evaluate(&mut self, reconfigure_endpoints: bool) {
        if self.dry_run {
            self.render_cache = devices::list_render();
            let (g, c) = devices::find_cables(&self.render_cache);
            self.virtual_cables = devices::find_buses(&self.render_cache).len().min(2);
            self.game_bus = g;
            self.chat_bus = c;
            self.active_output = self
                .settings
                .output_device_id
                .as_deref()
                .and_then(|id| self.render_cache.iter().find(|d| d.id == id).cloned())
                .or_else(|| devices::find_preferred_output(&self.render_cache, None).cloned());
            if let Some(ref out) = self.active_output {
                self.endpoint_vol.attach(Some(&out.id));
            }
            return;
        }

        self.render_cache = devices::list_render();
        let saved_out = self
            .settings
            .output_device_id
            .as_deref()
            .and_then(|id| self.render_cache.iter().find(|d| d.id == id).cloned());
        let output = saved_out.or_else(|| devices::find_preferred_output(&self.render_cache, None).cloned());
        self.active_output = output.clone();

        if let Some(ref out) = output {
            let changed_ep = self.endpoint_vol.device_id() != Some(out.id.as_str());
            self.endpoint_vol.attach(Some(&out.id));
            if changed_ep && self.start_ep_vol.is_none() && self.endpoint_vol.is_available() {
                self.start_ep_vol = Some((
                    out.id.clone(),
                    self.endpoint_vol.level(),
                    self.endpoint_vol.muted(),
                ));
            }
        } else {
            self.endpoint_vol.detach();
        }

        self.virtual_cables = devices::find_buses(&self.render_cache).len().min(2);
        let want_virtual = self.settings.mode == EngineMode::VirtualDevices && self.virtual_cables > 0 && output.is_some();

        if want_virtual {
            let out = output.unwrap();
            if reconfigure_endpoints {
                let res = driver_setup::configure(&self.settings, false, None);
                if res.changed {
                    self.render_cache = devices::list_render();
                }
            }
            let (g, c) = devices::find_cables(&self.render_cache);
            self.game_bus = g.clone();
            self.chat_bus = c.clone();
            if let Some(chat_dev) = c {
                let want_game_id = if self.settings.game_direct { None } else { g.as_ref().map(|d| d.id.as_str()) };
                let need_restart = !self.router.is_running()
                    || self.router.output_id().as_deref() != Some(out.id.as_str())
                    || self.router.chat_id().as_deref() != Some(chat_dev.id.as_str())
                    || self.router.game_id().as_deref() != want_game_id;

                if need_restart && !self.router.blocked() {
                    let mut prof = LatencyProfile::for_mode(self.settings.latency);
                    prof.extra_queue_ms = prof.extra_queue_ms.max(self.settings.learned_queue_ms);
                    prof.extra_source_ms = prof.extra_source_ms.max(self.settings.learned_source_ms);
                    self.router.set_gains(math::compute(&self.settings));
                    if let Err(e) = self.router.start(want_game_id, &chat_dev.id, &out.id, prof) {
                        log::error(format!("Router start failed: {e}"));
                    } else {
                        self.settings.game_device_id = want_game_id.map(str::to_string);
                        self.settings.chat_device_id = Some(chat_dev.id.clone());
                        if !self.settings.output_chosen_by_user {
                            self.settings.output_device_id = Some(out.id.clone());
                        }
                    }
                }
                self.virtual_active = true;
                if self.settings.set_windows_defaults {
                    self.apply_windows_defaults(want_game_id, &chat_dev.id, &out.id);
                } else {
                    self.restore_windows_defaults();
                }
            } else {
                self.deactivate_virtual();
            }
        } else {
            self.deactivate_virtual();
        }

        self.push_session_params();
        self.push_event(ControllerEvent::StateChanged);
    }

    fn deactivate_virtual(&mut self) {
        if self.virtual_active || self.router.is_running() {
            self.router.stop();
        }
        self.virtual_active = false;
        self.game_bus = None;
        self.chat_bus = None;
        self.restore_windows_defaults();
    }

    fn apply_windows_defaults(&mut self, game_id: Option<&str>, chat_id: &str, out_id: &str) {
        let want_console = game_id.unwrap_or(out_id);
        let want_comm = chat_id;
        if self.settings.defaults_backup.is_none() {
            let mut bk = DefaultDevicesBackup {
                console: devices::get_default_id(Role::Console, Flow::Render),
                multimedia: devices::get_default_id(Role::Multimedia, Flow::Render),
                communications: devices::get_default_id(Role::Communications, Flow::Render),
            };
            // Don't back up our own virtual buses as original defaults
            if bk.console.as_deref() == Some(want_console) || bk.console.as_deref() == Some(want_comm) {
                bk.console = Some(out_id.to_string());
            }
            if bk.multimedia.as_deref() == Some(want_console) || bk.multimedia.as_deref() == Some(want_comm) {
                bk.multimedia = Some(out_id.to_string());
            }
            self.settings.defaults_backup = Some(bk);
            self.mark_dirty();
        }
        if devices::get_default_id(Role::Console, Flow::Render).as_deref() != Some(want_console) {
            devices::set_default(want_console, Role::Console);
        }
        if devices::get_default_id(Role::Multimedia, Flow::Render).as_deref() != Some(want_console) {
            devices::set_default(want_console, Role::Multimedia);
        }
        if devices::get_default_id(Role::Communications, Flow::Render).as_deref() != Some(want_comm) {
            devices::set_default(want_comm, Role::Communications);
        }
        self.windows_defaults_applied = true;
    }

    fn restore_windows_defaults(&mut self) {
        let Some(bk) = self.settings.defaults_backup.take() else {
            self.windows_defaults_applied = false;
            return;
        };
        self.mark_dirty();
        if let Some(id) = bk.console.as_deref().filter(|id| devices::is_active(Some(id))) {
            devices::set_default(id, Role::Console);
        }
        if let Some(id) = bk.multimedia.as_deref().filter(|id| devices::is_active(Some(id))) {
            devices::set_default(id, Role::Multimedia);
        }
        if let Some(id) = bk.communications.as_deref().filter(|id| devices::is_active(Some(id))) {
            devices::set_default(id, Role::Communications);
        }
        self.windows_defaults_applied = false;
    }

    fn push_session_params(&self) {
        let gains = math::compute(&self.settings);
        self.router.set_gains(gains);

        let game_bus_id = self.game_bus.as_ref().map(|d| d.id.clone());
        let chat_bus_id = self.chat_bus.as_ref().map(|d| d.id.clone());
        let out_id = self.active_output.as_ref().map(|d| d.id.clone());

        let (route_devices, game_route_id, chat_route_id, direct_route_id) = if self.virtual_active && self.settings.route_apps_to_devices {
            (
                true,
                if self.settings.game_direct { out_id.clone() } else { game_bus_id.clone() },
                chat_bus_id.clone(),
                out_id.clone(),
            )
        } else {
            (false, None, None, None)
        };

        self.sessions.update(SessionParams {
            gains,
            rules: self.rules.clone(),
            rules_gen: self.rules_gen,
            route_devices,
            game_route_id,
            chat_route_id,
            direct_route_id,
            game_bus_id,
            chat_bus_id,
            meters_enabled: self.meters_enabled,
        });
    }

    fn rebuild_rules(&mut self) {
        self.rules = Arc::new(RuleSet::from_settings(&self.settings));
        self.rules_gen += 1;
        self.push_session_params();
    }

    fn apply_gains(&self) {
        self.push_session_params();
    }

    pub fn tick(&mut self) {
        let mut need_state = false;
        let mut need_live = false;

        // 1. Endpoint volume external changes
        if self.ep_vol_dirty.swap(false, Ordering::AcqRel) {
            need_live = true;
            if self.settings.knob_enabled
                && self.settings.knob_target == KnobTarget::Volume
                && self.knob.last_handled_tick() > 0
            {
                let ago = dualmix_audio::router::tick_ms() - self.knob.last_handled_tick();
                if ago > 600 && !self.dry_run {
                    self.show_target_osd(KnobTarget::Volume);
                }
            } else if !self.dry_run {
                let ago = dualmix_audio::router::tick_ms() - self.knob.last_handled_tick();
                if self.knob.last_handled_tick() == 0 || ago > 600 {
                    // External Windows volume change (e.g. hardware knob bypassing hook)
                    if self.settings.knob_enabled && self.settings.knob_target == KnobTarget::Volume {
                        self.show_target_osd(KnobTarget::Volume);
                    }
                }
            }
        }

        // 2. Device notifications (debounced re-evaluate)
        let mut got_dev = false;
        let mut reconf = false;
        while let Some(ev) = self.dev_events.lock().ok().and_then(|mut q| q.pop_front()) {
            got_dev = true;
            match ev {
                DeviceEvent::Changed | DeviceEvent::FormatChanged { .. } => {
                    reconf = true;
                }
                DeviceEvent::DefaultChanged { .. } => {}
            }
        }
        if got_dev {
            self.pending_re_evaluate = Some(Instant::now() + Duration::from_millis(450));
            if reconf {
                self.pending_reconfigure = true;
            }
        }
        if let Some(due) = self.pending_re_evaluate {
            if Instant::now() >= due {
                self.pending_re_evaluate = None;
                let rec = std::mem::take(&mut self.pending_reconfigure);
                self.evaluate(rec);
                need_state = true;
            }
        }

        // 3. Knob actions from hook thread
        while let Some(act) = self.knob_actions.lock().ok().and_then(|mut q| q.pop_front()) {
            self.handle_knob_action(act);
            need_live = true;
            need_state = true;
        }

        // 3b. Captured key or hotkey
        if let Some((vk, mods)) = self.captured_input.lock().ok().and_then(|mut g| g.take()) {
            let target = self.capturing_target.lock().ok().and_then(|mut g| g.take());
            *self.capture_prompt.lock().unwrap() = None;
            if let Some(tgt) = target {
                match tgt.as_str() {
                    "hotkey" => {
                        self.settings.knob_hotkey_key = vk;
                        self.settings.knob_hotkey_modifiers = mods;
                        self.settings.knob_hotkey_custom = true;
                    }
                    "mixGame" => {
                        self.settings.key_mix_game = vk;
                    }
                    "mixChat" => {
                        self.settings.key_mix_chat = vk;
                    }
                    "mixReset" => {
                        self.settings.key_mix_reset = vk;
                    }
                    _ => {}
                }
                self.knob.update_config(KnobConfig::from_settings(&self.settings));
                self.mark_dirty();
                need_state = true;
            }
        }

        // 4. Session engine events
        while let Some(ev) = self.sessions.try_dequeue_event() {
            match ev {
                EngineEvent::AppDiscovered { key, display_name, exe_path, volume } => {
                    let entry = self.settings.apps.entry(key.clone()).or_insert_with(AppRule::default);
                    entry.display_name = Some(display_name);
                    if exe_path.is_some() {
                        entry.exe_path = exe_path;
                    }
                    if (volume - 1.0).abs() > 0.01 && entry.volume == 1.0 {
                        entry.volume = volume.clamp(0.0, 1.0);
                    }
                    self.mark_dirty();
                    need_state = true;
                }
                EngineEvent::AppVolumeAdopted { key, volume } => {
                    if let Some(r) = self.settings.apps.get_mut(&key) {
                        r.volume = volume.clamp(0.0, 1.0);
                        self.mark_dirty();
                        need_live = true;
                    }
                }
                EngineEvent::AppRouted { key } => {
                    if !self.settings.routed_apps.contains(&key) {
                        self.settings.routed_apps.push(key.clone());
                        self.mark_dirty();
                    }
                    log::info(format!("Routed '{key}'"));
                }
            }
        }

        // 5. Async driver setup completion
        if let Some((res, _install)) = self.setup_result.lock().ok().and_then(|mut g| g.take()) {
            self.setup_busy.store(false, Ordering::Release);
            let sev = if res.ok {
                StatusSeverity::Success
            } else if res.declined {
                StatusSeverity::Warning
            } else {
                StatusSeverity::Error
            };
            self.setup_banner = Some(SetupBannerInfo {
                severity: sev,
                title: if res.ok {
                    loc::get("Audio.SetupInfo.DoneTitle")
                } else if res.declined {
                    loc::get("Audio.SetupInfo.DeclinedTitle")
                } else {
                    loc::get("Audio.SetupInfo.FailedTitle")
                },
                message: res.message,
            });
            self.evaluate(true);
            need_state = true;
        }

        // 6. Router notes, learned reserves, & health supervision
        if let Some(note) = self.router.take_note() {
            log::info(note);
        }
        if self.virtual_active {
            let lq = self.router.learned_queue_ms();
            let ls = self.router.learned_source_ms();
            if lq > self.settings.learned_queue_ms + 0.5 {
                self.settings.learned_queue_ms = lq;
                self.mark_dirty();
            }
            if ls > self.settings.learned_source_ms + 0.5 {
                self.settings.learned_source_ms = ls;
                self.mark_dirty();
            }

            if self.router.state() == RouterState::Error && !self.router.blocked() {
                let can_retry = self
                    .last_auto_restart
                    .map(|t| t.elapsed() >= Duration::from_secs(3))
                    .unwrap_or(true);
                if can_retry && self.auto_restart_count < 3 {
                    self.last_auto_restart = Some(Instant::now());
                    self.auto_restart_count += 1;
                    log::warn(format!(
                        "Router entered Error state; attempting recovery #{} ({})",
                        self.auto_restart_count,
                        self.router.last_error().unwrap_or_default()
                    ));
                    self.evaluate(false);
                    need_state = true;
                }
            } else if self.router.state() == RouterState::Running && !self.router.suspended() {
                if self.last_health_log.elapsed() >= Duration::from_secs(120) {
                    self.last_health_log = Instant::now();
                    log::info(format!(
                        "Audio health: lat={:.1}ms drift=({:+.0},{:+.0})ppm underruns={}",
                        self.router.latency_ms(),
                        self.router.game_drift_ppm(),
                        self.router.chat_drift_ppm(),
                        self.router.underruns()
                    ));
                }
            }
        }

        // 7. Debounced settings save
        if let Some(since) = self.save_dirty_since {
            if since.elapsed() >= Duration::from_millis(800) {
                self.save_now();
            }
        }

        if need_state {
            self.push_event(ControllerEvent::StateChanged);
        } else if need_live || self.meters_enabled {
            self.push_event(ControllerEvent::LiveChanged);
        }
    }

    fn handle_knob_action(&mut self, act: KnobAction) {
        let step = self.settings.knob_step.clamp(0.01, 0.25);
        match act.kind {
            KnobActionKind::Rotate => {
                let sign = if self.settings.knob_invert { -1.0 } else { 1.0 };
                self.step_specific_target(act.target, step * (act.direction as f64) * sign);
            }
            KnobActionKind::Press => self.press_specific_target(act.target),
            KnobActionKind::CycleFunction => self.cycle_target(),
            KnobActionKind::MixTowardsGame => self.mix_nudge(-step),
            KnobActionKind::MixTowardsChat => self.mix_nudge(step),
            KnobActionKind::MixReset => {
                self.settings.mix = 0.0;
                self.apply_gains();
                self.mark_dirty();
                self.show_target_osd(KnobTarget::Mix);
            }
        }
    }

    pub fn step_specific_target(&mut self, target: KnobTarget, delta: f64) {
        match target {
            KnobTarget::Mix => self.mix_nudge(delta),
            KnobTarget::Volume => {
                if self.endpoint_vol.is_available() {
                    let next = (self.endpoint_vol.level() + delta).clamp(0.0, 1.0);
                    self.endpoint_vol.set_muted(false);
                    self.endpoint_vol.set_level(next);
                    self.show_target_osd(KnobTarget::Volume);
                }
            }
            KnobTarget::Game => {
                self.settings.game_muted = false;
                self.settings.game_volume = (self.settings.game_volume + delta).clamp(0.0, 1.0);
                self.apply_gains();
                self.mark_dirty();
                self.show_target_osd(KnobTarget::Game);
            }
            KnobTarget::Chat => {
                self.settings.chat_muted = false;
                self.settings.chat_volume = (self.settings.chat_volume + delta).clamp(0.0, 1.0);
                self.apply_gains();
                self.mark_dirty();
                self.show_target_osd(KnobTarget::Chat);
            }
        }
    }

    pub fn press_specific_target(&mut self, target: KnobTarget) {
        let now = Instant::now();
        if self.settings.knob_double_press_cycles {
            if let Some(prev) = self.last_mute_backup.take() {
                if prev.at.elapsed() <= Duration::from_millis(450) {
                    // Revert the first press's mute so double-press cleanly cycles without leaving channels muted
                    self.settings.game_muted = prev.game_muted;
                    self.settings.chat_muted = prev.chat_muted;
                    if self.endpoint_vol.is_available() {
                        self.endpoint_vol.set_muted(prev.output_muted);
                    }
                    self.apply_gains();
                    self.cycle_target();
                    return;
                }
            }
        }

        self.last_mute_backup = Some(MuteBackup {
            at: now,
            game_muted: self.settings.game_muted,
            chat_muted: self.settings.chat_muted,
            output_muted: self.endpoint_vol.muted(),
        });

        match target {
            KnobTarget::Mix => {
                if self.settings.knob_mix_press_mutes {
                    // 1 press on Mix mutes everything (master + channels) or unmutes everything cleanly
                    let any_muted = self.settings.game_muted && self.settings.chat_muted
                        && (!self.endpoint_vol.is_available() || self.endpoint_vol.muted());
                    let next = !any_muted;
                    self.settings.game_muted = next;
                    self.settings.chat_muted = next;
                    if self.endpoint_vol.is_available() {
                        self.endpoint_vol.set_muted(next);
                    }
                    self.apply_gains();
                    self.mark_dirty();
                    self.show_target_osd(KnobTarget::Volume);
                } else {
                    self.cycle_target();
                }
            }
            KnobTarget::Volume => {
                if self.endpoint_vol.is_available() {
                    let next = !self.endpoint_vol.muted();
                    self.endpoint_vol.set_muted(next);
                    self.show_target_osd(KnobTarget::Volume);
                }
            }
            KnobTarget::Game => {
                self.settings.game_muted = !self.settings.game_muted;
                self.apply_gains();
                self.mark_dirty();
                self.show_target_osd(KnobTarget::Game);
            }
            KnobTarget::Chat => {
                self.settings.chat_muted = !self.settings.chat_muted;
                self.apply_gains();
                self.mark_dirty();
                self.show_target_osd(KnobTarget::Chat);
            }
        }
    }

    pub fn cycle_target(&mut self) {
        let cycle = if self.settings.knob_cycle.is_empty() {
            vec![KnobTarget::Mix, KnobTarget::Volume]
        } else {
            self.settings.knob_cycle.clone()
        };
        let pos = cycle.iter().position(|t| *t == self.settings.knob_target);
        let next = match pos {
            Some(i) => cycle[(i + 1) % cycle.len()],
            None => cycle[0],
        };
        self.set_knob_target(next, true);
    }

    pub fn set_knob_target(&mut self, target: KnobTarget, show_osd: bool) {
        if self.settings.knob_target != target {
            self.settings.knob_target = target;
            self.mark_dirty();
        }
        if show_osd && self.settings.show_osd {
            self.push_event(ControllerEvent::Osd(OsdInfo {
                kind: OsdKind::Function,
                title: loc::get("Tray.KnobSubMenu"),
                value_text: Self::target_label(target),
                value: 0.0,
                muted: false,
            }));
        }
    }

    pub fn mix_nudge(&mut self, delta: f64) {
        let raw = (self.settings.mix + delta).clamp(-1.0, 1.0);
        self.settings.mix = math::snap_center(raw);
        self.apply_gains();
        self.mark_dirty();
        self.show_target_osd(KnobTarget::Mix);
    }

    pub fn set_mix(&mut self, mix: f64, show_osd: bool) {
        self.settings.mix = math::snap_center(mix.clamp(-1.0, 1.0));
        self.apply_gains();
        self.mark_dirty();
        if show_osd {
            self.show_target_osd(KnobTarget::Mix);
        }
    }

    pub fn show_target_osd(&self, target: KnobTarget) {
        if !self.settings.show_osd {
            return;
        }
        let info = match target {
            KnobTarget::Mix => OsdInfo {
                kind: OsdKind::Mix,
                title: loc::get("Target.Mix"),
                value_text: Self::format_mix_text(self.settings.mix),
                value: self.settings.mix,
                muted: false,
            },
            KnobTarget::Volume => {
                let muted = self.endpoint_vol.muted();
                let lvl = if self.endpoint_vol.is_available() { self.endpoint_vol.level() } else { 1.0 };
                OsdInfo {
                    kind: OsdKind::Volume,
                    title: loc::get("Target.Volume"),
                    value_text: if muted { loc::get("Common.Muted") } else { math::percent(lvl) },
                    value: lvl,
                    muted,
                }
            }
            KnobTarget::Game => OsdInfo {
                kind: OsdKind::Game,
                title: loc::get("Common.Game"),
                value_text: if self.settings.game_muted {
                    loc::get("Common.Muted")
                } else {
                    math::percent(self.settings.game_volume)
                },
                value: self.settings.game_volume,
                muted: self.settings.game_muted,
            },
            KnobTarget::Chat => OsdInfo {
                kind: OsdKind::Chat,
                title: loc::get("Common.Chat"),
                value_text: if self.settings.chat_muted {
                    loc::get("Common.Muted")
                } else {
                    math::percent(self.settings.chat_volume)
                },
                value: self.settings.chat_volume,
                muted: self.settings.chat_muted,
            },
        };
        self.push_event(ControllerEvent::Osd(info));
    }

    pub fn handle_ui_command(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::Ready => {
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::HideWindow => {}
            UiCommand::SetTab { tab } => {
                self.active_tab = tab;
            }
            UiCommand::SetWizardStep { step } => {
                self.wizard_step = step.clamp(1, 3);
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetMix { percent } => {
                self.set_mix(percent / 100.0, false);
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::ResetMix => {
                self.set_mix(0.0, false);
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::SetGameVolume { percent } => {
                self.settings.game_volume = (percent / 100.0).clamp(0.0, 1.0);
                self.apply_gains();
                self.mark_dirty();
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::SetChatVolume { percent } => {
                self.settings.chat_volume = (percent / 100.0).clamp(0.0, 1.0);
                self.apply_gains();
                self.mark_dirty();
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::SetGameMuted { muted } => {
                self.settings.game_muted = muted;
                self.apply_gains();
                self.mark_dirty();
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::SetChatMuted { muted } => {
                self.settings.chat_muted = muted;
                self.apply_gains();
                self.mark_dirty();
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::SetOutputVolume { percent } => {
                if self.endpoint_vol.is_available() {
                    self.endpoint_vol.set_level((percent / 100.0).clamp(0.0, 1.0));
                    self.push_event(ControllerEvent::LiveChanged);
                }
            }
            UiCommand::SetOutputMuted { muted } => {
                if self.endpoint_vol.is_available() {
                    self.endpoint_vol.set_muted(muted);
                    self.push_event(ControllerEvent::LiveChanged);
                }
            }
            UiCommand::SetAppChannel { key, channel } => {
                let r = self.settings.apps.entry(key).or_insert_with(AppRule::default);
                r.channel = channel;
                self.rebuild_rules();
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetSystemChannel { channel } => {
                self.settings.system_sounds_channel = channel;
                self.rebuild_rules();
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::ResetAppAuto { key } => {
                if let Some(r) = self.settings.apps.get_mut(&key) {
                    r.channel = None;
                }
                self.rebuild_rules();
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetAppVolume { key, percent } => {
                let r = self.settings.apps.entry(key).or_insert_with(AppRule::default);
                r.volume = (percent / 100.0).clamp(0.0, 1.0);
                self.sessions.request_rescan();
                self.mark_dirty();
                self.push_event(ControllerEvent::LiveChanged);
            }
            UiCommand::PickAddApp => {
                let title = loc::get("Apps.PickTitle");
                let filter = loc::get("Apps.PickFilter");
                if let Some(path) = dialogs::open_file_dialog(HWND::default(), &title, &filter) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let key = stem.to_ascii_lowercase();
                        let r = self.settings.apps.entry(key).or_insert_with(AppRule::default);
                        r.display_name = Some(stem.to_string());
                        r.exe_path = Some(path.to_string_lossy().into_owned());
                        if r.channel.is_none() {
                            r.channel = Some(Channel::Chat);
                        }
                        self.rebuild_rules();
                        self.mark_dirty();
                        self.push_event(ControllerEvent::StateChanged);
                    }
                }
            }
            UiCommand::SetKnobTarget { target } => {
                self.set_knob_target(target, false);
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetKnobCycle { cycle } => {
                self.settings.knob_cycle = if cycle.is_empty() { vec![KnobTarget::Mix, KnobTarget::Volume] } else { cycle };
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetKnobOptions {
                double_press,
                ctrl_press,
                ctrl_alt,
                enabled,
                invert,
                mix_press_mutes,
            } => {
                self.settings.knob_double_press_cycles = double_press;
                self.settings.knob_ctrl_press_cycles = ctrl_press;
                self.settings.knob_ctrl_alternate = ctrl_alt;
                self.settings.knob_enabled = enabled;
                self.settings.knob_invert = invert;
                self.settings.knob_mix_press_mutes = mix_press_mutes;
                self.knob.update_config(KnobConfig::from_settings(&self.settings));
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetKnobStartFunction { func } => {
                self.settings.knob_start_function = func;
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetKnobStep { percent } => {
                self.settings.knob_step = (percent / 100.0).clamp(0.01, 0.25);
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::CaptureHotkey => {
                *self.capturing_target.lock().unwrap() = Some("hotkey".to_string());
                *self.capture_prompt.lock().unwrap() = Some(loc::get("Knob.CapturePrompt"));
                let wake = self.wake.clone();
                let cap = self.captured_input.clone();
                self.knob.set_capture_next(Some(Box::new(move |vk, mods| {
                    if let Ok(mut g) = cap.lock() {
                        *g = Some((vk, mods));
                    }
                    wake.wake();
                })));
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::ResetHotkey => {
                self.settings.knob_hotkey_key = 0x4D;
                self.settings.knob_hotkey_modifiers = HotkeyModifiers::CTRL.union(HotkeyModifiers::ALT);
                self.settings.knob_hotkey_custom = false;
                self.knob.update_config(KnobConfig::from_settings(&self.settings));
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::CaptureKey { which } => {
                *self.capturing_target.lock().unwrap() = Some(which);
                *self.capture_prompt.lock().unwrap() = Some(loc::get("Knob.CapturePrompt"));
                let wake = self.wake.clone();
                let cap = self.captured_input.clone();
                self.knob.set_capture_next(Some(Box::new(move |vk, mods| {
                    if let Ok(mut g) = cap.lock() {
                        *g = Some((vk, mods));
                    }
                    wake.wake();
                })));
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetOutputDevice { id } => {
                self.settings.output_device_id = id;
                self.settings.output_chosen_by_user = self.settings.output_device_id.is_some();
                self.mark_dirty();
                self.evaluate(true);
            }
            UiCommand::SetEngineMode { mode } => {
                self.settings.mode = mode;
                self.mark_dirty();
                self.evaluate(true);
            }
            UiCommand::SetAudioOptions {
                set_defaults,
                route_apps,
                game_direct,
            } => {
                self.settings.set_windows_defaults = set_defaults;
                self.settings.route_apps_to_devices = route_apps;
                self.settings.game_direct = game_direct;
                self.mark_dirty();
                self.evaluate(false);
            }
            UiCommand::SetLatency { latency } => {
                self.settings.latency = latency;
                self.settings.learned_queue_ms = 0.0;
                self.settings.learned_source_ms = 0.0;
                self.mark_dirty();
                if self.virtual_active {
                    self.router.stop();
                }
                self.evaluate(false);
            }
            UiCommand::SetDuckingDisabled { disabled } => {
                if !self.dry_run {
                    system_integration::set_ducking_disabled(disabled);
                }
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::DriverAction | UiCommand::BannerInstall => {
                self.start_elevated_setup(true, false, driver_setup::cached_pack());
            }
            UiCommand::RepairAudio | UiCommand::BannerRepair => {
                self.start_elevated_setup(false, true, None);
            }
            UiCommand::DismissSetupBanner => {
                self.setup_banner = None;
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetAutostart { enabled } => {
                self.settings.start_with_windows = enabled;
                if !self.dry_run {
                    system_integration::autostart_set(enabled);
                }
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetStartMinimized { enabled } => {
                self.settings.start_minimized = enabled;
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetShowOsd { enabled } => {
                self.settings.show_osd = enabled;
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetRestoreVolumes { enabled } => {
                self.settings.restore_volumes_on_exit = enabled;
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetTheme { theme } => {
                self.settings.theme = theme;
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetLanguage { lang } | UiCommand::WizardLang { lang } => {
                self.settings.language = lang;
                loc::set_language(lang);
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::SetUpdateMode { mode } => {
                self.settings.update_mode = mode;
                self.mark_dirty();
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::UpdateAction => {
                let st = self.updater.snapshot();
                let wake = self.wake.clone();
                if st.available_version.is_some() {
                    self.updater.install_async(move || wake.wake());
                } else {
                    self.updater.check_async(true, move || wake.wake());
                }
                self.push_event(ControllerEvent::StateChanged);
            }
            UiCommand::OpenWizard => {
                self.push_event(ControllerEvent::OpenWizard);
            }
            UiCommand::TestOsd => {
                self.push_event(ControllerEvent::Osd(OsdInfo {
                    kind: OsdKind::Mix,
                    title: loc::get("Target.Mix"),
                    value_text: Self::format_mix_text(self.settings.mix),
                    value: self.settings.mix,
                    muted: false,
                }));
            }
            UiCommand::RunDiag => {
                if !self.diag_busy.swap(true, Ordering::AcqRel) {
                    let busy = self.diag_busy.clone();
                    let out = self.diag_text.clone();
                    let wake = self.wake.clone();
                    std::thread::spawn(move || {
                        let rep = crate::diagnostics::run(true, false);
                        if let Ok(mut g) = out.lock() {
                            *g = rep.text;
                        }
                        busy.store(false, Ordering::Release);
                        wake.wake();
                    });
                    self.push_event(ControllerEvent::StateChanged);
                }
            }
            UiCommand::OpenDataFolder => {
                dialogs::shell_open(&log::data_dir().to_string_lossy(), None);
            }
            UiCommand::OpenUrl { url } => {
                if url.starts_with("https://") {
                    dialogs::shell_open(&url, None);
                }
            }
            UiCommand::WizardFinish {
                skip_driver,
                output_id,
                mode,
                start_with_windows,
                disable_ducking,
            } => {
                self.settings.first_run_done = true;
                self.settings.output_device_id = output_id;
                self.settings.output_chosen_by_user = self.settings.output_device_id.is_some();
                self.settings.mode = mode;
                self.settings.start_with_windows = start_with_windows;
                if !self.dry_run {
                    system_integration::autostart_set(start_with_windows);
                    system_integration::set_ducking_disabled(disable_ducking);
                }
                self.save_now();
                if !skip_driver && !driver_setup::cables_present() && !self.dry_run {
                    self.start_elevated_setup(true, false, driver_setup::cached_pack());
                } else {
                    self.evaluate(true);
                }
                self.push_event(ControllerEvent::WizardClosed);
                self.push_event(ControllerEvent::ShowMain);
            }
        }
    }

    fn start_elevated_setup(&mut self, install: bool, repair: bool, pack: Option<PathBuf>) {
        if self.dry_run || self.setup_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            self.setup_busy.store(false, Ordering::Release);
            return;
        };
        let out = self.setup_result.clone();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let res = driver_setup::run_elevated_blocking(&exe, install, repair, pack.as_deref());
            if let Ok(mut g) = out.lock() {
                *g = Some((res, install));
            }
            wake.wake();
        });
        self.push_event(ControllerEvent::StateChanged);
    }

    pub fn is_dark(&self) -> bool {
        dialogs::is_dark_theme(self.settings.theme)
    }

    pub fn tray_status(&self) -> TrayStatus {
        let mix_label = Self::format_mix_text(self.settings.mix);
        let tip = format!(
            "DualMix — {} · {}",
            Self::target_label(self.settings.knob_target),
            mix_label
        );
        TrayStatus {
            tooltip: tip,
            mix_label,
            target: self.settings.knob_target,
            autostart: self.settings.start_with_windows,
            dark: self.is_dark(),
        }
    }

    pub fn build_live_patch(&self) -> LivePatch {
        let snap = self.sessions.snapshot();
        let app_peaks = snap
            .iter()
            .map(|a| (a.key.clone(), a.peak, (a.relative_volume * 100.0).round() as i32, a.active))
            .collect();
        let (engine_chip, engine_text, latency_text, format_text, tech, qual) = self.engine_texts();
        LivePatch {
            mix_percent: (self.settings.mix * 100.0).round() as i32,
            mix_text: Self::format_mix_text(self.settings.mix),
            game_level_text: if self.settings.game_muted {
                loc::get("Common.Muted")
            } else {
                math::percent(self.settings.game_volume)
            },
            chat_level_text: if self.settings.chat_muted {
                loc::get("Common.Muted")
            } else {
                math::percent(self.settings.chat_volume)
            },
            game_volume_percent: (self.settings.game_volume * 100.0).round() as i32,
            chat_volume_percent: (self.settings.chat_volume * 100.0).round() as i32,
            game_muted: self.settings.game_muted,
            chat_muted: self.settings.chat_muted,
            output_volume_available: self.endpoint_vol.is_available(),
            output_volume_percent: (self.endpoint_vol.level() * 100.0).round() as i32,
            output_muted: self.endpoint_vol.muted(),
            game_peak: if self.virtual_active { self.router.game_peak() } else { self.sessions.game_peak() },
            chat_peak: if self.virtual_active { self.router.chat_peak() } else { self.sessions.chat_peak() },
            output_peak: self.router.output_peak(),
            app_peaks,
            router_running: self.router.is_running(),
            router_idle: self.router.suspended(),
            engine_chip,
            engine_text,
            latency_text,
            format_text,
            technical_details: tech,
            quality_summary: qual,
            knob_target: self.settings.knob_target,
            knob_target_text: Self::target_label(self.settings.knob_target),
            last_key_seen: self.last_key_seen.lock().map(|g| g.clone()).unwrap_or_default(),
            keyboard_status: self.keyboard_status_text(),
        }
    }

    pub fn build_ui_state(&self) -> UiState {
        let auto_lbl = self
            .active_output
            .as_ref()
            .filter(|_| !self.settings.output_chosen_by_user)
            .map(|d| loc::fmt("Audio.AutoOption", &[&d.name]))
            .unwrap_or_else(|| loc::get("Audio.AutoOptionEmpty"));
        let mut choices = vec![DeviceChoice {
            id: None,
            label: auto_lbl,
        }];
        for d in &self.render_cache {
            if !d.is_vb_cable() {
                choices.push(DeviceChoice {
                    id: Some(d.id.clone()),
                    label: d.name.clone(),
                });
            }
        }

        let live = self.build_live_patch();
        let snap = self.sessions.snapshot();
        let mut apps: Vec<UiAppRow> = Vec::with_capacity(snap.len() + 1);
        for a in snap.iter() {
            let initial = a
                .display_name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".to_string());
            let auto_ch = Self::channel_label(a.auto_channel);
            let subtitle = if a.is_system_sounds {
                loc::get("Apps.SystemSounds")
            } else if a.is_explicit {
                a.devices.clone()
            } else {
                format!("{} ({auto_ch})", loc::get("Apps.Sub.Auto"))
            };
            apps.push(UiAppRow {
                key: a.key.clone(),
                display_name: a.display_name.clone(),
                initial,
                subtitle,
                hint: a.devices.clone(),
                channel: a.channel,
                is_explicit: a.is_explicit,
                is_system: a.is_system_sounds,
                active: a.active,
                fixed_output: a.fixed_output,
                volume_percent: (a.relative_volume * 100.0).round() as i32,
                peak: a.peak,
            });
        }
        if !apps.iter().any(|r| r.key == SYSTEM_SOUNDS_KEY) {
            apps.push(UiAppRow {
                key: SYSTEM_SOUNDS_KEY.to_string(),
                display_name: loc::get("Apps.SystemSounds"),
                initial: "S".to_string(),
                subtitle: loc::get("Apps.SystemSounds"),
                hint: String::new(),
                channel: self.settings.system_sounds_channel,
                is_explicit: true,
                is_system: true,
                active: false,
                fixed_output: false,
                volume_percent: 100,
                peak: 0.0,
            });
        }

        let (sev, st_title, st_msg, needs_drv, needs_rep) = self.status_summary();
        let (upd_title, upd_text, upd_btn, upd_busy) = self.updater.ui_summary();
        let (game_line, chat_line) = self.device_lines();

        UiState {
            mode: if self.mode_wizard { "wizard" } else { "main" }.to_string(),
            wizard_step: self.wizard_step,
            active_tab: self.active_tab,
            dark: self.is_dark(),
            lang: if loc::current() == loc::Lang::It { "it" } else { "en" }.to_string(),
            version: self.version.clone(),
            data_dir: log::data_dir().to_string_lossy().into_owned(),
            severity: sev,
            status_title: st_title,
            status_message: st_msg,
            needs_driver: needs_drv,
            needs_repair: needs_rep,
            setup_busy: self.setup_busy.load(Ordering::Acquire),
            setup_banner: self.setup_banner.clone(),
            engine_mode: self.settings.mode,
            virtual_active: self.virtual_active,
            router_running: live.router_running,
            router_idle: live.router_idle,
            virtual_cables: self.virtual_cables,
            game_via_bus: self.game_bus.is_some() && !self.settings.game_direct,
            engine_chip: live.engine_chip,
            engine_text: live.engine_text,
            output_name: self
                .active_output
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| loc::get("Common.None")),
            format_text: live.format_text,
            latency_text: live.latency_text,
            game_device_line: game_line,
            chat_device_line: chat_line,
            game_device_name: self
                .game_bus
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| loc::get("Common.None")),
            chat_device_name: self
                .chat_bus
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| loc::get("Common.None")),
            technical_details: live.technical_details,
            quality_summary: live.quality_summary,
            output_choices: choices,
            selected_output_id: if self.settings.output_chosen_by_user {
                self.settings.output_device_id.clone()
            } else {
                None
            },
            set_windows_defaults: self.settings.set_windows_defaults,
            route_apps_to_devices: self.settings.route_apps_to_devices,
            game_direct: self.settings.game_direct,
            defaults_text: if self.windows_defaults_applied {
                let real = self
                    .active_output
                    .as_ref()
                    .map(|d| d.name.as_str())
                    .unwrap_or("Output");
                if self.game_bus.is_some() && !self.settings.game_direct {
                    loc::get("Audio.DefaultsDual")
                } else {
                    loc::fmt("Audio.DefaultsSingle", &[real])
                }
            } else {
                loc::get("Common.None")
            },
            latency: self.settings.latency,
            no_ducking: system_integration::is_ducking_disabled(),
            vb_cables_present: self.virtual_cables > 0,
            mix_percent: live.mix_percent,
            mix_text: live.mix_text,
            game_level_text: live.game_level_text,
            chat_level_text: live.chat_level_text,
            game_volume_percent: live.game_volume_percent,
            chat_volume_percent: live.chat_volume_percent,
            game_muted: live.game_muted,
            chat_muted: live.chat_muted,
            output_volume_available: live.output_volume_available,
            output_volume_percent: live.output_volume_percent,
            output_muted: live.output_muted,
            game_peak: live.game_peak,
            chat_peak: live.chat_peak,
            output_peak: live.output_peak,
            apps,
            knob_target: self.settings.knob_target,
            knob_target_text: live.knob_target_text,
            knob_hotkey_text: knob::chord_name(self.settings.knob_hotkey_key, self.settings.knob_hotkey_modifiers),
            knob_cycle: self.settings.knob_cycle.clone(),
            knob_cycle_summary: self
                .settings
                .knob_cycle
                .iter()
                .map(|t| Self::target_label(*t))
                .collect::<Vec<_>>()
                .join(" → "),
            knob_double_press: self.settings.knob_double_press_cycles,
            knob_ctrl_press: self.settings.knob_ctrl_press_cycles,
            knob_ctrl_alt: self.settings.knob_ctrl_alternate,
            knob_enabled: self.settings.knob_enabled,
            knob_invert: self.settings.knob_invert,
            knob_mix_press_mutes: self.settings.knob_mix_press_mutes,
            knob_start_function: self.settings.knob_start_function,
            knob_step_percent: (self.settings.knob_step * 100.0).round() as i32,
            key_mix_game_name: knob::key_name(self.settings.key_mix_game),
            key_mix_chat_name: knob::key_name(self.settings.key_mix_chat),
            key_mix_reset_name: knob::key_name(self.settings.key_mix_reset),
            capturing_target: self.capturing_target.lock().ok().and_then(|g| g.clone()),
            capture_prompt: self.capture_prompt.lock().ok().and_then(|g| g.clone()),
            keyboard_status: live.keyboard_status,
            last_key_seen: live.last_key_seen,
            autostart: self.settings.start_with_windows,
            start_minimized: self.settings.start_minimized,
            show_osd: self.settings.show_osd,
            restore_volumes_on_exit: self.settings.restore_volumes_on_exit,
            theme: self.settings.theme,
            language_pref: self.settings.language,
            update_mode: self.settings.update_mode,
            update_title: upd_title,
            update_text: upd_text,
            update_button_text: upd_btn,
            update_busy: upd_busy,
            diag_text: self.diag_text.lock().map(|g| g.clone()).unwrap_or_default(),
            diag_busy: self.diag_busy.load(Ordering::Acquire),
        }
    }

    fn status_summary(&self) -> (StatusSeverity, String, String, bool, bool) {
        if self.router.state() == RouterState::Error {
            return (
                StatusSeverity::Error,
                loc::get("Status.VirtualPaused.Title"),
                self.router
                    .last_error()
                    .unwrap_or_else(|| loc::get("Status.VirtualPaused.ErrBody")),
                false,
                true,
            );
        }
        if self.settings.mode == EngineMode::VirtualDevices && self.virtual_cables == 0 {
            return (
                StatusSeverity::Warning,
                loc::get("Status.NoDriver.Title"),
                loc::get("Status.NoDriver.Body"),
                true,
                false,
            );
        }
        let out_name = self
            .active_output
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| loc::get("Common.NoOutput"));
        if self.virtual_active && self.router.suspended() {
            return (
                StatusSeverity::Info,
                loc::get("Status.VirtualPaused.Title"),
                loc::get("Status.VirtualPaused.IdleBody"),
                false,
                false,
            );
        }
        if self.virtual_active {
            if self.game_bus.is_some() && !self.settings.game_direct {
                return (
                    StatusSeverity::Success,
                    loc::get("Status.ReadyDual.Title"),
                    loc::fmt("Status.ReadyDual.Body", &[&out_name]),
                    false,
                    false,
                );
            }
            return (
                StatusSeverity::Success,
                loc::get("Status.ReadySingle.Title"),
                loc::fmt("Status.ReadySingle.Body", &[&out_name]),
                false,
                false,
            );
        }
        (
            StatusSeverity::Success,
            loc::get("Status.PerAppMode.Title"),
            loc::fmt("Status.PerAppMode.Body", &[&out_name]),
            false,
            false,
        )
    }

    fn engine_texts(&self) -> (String, String, String, String, String, String) {
        if self.virtual_active {
            let lat = format!("{:.1} ms", self.router.latency_ms());
            let chip = if self.router.suspended() {
                loc::get("Mixer.ChipActiveIdle")
            } else {
                loc::fmt("Mixer.ChipActiveLat", &[&lat])
            };
            let fmt = self
                .router
                .output_format()
                .map(|f| format::describe(&f))
                .unwrap_or_else(|| "-".to_string());
            let out_name = self
                .active_output
                .as_ref()
                .map(|d| d.name.as_str())
                .unwrap_or("-");
            let game_line = if let Some(ref g) = self.game_bus {
                if !self.settings.game_direct {
                    loc::fmt(
                        "Status.Tech.GameBus",
                        &[&g.name, &loc::signed1(self.router.game_drift_ppm())],
                    )
                } else {
                    loc::get("Status.Tech.GameDirect")
                }
            } else {
                loc::get("Status.Tech.GameDirect")
            };
            let chat_name = self.chat_bus.as_ref().map(|d| d.name.as_str()).unwrap_or("-");
            let idle_str = if self.router.suspended() { loc::get("Status.Tech.Idle") } else { String::new() };
            let nort_str = if !self.router.real_time_scheduling() { loc::get("Status.Tech.NoRt") } else { String::new() };
            let tech = loc::fmt(
                "Status.Tech.Summary",
                &[
                    out_name,
                    &game_line,
                    chat_name,
                    &loc::signed1(self.router.chat_drift_ppm()),
                    &self.router.underruns().to_string(),
                    "",
                    &idle_str,
                    &nort_str,
                ],
            );
            let qual = loc::fmt("Audio.QualitySummaryFmt", &[&fmt, &lat]);
            let text = self.router.last_error().unwrap_or_else(|| chip.clone());
            (chip, text, lat, fmt, tech, qual)
        } else {
            let out_name = self
                .active_output
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| loc::get("Common.NoOutput"));
            (
                loc::get("Status.PerAppMode.Title"),
                loc::fmt("Status.PerAppMode.Body", &[&out_name]),
                "0 ms".to_string(),
                "-".to_string(),
                loc::get("Status.Tech.NotRunning"),
                loc::get("Status.PerAppMode.Title"),
            )
        }
    }

    fn device_lines(&self) -> (String, String) {
        let out_name = self
            .active_output
            .as_ref()
            .map(|d| d.name.as_str())
            .unwrap_or("-");
        let g = if self.virtual_active {
            match self.game_bus.as_ref() {
                Some(gb) if !self.settings.game_direct => loc::fmt("Mixer.ChannelDevice.Virtual", &[&gb.name]),
                _ => loc::fmt("Mixer.ChannelDevice.Direct", &[out_name]),
            }
        } else {
            loc::fmt("Mixer.ChannelDevice.PerApp", &[out_name])
        };
        let c = if self.virtual_active {
            let cname = self.chat_bus.as_ref().map(|d| d.name.as_str()).unwrap_or("-");
            loc::fmt("Mixer.ChannelDevice.Virtual", &[cname])
        } else {
            loc::fmt("Mixer.ChannelDevice.PerApp", &[out_name])
        };
        (g, c)
    }

    fn keyboard_status_text(&self) -> String {
        let failed = self.keyboard.failed_hotkeys();
        let active = self.keyboard.active_hotkeys();
        if failed.is_empty() {
            loc::fmt("Knob.Status.AllActive", &[&active.to_string()])
        } else {
            let total = active + failed.len();
            loc::fmt(
                "Knob.Status.SomeFailed",
                &[&active.to_string(), &total.to_string(), &failed.join(", ")],
            )
        }
    }

    pub fn format_mix_text(mix: f64) -> String {
        let pct = (mix * 100.0).round() as i32;
        if pct.abs() < 2 {
            loc::get("Common.Balanced")
        } else if pct < 0 {
            format!("{} +{}%", loc::get("Common.Game"), -pct)
        } else {
            format!("{} +{}%", loc::get("Common.Chat"), pct)
        }
    }

    pub fn target_label(t: KnobTarget) -> String {
        match t {
            KnobTarget::Mix => loc::get("Target.Mix"),
            KnobTarget::Volume => loc::get("Target.Volume"),
            KnobTarget::Game => loc::get("Target.Game"),
            KnobTarget::Chat => loc::get("Target.Chat"),
        }
    }

    fn channel_label(c: Channel) -> String {
        match c {
            Channel::Game => loc::get("Common.Game"),
            Channel::Chat => loc::get("Common.Chat"),
            Channel::Ignore => loc::get("Common.Direct"),
        }
    }
}
