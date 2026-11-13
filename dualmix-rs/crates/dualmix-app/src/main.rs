//! DualMix native Windows entry point (`DualMix.exe`), port of C# `Program.cs` / `App.xaml.cs` / `Supervisor.cs`.
//!
//! Threading model: one UI thread owns everything (controller, windows, tray, OSD) inside `HOST`. Window procedures
//! borrow it only for short, non-reentrant work; modal loops (tray menu, file dialogs) run with the borrow released.
//! Other threads talk to it only through the controller inbox (`WM_APP_WAKE`) and the web event queue.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod controller;
#[cfg(windows)]
mod diagnostics;
#[cfg(windows)]
mod system_integration;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod updater;

#[cfg(not(windows))]
fn main() {
    eprintln!("DualMix runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(app::win_main(&args));
}

#[cfg(windows)]
mod app {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use dualmix_core::loc;
    use dualmix_core::log;
    use dualmix_core::settings::{KnobTarget, SettingsStore, UpdateMode};
    use dualmix_ui::osd::OsdWindow;
    use dualmix_ui::tray::{TrayAction, TrayIcon, TrayStatus, WM_TRAY_ICON};
    use dualmix_ui::webwin::{self, WebEvent, WebWindow, WinKind};
    use serde_json::Value;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, WaitForMultipleObjects};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, MSG, PostMessageW, PostQuitMessage,
        RegisterClassExW, RegisterWindowMessageW, SetTimer, TranslateMessage, WM_ENDSESSION, WM_QUERYENDSESSION, WM_SETTINGCHANGE,
        WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use crate::controller::{Controller, HostAction, Inbox, WM_APP_WAKE};
    use crate::ui::{self, UiCache};

    const WM_APP_WEB: u32 = 0x8021;
    const WM_APP_SHOW: u32 = 0x8022;
    const WM_APP_EXIT: u32 = 0x8023;
    const TIMER_TICK: usize = 1;
    const TIMER_LIVE: usize = 2;
    const TIMER_RETRY: usize = 3;
    const SUPERVISOR_MUTEX: &str = "Local\\DualMix.Supervisor.7f3c2a";

    fn flag(args: &[String], f: &str) -> bool {
        args.iter().any(|a| a.eq_ignore_ascii_case(f))
    }

    fn value(args: &[String], f: &str) -> Option<String> {
        args.iter().position(|a| a.eq_ignore_ascii_case(f)).and_then(|i| args.get(i + 1)).cloned()
    }

    fn attach_console() {
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
    }

    pub fn win_main(args: &[String]) -> i32 {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        if flag(args, "--version") || flag(args, "-v") {
            attach_console();
            println!("DualMix {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        if flag(args, "--exit") {
            // Ask the running instance to quit (used by the setup/uninstall scripts).
            attach_console();
            if dualmix_ui::signal_exit_existing() {
                let start = Instant::now();
                while dualmix_ui::is_app_running() && start.elapsed() < Duration::from_secs(8) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            return 0;
        }
        let settings = SettingsStore::load();
        let lang_override = value(args, "--lang");
        loc::initialize(settings.language, lang_override.as_deref());

        if flag(args, "--setup") {
            // Elevated helper (driver installation / audio service repair): no UI, no single-instance lock.
            let _com = dualmix_audio::com::ComGuard::mta();
            return dualmix_audio::driver_setup::run_elevated(args);
        }
        if flag(args, "--diag") {
            attach_console();
            log::set_echo(true);
            let _com = dualmix_audio::com::ComGuard::sta();
            let rep = crate::diagnostics::run(flag(args, "--quick"), flag(args, "--stress"));
            println!("{}", rep.text);
            return if rep.ok { 0 } else { 1 };
        }
        if let Some(dir) = value(args, "--snapshot") {
            let _com = dualmix_audio::com::ComGuard::sta();
            return run_snapshot(PathBuf::from(dir));
        }
        if flag(args, "--supervise") {
            return run_supervisor(args);
        }
        let tray = flag(args, "--tray");
        // A double click / the Start menu hand over to the windowless, supervised way of running.
        if !flag(args, "--child") && !flag(args, "--native") {
            if dualmix_ui::is_app_running() {
                if !tray {
                    dualmix_ui::signal_show_existing();
                }
                return 0;
            }
            if let Ok(exe) = std::env::current_exe() {
                let mut a = vec!["--supervise".to_string()];
                if tray {
                    a.push("--tray".into());
                }
                if let Some(l) = &lang_override {
                    a.push("--lang".into());
                    a.push(l.clone());
                }
                if std::process::Command::new(exe).args(&a).spawn().is_ok() {
                    return 0;
                }
            }
        }
        let _com = dualmix_audio::com::ComGuard::sta();
        run_app(args, settings)
    }

    // ================================================================================== supervisor

    /// Keeps DualMix running like a service manager: restarts the child after an abnormal end (max 5 in 10 min).
    fn run_supervisor(args: &[String]) -> i32 {
        let tray = flag(args, "--tray");
        let name = HSTRING::from(SUPERVISOR_MUTEX);
        let mutex = unsafe { CreateMutexW(None, true, &name) };
        let first = unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
        if !first || dualmix_ui::is_app_running() {
            if !tray {
                dualmix_ui::signal_show_existing();
            }
            return 0;
        }
        let Ok(exe) = std::env::current_exe() else { return 1 };
        let lang = value(args, "--lang");
        let mut restarts: VecDeque<Instant> = VecDeque::new();
        let mut first_start = true;
        let code = loop {
            let mut a = vec!["--child".to_string()];
            if tray || !first_start {
                a.push("--tray".into());
            }
            if let Some(l) = &lang {
                a.push("--lang".into());
                a.push(l.clone());
            }
            let status = std::process::Command::new(&exe).args(&a).status();
            first_start = false;
            let code = match status {
                Ok(s) => s.code().unwrap_or(1),
                Err(e) => {
                    log::error(format!("Supervisor: cannot start DualMix | {e}"));
                    break 1;
                }
            };
            if code == 0 {
                break 0; // closed on purpose
            }
            let now = Instant::now();
            restarts.push_back(now);
            while restarts.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(600)) {
                restarts.pop_front();
            }
            if restarts.len() > 5 {
                log::error(format!("Supervisor: DualMix ended abnormally {} times in 10 minutes (last code 0x{code:08X}): giving up", restarts.len()));
                break code;
            }
            log::warn(format!("Supervisor: DualMix ended abnormally (code 0x{code:08X}): restarting ({}/5)", restarts.len()));
            std::thread::sleep(Duration::from_secs(2));
        };
        if let Ok(m) = mutex {
            unsafe {
                let _ = CloseHandle(m);
            }
        }
        code
    }

    // ================================================================================== host

    struct Host {
        ctrl: Controller,
        cache: UiCache,
        hwnd: HWND,
        osd: Option<OsdWindow>,
        main: Option<WebWindow>,
        wiz: Option<WebWindow>,
        main_visible: bool,
        live_on: bool,
        last_main_state: String,
        last_wiz_state: String,
        tooltip: String,
        exiting: bool,
        dark: bool,
        update_next: Instant,
        update_startup: bool,
        announced: Option<String>,
        auto_install_on_check: bool,
    }

    /// Operations that run a modal loop: executed with the host borrow released.
    enum Modal {
        PickPack(HWND),
        PickApp(HWND),
    }

    thread_local! {
        static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
        static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
        static WEB_Q: RefCell<VecDeque<WebEvent>> = const { RefCell::new(VecDeque::new()) };
        static HOST_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
        static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn web_sink() -> webwin::Sink {
        Rc::new(|ev| {
            WEB_Q.with(|q| q.borrow_mut().push_back(ev));
            let h = HOST_HWND.with(|h| h.get());
            if h != 0 {
                unsafe {
                    let _ = PostMessageW(HWND(h as *mut _), WM_APP_WEB, WPARAM(0), LPARAM(0));
                }
            }
        })
    }

    /// Runs `f` with the host borrowed. If it is already borrowed (should not happen), retries a moment later.
    fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> Option<R> {
        HOST.with(|h| match h.try_borrow_mut() {
            Ok(mut g) => g.as_mut().map(f),
            Err(_) => {
                let hw = HOST_HWND.with(|h| h.get());
                unsafe {
                    SetTimer(HWND(hw as *mut _), TIMER_RETRY, 30, None);
                }
                None
            }
        })
    }

    fn run_modals(mods: Vec<Modal>) {
        for m in mods {
            match m {
                Modal::PickPack(owner) => {
                    let p = dualmix_ui::dialogs::open_file_dialog(owner, &loc::get("Audio.DriverDialog.Title"), &loc::get("Audio.DriverDialog.Filter"));
                    if let Some(p) = p {
                        with_host(|h| h.ctrl.run_setup(false, false, Some(p)));
                    }
                }
                Modal::PickApp(owner) => {
                    let p = dualmix_ui::dialogs::open_file_dialog(owner, &loc::get("Apps.PickTitle"), &loc::get("Apps.PickFilter"));
                    if let Some(p) = p {
                        with_host(|h| h.ctrl.add_app_from_path(&p));
                    }
                }
            }
        }
        // anything produced by the modal results
        pump();
    }

    /// The single place where queued work is processed.
    fn pump() {
        let modals = with_host(|h| h.pump()).unwrap_or_default();
        if !modals.is_empty() {
            run_modals(modals);
        }
    }

    impl Host {
        fn pump(&mut self) -> Vec<Modal> {
            let mut modals = Vec::new();
            if self.exiting {
                return modals;
            }
            self.ctrl.handle_inbox();
            let events: Vec<WebEvent> = WEB_Q.with(|q| q.borrow_mut().drain(..).collect());
            for ev in events {
                self.on_web_event(ev, &mut modals);
            }
            let actions: Vec<HostAction> = std::mem::take(&mut self.ctrl.actions);
            for a in actions {
                self.on_action(a, &mut modals);
                if self.exiting {
                    return Vec::new();
                }
            }
            self.push_state(false);
            modals
        }

        fn on_web_event(&mut self, ev: WebEvent, modals: &mut Vec<Modal>) {
            match ev {
                WebEvent::Ready(_) => self.push_state(true),
                WebEvent::Message(kind, text) => {
                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                        log::warn(format!("UI: bad message {text}"));
                        return;
                    };
                    match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
                        "hideWindow" => self.hide_main(),
                        "log" => log::info(format!("UI: {}", v.get("text").and_then(Value::as_str).unwrap_or(""))),
                        _ => {
                            ui::command(&mut self.ctrl, &v);
                            let _ = kind;
                        }
                    }
                }
                WebEvent::CloseRequested(WinKind::Main) => self.hide_main(),
                WebEvent::CloseRequested(WinKind::Wizard) => {
                    // Closed without finishing: do not ask again at every start.
                    if !self.ctrl.settings.first_run_done {
                        self.ctrl.settings.first_run_done = true;
                        self.ctrl.mark_auto_setup_handled();
                        self.ctrl.save_now();
                    }
                    self.close_wizard();
                    self.open_main(true);
                }
                WebEvent::Visibility(WinKind::Main, v) => {
                    self.main_visible = v;
                    self.ctrl.set_meters_enabled(v);
                    self.sync_live_timer();
                    if v {
                        self.ctrl.rescan_now();
                        self.push_state(true);
                    }
                }
                WebEvent::Visibility(WinKind::Wizard, _) => {}
                WebEvent::Failed(kind, msg) => {
                    log::error(format!("UI window {kind:?} failed: {msg}"));
                    tray_balloon("DualMix", &msg);
                }
            }
            let _ = modals;
        }

        fn on_action(&mut self, a: HostAction, modals: &mut Vec<Modal>) {
            match a {
                HostAction::Osd(info, _force) => {
                    if let Some(o) = &self.osd {
                        o.show(&info, self.dark);
                    }
                }
                HostAction::Balloon(t, b) => tray_balloon(&t, &b),
                HostAction::ShowMain => self.open_main(true),
                HostAction::OpenWizard => self.open_wizard(),
                HostAction::CloseWizard => self.close_wizard(),
                HostAction::PickPack => modals.push(Modal::PickPack(self.owner())),
                HostAction::PickApp => modals.push(Modal::PickApp(self.owner())),
                HostAction::LanguageChanged => {
                    if let Some(w) = &self.wiz {
                        w.set_title(&loc::get("Wizard.WindowTitle"));
                    }
                    self.ctrl.evaluate(); // status texts are built in the current language
                    self.cache.invalidate();
                    self.push_state(true);
                }
                HostAction::ThemeChanged => self.apply_theme(),
                HostAction::Exit => self.exit(),
            }
        }

        fn owner(&self) -> HWND {
            self.main.as_ref().filter(|w| w.is_visible()).map(|w| w.hwnd()).or_else(|| self.wiz.as_ref().map(|w| w.hwnd())).unwrap_or(self.hwnd)
        }

        fn apply_theme(&mut self) {
            self.dark = dualmix_ui::dialogs::is_dark_theme(self.ctrl.settings.theme);
            for w in [&self.main, &self.wiz].into_iter().flatten() {
                w.set_dark(self.dark);
            }
            self.push_state(true);
        }

        fn push_state(&mut self, force: bool) {
            let dirty = std::mem::take(&mut self.ctrl.state_dirty) | std::mem::take(&mut self.ctrl.apps_dirty);
            if !dirty && !force {
                return;
            }
            if let Some(w) = &self.main {
                if w.is_ready() && (self.main_visible || force) {
                    let s = ui::state_json(&self.ctrl, &mut self.cache, false);
                    if force || s != self.last_main_state {
                        w.post(&s);
                        self.last_main_state = s;
                    }
                }
            }
            if let Some(w) = &self.wiz {
                if w.is_ready() {
                    let s = ui::state_json(&self.ctrl, &mut self.cache, true);
                    if force || s != self.last_wiz_state {
                        w.post(&s);
                        self.last_wiz_state = s;
                    }
                }
            }
            let tip = self.ctrl.tray_tooltip();
            if tip != self.tooltip {
                TRAY.with(|t| {
                    if let Ok(t) = t.try_borrow() {
                        if let Some(t) = t.as_ref() {
                            t.update_tooltip(&tip);
                        }
                    }
                });
                self.tooltip = tip;
            }
        }

        fn sync_live_timer(&mut self) {
            let want = self.main_visible && !self.exiting;
            if want != self.live_on {
                self.live_on = want;
                unsafe {
                    if want {
                        SetTimer(self.hwnd, TIMER_LIVE, 66, None);
                    } else {
                        let _ = KillTimer(self.hwnd, TIMER_LIVE);
                    }
                }
            }
        }

        fn live(&mut self) {
            if let Some(w) = &self.main {
                if self.main_visible && w.is_ready() {
                    w.post(&ui::live_json(&self.ctrl));
                }
            }
        }

        fn open_main(&mut self, activate: bool) {
            if self.exiting {
                return;
            }
            if self.main.is_none() {
                match WebWindow::create(WinKind::Main, "DualMix", self.dark, web_sink()) {
                    Ok(w) => self.main = Some(w),
                    Err(e) => {
                        log::error(format!("Main window: {e}"));
                        return;
                    }
                }
            }
            if let Some(w) = &self.main {
                w.show(activate);
            }
            self.main_visible = true;
            self.ctrl.set_meters_enabled(true);
            self.sync_live_timer();
            self.push_state(true);
        }

        fn hide_main(&mut self) {
            if let Some(w) = &self.main {
                w.hide();
            }
            self.main_visible = false;
            self.ctrl.set_meters_enabled(false);
            self.sync_live_timer();
            // C# App.OnMainWindowHidden: tell once that DualMix keeps running in the tray.
            if !self.ctrl.settings.tray_hint_shown {
                self.ctrl.settings.tray_hint_shown = true;
                self.ctrl.schedule_save();
                tray_balloon(&loc::get("Tray.StillRunning.Title"), &loc::get("Tray.StillRunning.Body"));
            }
        }

        fn open_wizard(&mut self) {
            if self.exiting {
                return;
            }
            if self.wiz.is_none() {
                match WebWindow::create(WinKind::Wizard, &loc::get("Wizard.WindowTitle"), self.dark, web_sink()) {
                    Ok(w) => self.wiz = Some(w),
                    Err(e) => {
                        log::error(format!("Setup wizard: {e}"));
                        return;
                    }
                }
            }
            if let Some(w) = &self.wiz {
                w.show(true);
            }
            self.last_wiz_state.clear();
            self.push_state(true);
        }

        fn close_wizard(&mut self) {
            if let Some(w) = self.wiz.take() {
                w.destroy();
            }
            self.last_wiz_state.clear();
        }

        /// Port of C# `App.AutoUpdateAsync`, called from the 250 ms tick.
        fn auto_update(&mut self) {
            if self.ctrl.updater.take_checked() {
                self.ctrl.settings.update_next_check = Some(dualmix_core::log::iso_from_unix(dualmix_core::log::unix_now() + 86_400));
                self.ctrl.schedule_save();
                let snap = self.ctrl.updater.snapshot();
                if let Some(info) = snap.available {
                    if self.auto_install_on_check {
                        tray_balloon(&loc::get("Updater.BalloonTitle"), &loc::fmt("Updater.BalloonInstalling", &[&info.version]));
                        self.ctrl.updater.install(self.ctrl.inbox());
                    } else if self.announced.as_deref() != Some(&info.version) {
                        self.announced = Some(info.version.clone());
                        let body = if self.ctrl.settings.update_mode == UpdateMode::Auto { loc::get("Updater.BalloonAutoNext") } else { loc::get("Updater.BalloonManual") };
                        tray_balloon(&loc::fmt("Updater.BalloonAvailableTitle", &[&info.version]), &body);
                    }
                }
                self.auto_install_on_check = false;
            }
            if Instant::now() < self.update_next || self.exiting {
                return;
            }
            let startup = self.update_startup;
            self.update_startup = false;
            self.update_next = Instant::now() + Duration::from_secs(3600);
            let s = &self.ctrl.settings;
            if s.update_mode == UpdateMode::Off || self.ctrl.updater.busy() || self.ctrl.dry_run {
                return;
            }
            let due = dualmix_core::log::parse_iso(s.update_next_check.as_deref().unwrap_or("")).is_none_or(|t| dualmix_core::log::unix_now() >= t);
            if !due && !(startup && s.update_mode == UpdateMode::Auto) {
                return;
            }
            // At start-up an update is installed right away when updates are automatic; later it is only announced.
            self.auto_install_on_check = startup && s.update_mode == UpdateMode::Auto;
            self.ctrl.updater.check(self.ctrl.inbox());
        }

        fn tick(&mut self) {
            if self.exiting {
                return;
            }
            self.ctrl.tick();
            self.auto_update();
            if self.main_visible {
                self.ctrl.apps_dirty = true; // app list/status refresh (diffed before sending)
            }
        }

        fn tray_status(&self) -> TrayStatus {
            TrayStatus {
                tooltip: self.tooltip.clone(),
                mix_label: format!("Game {} · Chat {}", self.ctrl.game_level_text(), self.ctrl.chat_level_text()),
                target: self.ctrl.settings.knob_target,
                autostart: self.ctrl.autostart_enabled,
                dark: self.dark,
            }
        }

        fn on_tray(&mut self, act: TrayAction) {
            match act {
                TrayAction::Open => self.open_main(true),
                TrayAction::AllGame => self.ctrl.set_mix(-1.0, false),
                TrayAction::Balanced => self.ctrl.reset_mix(false),
                TrayAction::AllChat => self.ctrl.set_mix(1.0, false),
                TrayAction::SetTarget(t) => self.ctrl.set_knob_target(t, true),
                TrayAction::ToggleAutostart => {
                    let on = !self.ctrl.autostart_enabled;
                    self.ctrl.settings.start_with_windows = on;
                    crate::system_integration::autostart_set(on);
                    self.ctrl.autostart_enabled = on;
                    self.ctrl.schedule_save();
                    self.ctrl.state_dirty = true;
                }
                TrayAction::Exit => self.exit(),
            }
        }

        fn exit(&mut self) {
            if self.exiting {
                return;
            }
            self.exiting = true;
            log::info("Exit requested");
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_TICK);
                let _ = KillTimer(self.hwnd, TIMER_LIVE);
            }
            if let Some(w) = self.main.take() {
                w.destroy();
            }
            if let Some(w) = self.wiz.take() {
                w.destroy();
            }
            if let Some(o) = &self.osd {
                o.hide_now();
            }
            self.ctrl.shutdown();
            unsafe { PostQuitMessage(0) };
        }
    }

    fn tray_balloon(title: &str, body: &str) {
        TRAY.with(|t| {
            if let Ok(t) = t.try_borrow() {
                if let Some(t) = t.as_ref() {
                    t.show_balloon(title, body);
                }
            }
        });
    }

    unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_APP_WAKE | WM_APP_WEB => {
                    pump();
                    LRESULT(0)
                }
                WM_TIMER => {
                    match wparam.0 {
                        TIMER_TICK => {
                            with_host(|h| h.tick());
                            pump();
                        }
                        TIMER_LIVE => {
                            with_host(|h| h.live());
                        }
                        TIMER_RETRY => {
                            let _ = KillTimer(hwnd, TIMER_RETRY);
                            pump();
                        }
                        _ => {}
                    }
                    LRESULT(0)
                }
                WM_APP_SHOW => {
                    with_host(|h| h.open_main(true));
                    LRESULT(0)
                }
                WM_APP_EXIT => {
                    with_host(|h| h.exit());
                    LRESULT(0)
                }
                WM_TRAY_ICON => {
                    let ev = (lparam.0 & 0xFFFF) as u32;
                    if ev == 0x0202 {
                        // WM_LBUTTONUP
                        with_host(|h| h.open_main(true));
                        pump();
                    } else if ev == 0x0205 || ev == 0x007B {
                        // right click: the menu runs a modal loop, so the host is not borrowed meanwhile
                        if let Some(status) = with_host(|h| h.tray_status()) {
                            let act = TRAY.with(|t| t.try_borrow().ok().and_then(|t| t.as_ref().and_then(|t| t.handle_message(lparam, &status))));
                            if let Some(a) = act {
                                with_host(|h| h.on_tray(a));
                                pump();
                            }
                        }
                    }
                    LRESULT(0)
                }
                WM_QUERYENDSESSION => LRESULT(1),
                WM_ENDSESSION => {
                    if wparam.0 != 0 {
                        log::info("Windows session ending");
                        with_host(|h| h.exit());
                    }
                    LRESULT(0)
                }
                WM_SETTINGCHANGE => {
                    // "ImmersiveColorSet": the Windows light/dark theme changed
                    if lparam.0 != 0 {
                        let s = PCWSTR(lparam.0 as *const u16).to_string().unwrap_or_default();
                        if s == "ImmersiveColorSet" {
                            with_host(|h| h.apply_theme());
                        }
                    }
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
                _ => {
                    let tc = TASKBAR_CREATED.with(|c| c.get());
                    if tc != 0 && msg == tc {
                        let tip = with_host(|h| h.tooltip.clone()).unwrap_or_else(|| "DualMix".into());
                        TRAY.with(|t| {
                            if let Ok(mut t) = t.try_borrow_mut() {
                                if let Some(t) = t.as_mut() {
                                    t.readd(&tip);
                                }
                            }
                        });
                        return LRESULT(0);
                    }
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }
    }

    fn create_host_window() -> HWND {
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(host_proc),
                hInstance: hinst,
                lpszClassName: w!("DualMixHostWindow"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            // A real (hidden) top-level window, not message-only: it must receive broadcasts such as
            // TaskbarCreated (tray icon after an Explorer restart), WM_SETTINGCHANGE and WM_ENDSESSION.
            CreateWindowExW(WS_EX_TOOLWINDOW, w!("DualMixHostWindow"), w!("DualMix"), WS_POPUP, 0, 0, 0, 0, None, None, hinst, None).unwrap_or_default()
        }
    }

    fn run_app(args: &[String], settings: dualmix_core::settings::Settings) -> i32 {
        let name = HSTRING::from(dualmix_ui::INSTANCE_MUTEX);
        let mutex = unsafe { CreateMutexW(None, true, &name) };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS || mutex.is_err() {
            if !flag(args, "--tray") {
                dualmix_ui::signal_show_existing();
            }
            return 0;
        }
        log::info(format!("DualMix {} starting ({})", env!("CARGO_PKG_VERSION"), args.join(" ")));
        unsafe {
            let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(w!("Marco.DualMix"));
        }
        dualmix_audio::process_tuning::apply_process_tuning();
        let tray_start = flag(args, "--tray");
        let force_wizard = flag(args, "--wizard");

        // Keep the autostart task pointing at the current installation (e.g. after an update/move).
        let autostart = if settings.start_with_windows {
            if !crate::system_integration::autostart_is_current() {
                crate::system_integration::autostart_set(true);
            }
            true
        } else {
            if crate::system_integration::autostart_is_enabled() {
                crate::system_integration::autostart_set(false);
            }
            false
        };

        let hwnd = create_host_window();
        HOST_HWND.with(|h| h.set(hwnd.0 as isize));
        TASKBAR_CREATED.with(|c| c.set(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }));
        let inbox = Inbox::new();
        inbox.set_hwnd(hwnd.0 as isize);
        let first_run = !settings.first_run_done;
        let start_minimized = settings.start_minimized;
        let dark = dualmix_ui::dialogs::is_dark_theme(settings.theme);
        let mut ctrl = Controller::new(settings, false, Arc::clone(&inbox));
        ctrl.autostart_enabled = autostart;
        if flag(args, "--accept-injected") {
            ctrl.set_accept_injected(true);
        }
        ctrl.start();
        TRAY.with(|t| *t.borrow_mut() = Some(TrayIcon::new(hwnd)));
        let host = Host {
            tooltip: String::new(),
            ctrl,
            cache: UiCache::default(),
            hwnd,
            osd: OsdWindow::create(),
            main: None,
            wiz: None,
            main_visible: false,
            live_on: false,
            last_main_state: String::new(),
            last_wiz_state: String::new(),
            exiting: false,
            dark,
            update_next: Instant::now() + Duration::from_secs(60),
            update_startup: true,
            announced: None,
            auto_install_on_check: false,
        };
        HOST.with(|h| *h.borrow_mut() = Some(host));

        // Show / Exit requests from other processes (auto-reset events, like the C# EventWaitHandles).
        let show_ev = unsafe { CreateEventW(None, false, false, &HSTRING::from(dualmix_ui::SHOW_EVENT)).unwrap_or_default() };
        let exit_ev = unsafe { CreateEventW(None, false, false, &HSTRING::from(dualmix_ui::EXIT_EVENT)).unwrap_or_default() };
        let (sh, eh, hw) = (show_ev.0 as isize, exit_ev.0 as isize, hwnd.0 as isize);
        std::thread::Builder::new()
            .name("DualMix.Ipc".into())
            .spawn(move || unsafe {
                let handles = [HANDLE(sh as *mut _), HANDLE(eh as *mut _)];
                loop {
                    let r = WaitForMultipleObjects(&handles, false, u32::MAX);
                    let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0);
                    let msg = match idx {
                        0 => WM_APP_SHOW,
                        1 => WM_APP_EXIT,
                        _ => break,
                    };
                    let _ = PostMessageW(HWND(hw as *mut _), msg, WPARAM(0), LPARAM(0));
                    if idx == 1 {
                        break;
                    }
                }
            })
            .ok();

        unsafe {
            SetTimer(hwnd, TIMER_TICK, 250, None);
        }
        with_host(|h| {
            if first_run || force_wizard {
                h.open_wizard();
            } else if !tray_start || !start_minimized {
                h.open_main(true);
            }
        });
        pump();

        unsafe {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        HOST.with(|h| h.borrow_mut().take());
        TRAY.with(|t| t.borrow_mut().take());
        unsafe {
            let _ = CloseHandle(show_ev);
            let _ = CloseHandle(exit_ev);
            if let Ok(m) = mutex {
                let _ = CloseHandle(m);
            }
        }
        log::info("DualMix exited");
        0
    }

    // ================================================================================== snapshot

    /// Developer aid: renders the wizard steps and every page to PNG files, without touching the audio, and exits.
    fn run_snapshot(dir: PathBuf) -> i32 {
        SettingsStore::set_read_only(true); // screenshots leave no trace
        let _ = std::fs::create_dir_all(&dir);
        let settings = SettingsStore::load();
        let hwnd = create_host_window();
        HOST_HWND.with(|h| h.set(hwnd.0 as isize));
        let inbox = Inbox::new();
        inbox.set_hwnd(hwnd.0 as isize);
        let dark = dualmix_ui::dialogs::is_dark_theme(settings.theme);
        let mut ctrl = Controller::new(settings, true, inbox);
        ctrl.start();
        let mut cache = UiCache::default();
        let wait_ready = |w: &WebWindow| {
            let start = Instant::now();
            while !w.is_ready() && start.elapsed() < Duration::from_secs(20) {
                webwin::pump_for(Duration::from_millis(50));
                WEB_Q.with(|q| q.borrow_mut().clear());
            }
            w.is_ready()
        };
        let mut ok = true;
        let shoot = |w: &WebWindow, name: &str, ok: &mut bool| {
            webwin::pump_for(Duration::from_millis(500));
            if let Err(e) = w.capture_png(&dir.join(name)) {
                log::error(format!("snapshot {name}: {e}"));
                *ok = false;
            }
        };
        match WebWindow::create(WinKind::Wizard, &loc::get("Wizard.WindowTitle"), dark, web_sink()) {
            Ok(w) => {
                w.show(false);
                if wait_ready(&w) {
                    w.post(&ui::state_json(&ctrl, &mut cache, true));
                    for step in 1..=3 {
                        w.post(&format!("{{\"t\":\"wizardStep\",\"step\":{step}}}"));
                        shoot(&w, &format!("wizard_step{step}.png"), &mut ok);
                    }
                } else {
                    ok = false;
                }
                w.destroy();
            }
            Err(e) => {
                log::error(format!("snapshot wizard: {e}"));
                ok = false;
            }
        }
        match WebWindow::create(WinKind::Main, "DualMix", dark, web_sink()) {
            Ok(w) => {
                w.show(false);
                ctrl.set_meters_enabled(true);
                if wait_ready(&w) {
                    webwin::pump_for(Duration::from_millis(1500));
                    ctrl.tick();
                    w.post(&ui::state_json(&ctrl, &mut cache, false));
                    for (i, page) in ["mixer", "apps", "knob", "audio", "general"].iter().enumerate() {
                        w.post(&format!("{{\"t\":\"nav\",\"page\":\"{page}\"}}"));
                        shoot(&w, &format!("page{i}.png"), &mut ok);
                        w.exec("window.scrollToEnd && window.scrollToEnd()");
                        shoot(&w, &format!("page{i}_end.png"), &mut ok);
                    }
                } else {
                    ok = false;
                }
                w.destroy();
            }
            Err(e) => {
                log::error(format!("snapshot main: {e}"));
                ok = false;
            }
        }
        let _ = KnobTarget::Mix;
        ctrl.shutdown();
        if ok { 0 } else { 1 }
    }
}
