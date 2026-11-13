//! Bridge between the controller and the HTML pages: the JSON state the pages render, and the commands they send.
//! Port of the C# page code-behind (MixerPage, AppsPage, KnobPage, AudioPage, GeneralPage, SetupWindow).

use std::collections::HashSet;

use dualmix_audio::devices::{self, Flow, Role};
use dualmix_audio::driver_setup;
use dualmix_core::knob;
use dualmix_core::loc;
use dualmix_core::log;
use dualmix_core::rules::{RuleSet, SYSTEM_SOUNDS_KEY};
use dualmix_core::settings::{
    Channel, EngineMode, KnobStartFunction, KnobTarget, LanguagePreference, LatencyMode, ThemePreference, UpdateMode,
};
use serde_json::{Value, json};

use crate::controller::{CaptureTarget, Controller, HostAction, target_name};
use crate::updater::UpdateState;

/// Device lists cached by `Controller::devices_gen` (enumerating endpoints is not free).
#[derive(Default)]
pub struct UiCache {
    seen_gen: u64,
    outputs: Vec<(String, String)>,
    auto_label: String,
    cables_present: bool,
}

fn short(name: &str) -> String {
    match name.find(" (") {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

fn channel_str(c: Channel) -> &'static str {
    match c {
        Channel::Game => "game",
        Channel::Chat => "chat",
        Channel::Ignore => "direct",
    }
}

fn target_str(t: KnobTarget) -> &'static str {
    match t {
        KnobTarget::Mix => "Mix",
        KnobTarget::Volume => "Volume",
        KnobTarget::Game => "Game",
        KnobTarget::Chat => "Chat",
    }
}

fn parse_target(s: &str) -> Option<KnobTarget> {
    match s {
        "Mix" => Some(KnobTarget::Mix),
        "Volume" => Some(KnobTarget::Volume),
        "Game" => Some(KnobTarget::Game),
        "Chat" => Some(KnobTarget::Chat),
        _ => None,
    }
}

impl UiCache {
    fn refresh(&mut self, c: &Controller) {
        if self.seen_gen == c.devices_gen && self.seen_gen != 0 {
            return;
        }
        self.seen_gen = c.devices_gen;
        let render: Vec<_> = devices::list_render().into_iter().filter(|d| !d.is_vb_cable()).collect();
        let pref = c.settings.defaults_backup.as_ref().and_then(|b| b.multimedia.clone()).or_else(|| devices::get_default_id(Role::Multimedia, Flow::Render));
        self.auto_label = match devices::find_preferred_output(&render, pref.as_deref()) {
            Some(a) => loc::fmt("Audio.AutoOption", &[&a.name]),
            None => loc::get("Audio.AutoOptionEmpty"),
        };
        self.outputs = render.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
        self.cables_present = driver_setup::cables_present();
    }

    pub fn invalidate(&mut self) {
        self.seen_gen = 0;
    }
}

/// The volatile part, pushed ~15 times a second while the window is visible.
pub fn live_json(c: &Controller) -> String {
    let apps: Vec<Value> = c.apps().iter().map(|a| json!([a.key, (a.peak as f64 * 1000.0).round() / 1000.0])).collect();
    json!({
        "t": "live",
        "gamePeak": c.game_peak(),
        "chatPeak": c.chat_peak(),
        "outPeak": c.output_peak(),
        "apps": apps,
        "chip": c.engine_chip(),
        "lastKey": c.last_key_seen,
    })
    .to_string()
}

fn describe(c: &Controller) -> Value {
    let s = &c.settings;
    let mut inv = s.knob_invert;
    let (mut l, mut r, p, n) = match s.knob_target {
        KnobTarget::Mix => (
            loc::get("Knob.Describe.MixL"),
            loc::get("Knob.Describe.MixR"),
            if s.knob_mix_press_mutes { loc::get("Knob.Describe.MixPressMute") } else { loc::get("Knob.Describe.MixPressReset") },
            loc::get("Knob.Describe.MixNote"),
        ),
        KnobTarget::Volume => {
            inv = false; // the rotation is handled by Windows itself
            (loc::get("Knob.Describe.VolL"), loc::get("Knob.Describe.VolR"), loc::get("Knob.Describe.VolPress"), loc::get("Knob.Describe.VolNote"))
        }
        KnobTarget::Game => (loc::get("Knob.Describe.GameL"), loc::get("Knob.Describe.GameR"), loc::get("Knob.Describe.GamePress"), loc::get("Knob.Describe.MixNote")),
        KnobTarget::Chat => (loc::get("Knob.Describe.ChatL"), loc::get("Knob.Describe.ChatR"), loc::get("Knob.Describe.ChatPress"), loc::get("Knob.Describe.MixNote")),
    };
    if inv {
        std::mem::swap(&mut l, &mut r);
    }
    json!({ "left": l, "right": r, "press": p, "note": n })
}

fn app_rows(c: &Controller) -> Vec<Value> {
    let rules = RuleSet::from_settings(&c.settings);
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let sub = |channel: Channel, explicit: bool, active: bool, fixed: bool, system: bool| -> String {
        let place = match channel {
            Channel::Game => loc::get("Common.Game"),
            Channel::Chat => loc::get("Common.Chat"),
            Channel::Ignore => loc::get("AppRow.DirectSub"),
        };
        let how = if explicit { loc::get("AppRow.Manual") } else { loc::get("AppRow.Auto") };
        let state = if !active { loc::get("AppRow.Inactive") } else if fixed { loc::get("AppRow.FixedOutput") } else { String::new() };
        if system { format!("{place}{state}") } else { format!("{place} · {how}{state}") }
    };
    for a in c.apps().iter() {
        seen.insert(a.key.to_lowercase());
        let name = if a.is_system_sounds { loc::get("Apps.SystemSounds") } else { a.display_name.clone() };
        rows.push(json!({
            "key": a.key, "name": name, "system": a.is_system_sounds, "active": a.active, "live": true,
            "channel": channel_str(a.channel), "explicit": a.is_explicit, "auto": channel_str(a.auto_channel),
            "volume": (a.relative_volume * 100.0).round(), "fixed": a.fixed_output,
            "sub": sub(a.channel, a.is_explicit, a.active, a.fixed_output, a.is_system_sounds),
            "hint": if a.fixed_output { loc::get("AppRow.FixedHint") } else { a.key.clone() },
        }));
    }
    let mut saved: Vec<_> = c.settings.apps.iter().filter(|(k, _)| !seen.contains(&k.to_lowercase())).collect();
    saved.sort_by_key(|(k, r)| r.display_name.clone().unwrap_or_else(|| k.to_string()).to_lowercase());
    for (key, rule) in saved {
        let system = key == SYSTEM_SOUNDS_KEY;
        let auto = rules.auto_channel(key, system);
        let channel = rule.channel.unwrap_or(auto);
        let name = if system { loc::get("Apps.SystemSounds") } else { rule.display_name.clone().unwrap_or_else(|| key.clone()) };
        rows.push(json!({
            "key": key, "name": name, "system": system, "active": false, "live": false,
            "channel": channel_str(channel), "explicit": rule.channel.is_some(), "auto": channel_str(auto),
            "volume": (rule.volume * 100.0).round(), "fixed": false,
            "sub": sub(channel, rule.channel.is_some(), false, false, system), "hint": key,
        }));
    }
    rows
}

pub fn state_json(c: &Controller, cache: &mut UiCache, wizard: bool) -> String {
    cache.refresh(c);
    let s = &c.settings;
    let (game_lvl, chat_lvl) = c.levels();
    let virtual_mode = s.mode == EngineMode::VirtualDevices;
    let up = c.updater.snapshot();
    let update_text = match up.state {
        UpdateState::Idle => {
            if s.update_mode == UpdateMode::Off { loc::get("General.UpdateIdleOff") } else { loc::get("General.UpdateIdleOn") }
        }
        UpdateState::Available => match up.available.as_ref().and_then(|a| a.notes.clone()) {
            Some(n) => format!("{} {n}", up.status_text),
            None => up.status_text.clone(),
        },
        _ => up.status_text.clone(),
    };
    let update_button = match (&up.available, &up.state) {
        (Some(a), st) if *st != UpdateState::UpToDate => loc::fmt("General.UpdateInstall", &[&a.version]),
        _ => loc::get("General.UpdateCheck"),
    };
    let (driver_text, driver_btn, driver_primary) = match c.virtual_cables {
        0 => (loc::get("Audio.Driver0Desc"), loc::get("Audio.DriverBtn.Install"), true),
        1 => (loc::fmt("Audio.Driver1Desc", &[&short(&c.chat_device_name())]), loc::get("Audio.DriverBtn.Upgrade"), true),
        _ => (loc::fmt("Audio.Driver2Desc", &[&c.game_device_name(), &c.chat_device_name()]), loc::get("Audio.DriverBtn.Reconfigure"), false),
    };
    let playback = if c.game_via_bus() { "\"DualMix Game\"".to_string() } else { loc::get("Audio.DefaultsRealCard") };
    let mut outputs = vec![json!({"id": "", "label": cache.auto_label})];
    outputs.extend(cache.outputs.iter().map(|(id, n)| json!({"id": id, "label": n})));
    let output_sel = if s.output_chosen_by_user { s.output_device_id.clone().unwrap_or_default() } else { String::new() };
    let game_device = if c.game_via_bus() {
        loc::fmt("Mixer.GameAppOut", &[&short(&c.game_device_name())])
    } else if c.virtual_active() {
        loc::get("Mixer.GameDirectCard")
    } else {
        loc::get("Mixer.PerAppVol")
    };
    let chat_device = if c.virtual_active() { loc::fmt("Mixer.GameAppOut", &[&short(&c.chat_device_name())]) } else { loc::get("Mixer.PerAppVol") };
    let quality = if c.router_running() { loc::fmt("Audio.QualitySummaryFmt", &[&c.format_text(), &c.latency_text()]) } else { c.engine_text() };
    let cycle: Vec<&str> = s.knob_cycle.iter().map(|t| target_str(*t)).collect();
    let cycle_names: Vec<String> = s.knob_cycle.iter().map(|t| target_name(*t)).collect();
    let capture = |t: CaptureTarget| c.capture == Some(t);
    let key_label = |vk: i32, t: CaptureTarget| if capture(t) { loc::get("Knob.KeyPressPrompt") } else { knob::key_name(vk) };
    json!({
        "t": "state",
        "wizard": wizard,
        "lang": loc::current().code(),
        "dark": dualmix_ui::dialogs::is_dark_theme(s.theme),
        "version": crate::updater::current_version(),
        "status": {
            "severity": c.severity.as_str(), "title": c.status_title, "message": c.status_message,
            "needsDriver": c.needs_driver && virtual_mode, "needsRepair": c.needs_repair, "setupBusy": c.setup_busy,
        },
        "chip": c.engine_chip(),
        "mixer": {
            "mix": (s.mix * 100.0).round(), "mixText": c.mix_text(),
            "gameLevel": math_pct(game_lvl), "chatLevel": math_pct(chat_lvl),
            "gameVol": (s.game_volume * 100.0).round(), "chatVol": (s.chat_volume * 100.0).round(),
            "gameMuted": s.game_muted, "chatMuted": s.chat_muted,
            "gameDevice": game_device, "chatDevice": chat_device,
            "outputName": c.output_name(), "formatText": c.format_text(),
            "outAvail": c.output_volume_available(), "outVol": c.output_volume_percent(), "outMuted": c.output_muted(),
            "knobTargetText": target_name(s.knob_target), "hotkeyText": c.knob_hotkey_text(),
        },
        "apps": app_rows(c),
        "knob": {
            "target": target_str(s.knob_target), "cycle": cycle, "cycleSummary": loc::fmt("Knob.CycleSummary", &[&cycle_names.join(" → ")]),
            "doublePress": s.knob_double_press_cycles, "ctrlPress": s.knob_ctrl_press_cycles, "ctrlAlt": s.knob_ctrl_alternate,
            "enabled": s.knob_enabled, "invert": s.knob_invert, "mixPressMutes": s.knob_mix_press_mutes,
            "startFunction": format!("{:?}", s.knob_start_function), "step": (s.knob_step * 100.0).round(),
            "hotkeyText": if capture(CaptureTarget::Hotkey) { loc::get("Knob.HotkeyPress") } else { c.capture_message.clone().unwrap_or_else(|| c.knob_hotkey_text()) },
            "keyGame": key_label(s.key_mix_game, CaptureTarget::KeyGame),
            "keyChat": key_label(s.key_mix_chat, CaptureTarget::KeyChat),
            "keyReset": key_label(s.key_mix_reset, CaptureTarget::KeyReset),
            "describe": describe(c), "keyboardStatus": c.keyboard_status(), "lastKey": c.last_key_seen,
        },
        "audio": {
            "outputs": outputs, "outputSel": output_sel, "useVirtual": virtual_mode, "cables": c.virtual_cables,
            "driverText": driver_text, "driverButton": if c.setup_busy { loc::get("Audio.DriverBtn.Busy") } else { driver_btn }, "driverPrimary": driver_primary,
            "gameDirect": s.game_direct, "gameDirectVisible": c.virtual_cables >= 2, "setDefaults": s.set_windows_defaults,
            "routeApps": s.route_apps_to_devices, "defaultsText": loc::fmt("Audio.DefaultsCardText", &[&playback]),
            "latency": format!("{:?}", s.latency), "quality": quality, "technical": c.technical_details(),
            "ducking": crate::system_integration::is_ducking_disabled(),
            "setupInfo": c.setup_info.as_ref().map(|i| json!({"severity": i.severity.as_str(), "title": i.title, "message": i.message})),
        },
        "general": {
            "autostart": c.autostart_enabled, "startHidden": s.start_minimized, "osd": s.show_osd, "restoreVolumes": s.restore_volumes_on_exit,
            "theme": format!("{:?}", s.theme), "language": format!("{:?}", s.language), "updateMode": format!("{:?}", s.update_mode),
            "updateTitle": format!("DualMix {}", crate::updater::current_version()), "updateText": update_text,
            "updateButton": update_button, "updateBusy": c.updater.busy(), "dataPath": log::data_dir().display().to_string(),
            "diagText": c.diag_text.clone().unwrap_or_else(|| loc::get("General.DiagDesc")), "diagRunning": c.diag_running,
        },
        "wizardData": {
            "outputs": [], "vbReady": cache.cables_present, "mode": format!("{:?}", s.mode),
            "startWithWindows": if s.first_run_done { c.autostart_enabled } else { s.start_with_windows },
            "disableDucking": !s.first_run_done || crate::system_integration::is_ducking_disabled(),
        },
    })
    .to_string()
}

fn math_pct(v: f64) -> String {
    dualmix_core::math::percent(v)
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Executes a command sent by a page. Unknown commands are logged and ignored.
pub fn command(c: &mut Controller, v: &Value) {
    let cmd = st(v, "cmd");
    match cmd {
        // ---- Mixer
        "setMix" => c.set_mix_percent(f(v, "value")),
        "resetMix" => c.reset_mix(false),
        "allGame" => c.set_mix(-1.0, false),
        "allChat" => c.set_mix(1.0, false),
        "setGameVol" => c.set_channel_volume(Channel::Game, f(v, "value") / 100.0, false),
        "setChatVol" => c.set_channel_volume(Channel::Chat, f(v, "value") / 100.0, false),
        "toggleGameMute" => {
            let m = !c.settings.game_muted;
            c.set_game_muted(m);
        }
        "toggleChatMute" => {
            let m = !c.settings.chat_muted;
            c.set_chat_muted(m);
        }
        "setOutVol" => c.set_output_volume_percent(f(v, "value")),
        "toggleOutMute" => {
            let m = !c.output_muted();
            c.set_output_muted(m);
        }
        "installDriver" => c.run_setup(true, c.needs_repair, None),
        "repair" => {
            let install = c.needs_driver && c.settings.mode == EngineMode::VirtualDevices;
            c.run_setup(install, true, None);
        }
        // ---- Apps
        "rescan" => c.rescan_now(),
        "addApp" => c.actions.push(HostAction::PickApp),
        "setAppChannel" => {
            let key = st(v, "key").to_string();
            let ch = match st(v, "channel") {
                "game" => Channel::Game,
                "chat" => Channel::Chat,
                _ => Channel::Ignore,
            };
            if key == SYSTEM_SOUNDS_KEY {
                c.settings.system_sounds_channel = ch;
                c.rules_changed(true);
            } else {
                // Choosing the channel the app would get anyway keeps it "automatic".
                let auto = RuleSet::from_settings(&c.settings).auto_channel(&key, false);
                c.set_app_channel(&key, if ch == auto { None } else { Some(ch) });
            }
        }
        "resetAppChannel" => {
            let key = st(v, "key").to_string();
            if key != SYSTEM_SOUNDS_KEY {
                c.set_app_channel(&key, None);
            }
        }
        "setAppVolume" => {
            let key = st(v, "key").to_string();
            c.set_app_volume(&key, f(v, "value") / 100.0);
        }
        "forgetApp" => {
            let key = st(v, "key").to_string();
            c.forget_app(&key);
        }
        // ---- Knob
        "setKnobTarget" => {
            if let Some(t) = parse_target(st(v, "value")) {
                c.set_knob_target(t, false);
            }
        }
        "setKnobCycle" => {
            let list: Vec<KnobTarget> = v.get("value").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().and_then(parse_target)).collect()).unwrap_or_default();
            if !list.is_empty() {
                c.settings.knob_cycle = list;
                c.knob_settings_changed();
            } else {
                c.state_dirty = true; // the page puts the last box back
            }
        }
        "setKnobOption" => {
            let on = b(v, "value");
            match st(v, "name") {
                "doublePress" => c.settings.knob_double_press_cycles = on,
                "ctrlPress" => c.settings.knob_ctrl_press_cycles = on,
                "ctrlAlt" => c.settings.knob_ctrl_alternate = on,
                "enabled" => c.settings.knob_enabled = on,
                "invert" => c.settings.knob_invert = on,
                "mixPressMutes" => c.settings.knob_mix_press_mutes = on,
                _ => {}
            }
            c.knob_settings_changed();
        }
        "setKnobStart" => {
            c.settings.knob_start_function = match st(v, "value") {
                "Last" => KnobStartFunction::Last,
                "Mix" => KnobStartFunction::Mix,
                "Game" => KnobStartFunction::Game,
                "Chat" => KnobStartFunction::Chat,
                _ => KnobStartFunction::Volume,
            };
            c.schedule_save();
            c.state_dirty = true;
        }
        "setKnobStep" => {
            c.settings.knob_step = (f(v, "value") / 100.0).clamp(0.01, 0.25);
            c.knob_settings_changed();
        }
        "captureHotkey" => c.begin_capture(CaptureTarget::Hotkey),
        "captureKey" => c.begin_capture(match st(v, "which") {
            "Game" => CaptureTarget::KeyGame,
            "Chat" => CaptureTarget::KeyChat,
            _ => CaptureTarget::KeyReset,
        }),
        "cancelCapture" => c.cancel_capture(),
        "resetHotkey" => c.reset_hotkey(),
        // ---- Audio
        "setOutput" => {
            let id = st(v, "value");
            c.settings.output_chosen_by_user = !id.is_empty();
            if !id.is_empty() {
                c.settings.output_device_id = Some(id.to_string());
            }
            c.engine_settings_changed(true);
        }
        "setUseVirtual" => {
            c.settings.mode = if b(v, "value") { EngineMode::VirtualDevices } else { EngineMode::PerApp };
            c.engine_settings_changed(false);
        }
        "setAudioOption" => {
            let on = b(v, "value");
            let mut restart = false;
            match st(v, "name") {
                "gameDirect" => {
                    restart = on != c.settings.game_direct;
                    c.settings.game_direct = on;
                }
                "setDefaults" => c.settings.set_windows_defaults = on,
                "routeApps" => c.settings.route_apps_to_devices = on,
                _ => {}
            }
            c.engine_settings_changed(restart);
        }
        "setLatency" => {
            c.settings.latency = match st(v, "value") {
                "Minima" => LatencyMode::Minima,
                "Sicura" => LatencyMode::Sicura,
                _ => LatencyMode::Bilanciata,
            };
            // What was learned belonged to the previous choice.
            c.settings.learned_queue_ms = 0.0;
            c.settings.learned_source_ms = 0.0;
            c.engine_settings_changed(true);
        }
        "setDucking" => {
            crate::system_integration::set_ducking_disabled(b(v, "value"));
            c.state_dirty = true;
        }
        "driverButton" => match c.virtual_cables {
            0 => c.run_setup(true, c.needs_repair, None),
            1 => c.actions.push(HostAction::PickPack),
            _ => c.run_setup(false, false, None),
        },
        "dismissSetupInfo" => {
            c.setup_info = None;
            c.state_dirty = true;
        }
        // ---- General
        "setGeneralOption" => {
            let on = b(v, "value");
            match st(v, "name") {
                "autostart" => {
                    c.settings.start_with_windows = on;
                    crate::system_integration::autostart_set(on);
                    c.autostart_enabled = on;
                }
                "startHidden" => c.settings.start_minimized = on,
                "osd" => c.settings.show_osd = on,
                "restoreVolumes" => c.settings.restore_volumes_on_exit = on,
                _ => {}
            }
            c.schedule_save();
            c.state_dirty = true;
        }
        "setTheme" => {
            c.settings.theme = match st(v, "value") {
                "Light" => ThemePreference::Light,
                "Dark" => ThemePreference::Dark,
                _ => ThemePreference::System,
            };
            c.schedule_save();
            c.state_dirty = true;
            c.actions.push(HostAction::ThemeChanged);
        }
        "setLanguage" => {
            c.settings.language = match st(v, "value") {
                "English" => LanguagePreference::English,
                "Italian" => LanguagePreference::Italian,
                _ => LanguagePreference::Auto,
            };
            loc::set_language(c.settings.language);
            c.schedule_save();
            c.state_dirty = true;
            c.actions.push(HostAction::LanguageChanged);
        }
        "setUpdateMode" => {
            c.settings.update_mode = match st(v, "value") {
                "Off" => UpdateMode::Off,
                "Notify" => UpdateMode::Notify,
                _ => UpdateMode::Auto,
            };
            c.schedule_save();
            c.state_dirty = true;
        }
        "updateButton" => {
            let snap = c.updater.snapshot();
            if snap.available.is_some() && snap.state != UpdateState::UpToDate {
                c.updater.install(c.inbox());
            } else {
                c.updater.check(c.inbox());
            }
        }
        "openWizard" => c.actions.push(HostAction::OpenWizard),
        "testOsd" => {
            let i = c.mix_osd_info();
            c.actions.push(HostAction::Osd(i, true));
        }
        "runDiag" => {
            if !c.diag_running {
                c.diag_running = true;
                c.diag_text = Some(loc::get("General.DiagRunningText"));
                c.state_dirty = true;
                let ib = c.inbox();
                std::thread::spawn(move || {
                    let ok = std::env::current_exe().ok().and_then(|exe| {
                        std::process::Command::new(exe)
                            .args(["--diag", "--quick"])
                            .creation_flags_hidden()
                            .status()
                            .ok()
                            .map(|s| s.success())
                    });
                    ib.post(crate::controller::Ev::DiagDone(ok));
                });
            }
        }
        "openFolder" => dualmix_ui::dialogs::shell_open("explorer.exe", Some(&format!("\"{}\"", log::data_dir().display()))),
        "openUrl" => {
            let url = st(v, "value");
            if url.starts_with("https://") {
                dualmix_ui::dialogs::shell_open(url, None);
            }
        }
        // ---- Wizard
        "wizardLanguage" => {
            c.settings.language = match st(v, "value") {
                "English" => LanguagePreference::English,
                "Italian" => LanguagePreference::Italian,
                _ => LanguagePreference::Auto,
            };
            loc::set_language(c.settings.language);
            c.schedule_save();
            c.state_dirty = true;
            c.actions.push(HostAction::LanguageChanged);
        }
        "wizardFinish" => wizard_finish(c, v, false),
        "wizardSkip" => wizard_finish(c, v, true),
        // ---- window
        "exitApp" => c.actions.push(HostAction::Exit),
        other => log::warn(format!("UI: unknown command '{other}'")),
    }
}

trait HiddenCommand {
    fn creation_flags_hidden(&mut self) -> &mut Self;
}

impl HiddenCommand for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(driver_setup::CREATE_NO_WINDOW)
    }
}

/// Port of `SetupWindow.ApplyAndClose`.
fn wizard_finish(c: &mut Controller, v: &Value, skip_driver: bool) {
    let s = &mut c.settings;
    s.first_run_done = true;
    if !c.dry_run {
        let id = st(v, "output");
        s.output_chosen_by_user = !id.is_empty();
        if !id.is_empty() {
            s.output_device_id = Some(id.to_string());
        }
        let want_virtual = st(v, "mode") != "PerApp";
        s.mode = if want_virtual { EngineMode::VirtualDevices } else { EngineMode::PerApp };
        s.start_with_windows = b(v, "startWithWindows");
        crate::system_integration::autostart_set(s.start_with_windows);
        c.autostart_enabled = s.start_with_windows;
        if b(v, "disableDucking") && !crate::system_integration::is_ducking_disabled() {
            crate::system_integration::set_ducking_disabled(true);
        }
        c.save_now();
        let install = !skip_driver && want_virtual && !driver_setup::cables_present();
        if !install {
            c.mark_auto_setup_handled();
        }
        c.engine_settings_changed(true);
        if install {
            c.mark_auto_setup_handled();
            let repair = c.needs_repair;
            c.run_setup(true, repair, None);
        }
    }
    c.state_dirty = true;
    c.actions.push(HostAction::CloseWizard);
    c.actions.push(HostAction::ShowMain);
}
