//! DualMix native Windows entry point (`DualMix.exe`).
//!
//! Supports CLI tools (`--version`, `--diag`, `--exit`, `--setup`, `--snapshot`) and
//! the full Win32 + WebView2 + Tray + OSD + WASAPI background application (`--supervise`, `--tray`, `--wizard`).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod controller;
#[cfg(windows)]
mod diagnostics;
#[cfg(windows)]
mod system_integration;
#[cfg(windows)]
mod updater;

#[cfg(not(windows))]
fn main() {
    eprintln!("DualMix runs on Windows only.");
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = win_main(&args);
    std::process::exit(code);
}

#[cfg(windows)]
fn attach_parent_console_if_needed() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(windows)]
fn cli_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case(flag))
}

#[cfg(windows)]
fn cli_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    for (i, a) in args.iter().enumerate() {
        if a.eq_ignore_ascii_case(flag) {
            return args.get(i + 1).cloned();
        }
        if a.len() > prefix.len() && a[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            return Some(a[prefix.len()..].to_string());
        }
    }
    None
}

#[cfg(windows)]
fn parse_lang_cli(s: Option<&str>) -> Option<dualmix_core::settings::LanguagePreference> {
    use dualmix_core::settings::LanguagePreference;
    let v = s?.trim().to_ascii_lowercase();
    if v.starts_with("it") {
        Some(LanguagePreference::Italian)
    } else if v.starts_with("en") {
        Some(LanguagePreference::English)
    } else {
        None
    }
}

#[cfg(windows)]
fn win_main(args: &[String]) -> i32 {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use dualmix_core::log;
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _com = dualmix_audio::com::ComGuard::sta();

    if cli_flag(args, "--version") || cli_flag(args, "-v") {
        attach_parent_console_if_needed();
        println!("DualMix {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }

    if cli_flag(args, "--help") || cli_flag(args, "-h") {
        attach_parent_console_if_needed();
        println!("DualMix {}", env!("CARGO_PKG_VERSION"));
        println!("Options: --tray  --wizard  --diag [--quick|--stress]  --snapshot <dir>  --lang en|it  --accept-injected  --supervise  --exit  --version");
        return 0;
    }

    if cli_flag(args, "--exit") {
        attach_parent_console_if_needed();
        if dualmix_ui::signal_exit_existing() {
            let start = Instant::now();
            while dualmix_ui::is_app_running() && start.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("DualMix stopped.");
        }
        return 0;
    }

    if cli_flag(args, "--setup") {
        let s = dualmix_core::settings::SettingsStore::load();
        dualmix_core::loc::initialize(s.language, None);
        return dualmix_audio::driver_setup::run_elevated(args);
    }

    if cli_flag(args, "--diag") {
        attach_parent_console_if_needed();
        log::set_echo(true);
        let lang = cli_value(args, "--lang");
        let s = dualmix_core::settings::SettingsStore::load();
        dualmix_core::loc::initialize(s.language, lang.as_deref());
        let rep = diagnostics::run(cli_flag(args, "--quick"), cli_flag(args, "--stress"));
        println!("{}", rep.text);
        println!("Report saved: {}", rep.path.display());
        return if rep.ok { 0 } else { 1 };
    }

    if let Some(snap_dir) = cli_value(args, "--snapshot") {
        attach_parent_console_if_needed();
        let lang_str = cli_value(args, "--lang").unwrap_or_else(|| "en".to_string());
        let lang = parse_lang_cli(Some(&lang_str));
        return run_snapshot(PathBuf::from(snap_dir), lang);
    }

    if cli_flag(args, "--supervise") {
        return run_supervisor(args);
    }

    run_interactive_app(args)
}

#[cfg(windows)]
fn run_supervisor(args: &[String]) -> i32 {
    use std::process::Command;
    use std::time::{Duration, Instant};
    use dualmix_core::log;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
    use windows::core::HSTRING;

    let Ok(exe) = std::env::current_exe() else { return 1 };
    let child_args: Vec<String> = args
        .iter()
        .filter(|a| !a.eq_ignore_ascii_case("--supervise"))
        .cloned()
        .chain(std::iter::once("--child".to_string()))
        .collect();

    let exit_ev_name = HSTRING::from(dualmix_ui::EXIT_EVENT);
    let exit_ev = unsafe { CreateEventW(None, true, false, &exit_ev_name).ok() };

    let mut quick_crashes = 0u32;
    loop {
        if let Some(ev) = exit_ev {
            unsafe {
                let _ = ResetEvent(ev);
            }
        }
        let started = Instant::now();
        let Ok(mut child) = Command::new(&exe).args(&child_args).spawn() else {
            return 1;
        };
        let code = loop {
            if let Ok(Some(st)) = child.try_wait() {
                break st.code().unwrap_or(1);
            }
            if let Some(ev) = exit_ev {
                if unsafe { WaitForSingleObject(ev, 0) } == WAIT_OBJECT_0 {
                    // Graceful exit requested; give child up to 5s to shut down cleanly
                    for _ in 0..100 {
                        if let Ok(Some(st)) = child.try_wait() {
                            unsafe { let _ = CloseHandle(ev); }
                            return st.code().unwrap_or(0);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        if code == 0 {
            if let Some(ev) = exit_ev {
                unsafe { let _ = CloseHandle(ev); }
            }
            return 0;
        }
        if let Some(ev) = exit_ev {
            if unsafe { WaitForSingleObject(ev, 0) } == WAIT_OBJECT_0 {
                unsafe { let _ = CloseHandle(ev); }
                return 0;
            }
        }
        if started.elapsed() < Duration::from_secs(10) {
            quick_crashes += 1;
            if quick_crashes >= 4 {
                log::error("Supervisor stopping after repeated immediate crashes");
                if let Some(ev) = exit_ev {
                    unsafe { let _ = CloseHandle(ev); }
                }
                return code;
            }
        } else {
            quick_crashes = 0;
        }
        log::warn(format!("Child exited unexpectedly with code {code}; restarting in 1s"));
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(windows)]
fn run_snapshot(out_dir: std::path::PathBuf, lang: Option<dualmix_core::settings::LanguagePreference>) -> i32 {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;
    use dualmix_ui::state::UiCommand;
    use dualmix_ui::webview::{FluentWebWindow, WindowKind};
    use crate::controller::{Controller, WakeHandle};

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("Failed to create snapshot dir {}: {e}", out_dir.display());
        return 1;
    }

    let wake = WakeHandle::new();
    let ctrl = Rc::new(RefCell::new(Controller::new(true, false, lang, wake)));

    // 1. Capture SetupWizard steps 1..3
    {
        ctrl.borrow_mut().set_wizard_mode(true);
        let c_cmd = ctrl.clone();
        let wiz = match FluentWebWindow::new(
            WindowKind::Wizard,
            "DualMix Setup",
            ctrl.borrow().is_dark(),
            Box::new(move |cmd| {
                c_cmd.borrow_mut().handle_ui_command(cmd);
            }),
            Box::new(|_| {}),
            Box::new(|| {}),
        ) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("Wizard WebView2 init failed: {e}");
                return 1;
            }
        };
        wiz.show_unfocused();
        if !wiz.wait_ready(Duration::from_secs(12)) {
            eprintln!("Wizard WebView2 timed out waiting for Ready");
            return 1;
        }
        for step in 1u8..=3u8 {
            ctrl.borrow_mut().handle_ui_command(UiCommand::SetWizardStep { step });
            let st = ctrl.borrow().build_ui_state();
            wiz.push_state(&st);
            wiz.pump_ms(220);
            let p = out_dir.join(format!("wizard_step{step}.png"));
            if let Err(e) = wiz.capture_png(&p) {
                eprintln!("Snapshot {} failed: {e}", p.display());
                return 1;
            }
        }
        wiz.really_close();
        wiz.pump_ms(80);
    }

    // 2. Capture MainWindow tabs 0..4 (+ page4_end)
    {
        ctrl.borrow_mut().set_wizard_mode(false);
        let c_cmd = ctrl.clone();
        let main = match FluentWebWindow::new(
            WindowKind::Main,
            "DualMix",
            ctrl.borrow().is_dark(),
            Box::new(move |cmd| {
                c_cmd.borrow_mut().handle_ui_command(cmd);
            }),
            Box::new(|_| {}),
            Box::new(|| {}),
        ) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("Main WebView2 init failed: {e}");
                return 1;
            }
        };
        main.set_hide_on_close(false);
        main.show_unfocused();
        if !main.wait_ready(Duration::from_secs(12)) {
            eprintln!("Main WebView2 timed out waiting for Ready");
            return 1;
        }
        for tab in 0u8..=4u8 {
            ctrl.borrow_mut().handle_ui_command(UiCommand::SetTab { tab });
            let mut st = ctrl.borrow().build_ui_state();
            st.active_tab = tab;
            main.push_state(&st);
            main.pump_ms(240);
            let p = out_dir.join(format!("page{tab}.png"));
            if let Err(e) = main.capture_png(&p) {
                eprintln!("Snapshot {} failed: {e}", p.display());
                return 1;
            }
        }
        main.scroll_to_end();
        main.pump_ms(180);
        let end_p = out_dir.join("page4_end.png");
        if let Err(e) = main.capture_png(&end_p) {
            eprintln!("Snapshot {} failed: {e}", end_p.display());
            return 1;
        }
        main.really_close();
        main.pump_ms(80);
    }

    println!("Snapshots captured to {}", out_dir.display());
    0
}

#[cfg(windows)]
struct AppHostState {
    ctrl: std::rc::Rc<std::cell::RefCell<controller::Controller>>,
    wake: std::sync::Arc<controller::WakeHandle>,
    host_hwnd: windows::Win32::Foundation::HWND,
    tray: dualmix_ui::tray::TrayIcon,
    osd: Option<dualmix_ui::osd::OsdWindow>,
    main_win: std::cell::RefCell<Option<dualmix_ui::webview::FluentWebWindow>>,
    wiz_win: std::cell::RefCell<Option<dualmix_ui::webview::FluentWebWindow>>,
    show_ev: windows::Win32::Foundation::HANDLE,
    exit_ev: windows::Win32::Foundation::HANDLE,
    running: std::cell::Cell<bool>,
    fast_timer: std::cell::Cell<bool>,
    modal_open: std::cell::Cell<bool>,
    auto_update_checked: std::cell::Cell<bool>,
    auto_update_handled: std::cell::Cell<bool>,
    started_at: std::time::Instant,
}

#[cfg(windows)]
impl AppHostState {
    fn sync_timer_rate(&self) {
        let want_fast = self.ctrl.try_borrow().map(|c| c.meters_enabled()).unwrap_or(false);
        if want_fast != self.fast_timer.get() && !self.host_hwnd.0.is_null() {
            self.fast_timer.set(want_fast);
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                    self.host_hwnd,
                    1,
                    if want_fast { 66 } else { 250 },
                    None,
                );
            }
        }
    }

    fn open_main(&self) {
        if let Some(w) = self.main_win.borrow().clone() {
            let st = self.ctrl.borrow().build_ui_state();
            w.push_state(&st);
            w.show();
            self.ctrl.borrow_mut().set_meters_enabled(true);
            self.sync_timer_rate();
            return;
        }

        let ctrl = self.ctrl.clone();
        let ctrl_vis = self.ctrl.clone();
        let wake_cmd = self.wake.clone();
        let wake_vis = self.wake.clone();
        let dark = self.ctrl.borrow().is_dark();
        let title = format!("DualMix {}", env!("CARGO_PKG_VERSION"));
        let ctrl_ref = self.ctrl.clone();

        match dualmix_ui::webview::FluentWebWindow::new(
            dualmix_ui::webview::WindowKind::Main,
            &title,
            dark,
            Box::new(move |cmd| {
                if let Ok(mut c) = ctrl.try_borrow_mut() {
                    c.handle_ui_command(cmd);
                }
                wake_cmd.wake();
            }),
            Box::new(move |vis| {
                if let Ok(mut c) = ctrl_vis.try_borrow_mut() {
                    c.set_meters_enabled(vis);
                }
                if !vis {
                    if let Ok(mut c) = ctrl_ref.try_borrow_mut() {
                        if !c.settings.tray_hint_shown && !c.dry_run {
                            c.settings.tray_hint_shown = true;
                            c.save_now();
                            c.push_event_balloon(
                                dualmix_core::loc::get("Tray.StillRunning.Title"),
                                dualmix_core::loc::get("Tray.StillRunning.Body"),
                            );
                        }
                    }
                }
                wake_vis.wake();
            }),
            Box::new(|| {}),
        ) {
            Ok(w) => {
                *self.main_win.borrow_mut() = Some(w.clone());
                let st = self.ctrl.borrow().build_ui_state();
                w.push_state(&st);
                w.show();
                self.ctrl.borrow_mut().set_meters_enabled(true);
                self.sync_timer_rate();
            }
            Err(e) => dualmix_core::log::error(format!("MainWindow creation failed: {e}")),
        }
    }

    fn open_wizard(&self) {
        if let Some(w) = self.wiz_win.borrow().clone() {
            self.ctrl.borrow_mut().set_wizard_mode(true);
            let st = self.ctrl.borrow().build_ui_state();
            w.push_state(&st);
            w.show();
            return;
        }

        self.ctrl.borrow_mut().set_wizard_mode(true);
        let ctrl = self.ctrl.clone();
        let ctrl_close = self.ctrl.clone();
        let wake_cmd = self.wake.clone();
        let dark = self.ctrl.borrow().is_dark();
        let title = dualmix_core::loc::get("Wizard.WindowTitle");
        match dualmix_ui::webview::FluentWebWindow::new(
            dualmix_ui::webview::WindowKind::Wizard,
            &title,
            dark,
            Box::new(move |cmd| {
                if let Ok(mut c) = ctrl.try_borrow_mut() {
                    c.handle_ui_command(cmd);
                }
                wake_cmd.wake();
            }),
            Box::new(|_| {}),
            Box::new(move || {
                if let Ok(mut c) = ctrl_close.try_borrow_mut() {
                    c.set_wizard_mode(false);
                }
            }),
        ) {
            Ok(w) => {
                *self.wiz_win.borrow_mut() = Some(w.clone());
                let st = self.ctrl.borrow().build_ui_state();
                w.push_state(&st);
                w.show();
            }
            Err(e) => dualmix_core::log::error(format!("SetupWindow creation failed: {e}")),
        }
    }

    fn drain_controller_events(&self) {
        loop {
            let ev = self.ctrl.try_borrow_mut().ok().and_then(|mut c| c.pop_event());
            let Some(ev) = ev else { break };
            match ev {
                controller::ControllerEvent::Osd(info) => {
                    if let Some(ref osd) = self.osd {
                        let dark = self.ctrl.borrow().is_dark();
                        osd.show(&info, dark);
                    }
                }
                controller::ControllerEvent::StateChanged => {
                    let st = self.ctrl.borrow().build_ui_state();
                    if let Some(w) = self.main_win.borrow().clone() {
                        w.push_state(&st);
                    }
                    if let Some(w) = self.wiz_win.borrow().clone() {
                        let mut wiz_st = st.clone();
                        wiz_st.mode = "wizard".to_string();
                        w.push_state(&wiz_st);
                    }
                    let ts = self.ctrl.borrow().tray_status();
                    self.tray.update_tooltip(&ts.tooltip);
                }
                controller::ControllerEvent::LiveChanged => {
                    if let Some(w) = self.main_win.borrow().clone() {
                        if w.is_visible() {
                            let patch = self.ctrl.borrow().build_live_patch();
                            w.push_live(&patch);
                        }
                    }
                }
                controller::ControllerEvent::WizardClosed => {
                    let w = self.wiz_win.borrow_mut().take();
                    if let Some(w) = w {
                        w.really_close();
                    }
                    self.ctrl.borrow_mut().set_wizard_mode(false);
                }
                controller::ControllerEvent::ShowMain => {
                    self.open_main();
                }
                controller::ControllerEvent::HideMain => {
                    if let Some(w) = self.main_win.borrow().clone() {
                        w.hide();
                        self.ctrl.borrow_mut().set_meters_enabled(false);
                    } else if let Some(w) = self.wiz_win.borrow().clone() {
                        w.hide();
                    }
                }
                controller::ControllerEvent::MinimizeMain => {
                    if self.ctrl.borrow().is_wizard_mode() {
                        if let Some(w) = self.wiz_win.borrow().clone() {
                            w.minimize();
                        }
                    } else if let Some(w) = self.main_win.borrow().clone() {
                        w.minimize();
                    }
                }
                controller::ControllerEvent::ToggleMaxMain => {
                    if let Some(w) = self.main_win.borrow().clone() {
                        w.toggle_maximize();
                    }
                }
                controller::ControllerEvent::PickAddApp => {
                    if !self.modal_open.replace(true) {
                        let owner = self
                            .main_win
                            .borrow()
                            .as_ref()
                            .map(|w| w.hwnd())
                            .unwrap_or_default();
                        let title = dualmix_core::loc::get("Apps.PickTitle");
                        let filter = dualmix_core::loc::get("Apps.PickFilter");
                        let picked = dualmix_ui::dialogs::open_file_dialog(owner, &title, &filter);
                        self.modal_open.set(false);
                        if let Some(path) = picked {
                            self.ctrl.borrow_mut().apply_picked_app(path);
                        }
                    }
                }
                controller::ControllerEvent::OpenWizard => {
                    self.open_wizard();
                }
                controller::ControllerEvent::Balloon { title, body } => {
                    self.tray.show_balloon(&title, &body);
                }
            }
        }
        self.sync_timer_rate();
    }

    fn shutdown(&self, hwnd: windows::Win32::Foundation::HWND) {
        if !self.running.replace(false) {
            return;
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(hwnd, 1);
        }
        let m = self.main_win.borrow_mut().take();
        if let Some(w) = m {
            w.really_close();
        }
        let wz = self.wiz_win.borrow_mut().take();
        if let Some(w) = wz {
            w.really_close();
        }
        self.ctrl.borrow_mut().shutdown();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
        }
    }
}

#[cfg(windows)]
fn run_interactive_app(args: &[String]) -> i32 {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Instant;
    use dualmix_core::log;
    use dualmix_core::settings::UpdateMode;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, ResetEvent, WaitForSingleObject};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
        GWLP_USERDATA, HWND_MESSAGE, MSG, RegisterClassExW, SetTimer, SetWindowLongPtrW,
        TranslateMessage, WM_ENDSESSION, WM_NCCREATE, WM_QUERYENDSESSION, WM_TIMER, WNDCLASSEXW,
    };
    use windows::core::{HSTRING, w};

    let mutex_name = HSTRING::from(dualmix_ui::INSTANCE_MUTEX);
    let mutex = unsafe { CreateMutexW(None, true, &mutex_name) };
    let already = unsafe { GetLastError() == ERROR_ALREADY_EXISTS };
    if already || mutex.is_err() {
        let _ = dualmix_ui::signal_show_existing();
        if let Ok(m) = mutex {
            unsafe { let _ = CloseHandle(m); }
        }
        return 0;
    }
    let mutex = mutex.unwrap();

    let show_name = HSTRING::from(dualmix_ui::SHOW_EVENT);
    let exit_name = HSTRING::from(dualmix_ui::EXIT_EVENT);
    let show_ev = unsafe { CreateEventW(None, true, false, &show_name).unwrap_or_default() };
    let exit_ev = unsafe { CreateEventW(None, true, false, &exit_name).unwrap_or_default() };
    unsafe {
        if !show_ev.is_invalid() {
            let _ = ResetEvent(show_ev);
        }
        if !exit_ev.is_invalid() {
            let _ = ResetEvent(exit_ev);
        }
    }

    log::info(format!("DualMix {} starting", env!("CARGO_PKG_VERSION")));
    dualmix_audio::process_tuning::apply_process_tuning();

    let lang_cli = parse_lang_cli(cli_value(args, "--lang").as_deref());
    let accept_injected = cli_flag(args, "--accept-injected");
    let start_tray = cli_flag(args, "--tray");
    let force_wizard = cli_flag(args, "--wizard");

    let wake = controller::WakeHandle::new();
    let ctrl = Rc::new(RefCell::new(controller::Controller::new(false, accept_injected, lang_cli, wake.clone())));

    unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            if msg == WM_NCCREATE {
                let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
                if !cs.lpCreateParams.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                }
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AppHostState;
            if ptr.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let host = &*ptr;

            match msg {
                WM_TIMER => {
                    if !host.show_ev.is_invalid() && WaitForSingleObject(host.show_ev, 0) == windows::Win32::Foundation::WAIT_OBJECT_0 {
                        let _ = ResetEvent(host.show_ev);
                        host.open_main();
                    }
                    if !host.exit_ev.is_invalid() && WaitForSingleObject(host.exit_ev, 0) == windows::Win32::Foundation::WAIT_OBJECT_0 {
                        host.shutdown(hwnd);
                        return LRESULT(0);
                    }
                    if let Ok(mut c) = host.ctrl.try_borrow_mut() {
                        c.tick(true);
                    }
                    if !host.modal_open.get() {
                        host.drain_controller_events();
                    }

                    if !host.auto_update_checked.get() && host.started_at.elapsed().as_secs() >= 5 {
                        host.auto_update_checked.set(true);
                        let mode = host.ctrl.borrow().settings.update_mode;
                        if mode != UpdateMode::Off {
                            let upd = host.ctrl.borrow().updater.clone();
                            let w = host.wake.clone();
                            upd.check_async(false, move || {
                                w.wake();
                            });
                        }
                    } else if host.auto_update_checked.get() && !host.auto_update_handled.get() {
                        let snap = host.ctrl.borrow().updater.snapshot();
                        if !snap.busy {
                            if snap.phase == crate::updater::UpdatePhase::Available {
                                host.auto_update_handled.set(true);
                                let mode = host.ctrl.borrow().settings.update_mode;
                                let ver = snap.available_version.clone().unwrap_or_default();
                                if mode == UpdateMode::Auto {
                                    host.tray.show_balloon(
                                        &dualmix_core::loc::get("Updater.BalloonTitle"),
                                        &dualmix_core::loc::fmt("Updater.BalloonInstalling", &[&ver]),
                                    );
                                    let upd = host.ctrl.borrow().updater.clone();
                                    let w = host.wake.clone();
                                    upd.install_async(move || {
                                        w.wake();
                                    });
                                } else if mode == UpdateMode::Notify {
                                    host.tray.show_balloon(
                                        &dualmix_core::loc::fmt("Updater.BalloonAvailableTitle", &[&ver]),
                                        &dualmix_core::loc::get("Updater.BalloonManual"),
                                    );
                                }
                            } else if !matches!(snap.phase, crate::updater::UpdatePhase::Checking) {
                                host.auto_update_handled.set(true);
                            }
                        }
                    }
                    LRESULT(0)
                }
                controller::WM_APP_WAKE => {
                    host.wake.clear_pending();
                    if let Ok(mut c) = host.ctrl.try_borrow_mut() {
                        c.tick(false);
                    }
                    if !host.modal_open.get() {
                        host.drain_controller_events();
                    }
                    LRESULT(0)
                }
                dualmix_ui::tray::WM_TRAY_ICON => {
                    if host.modal_open.get() {
                        return LRESULT(0);
                    }
                    let status = host.ctrl.borrow().tray_status();
                    host.modal_open.set(true);
                    let act = host.tray.handle_message(lparam, &status);
                    host.modal_open.set(false);
                    if let Some(act) = act {
                        match act {
                            dualmix_ui::tray::TrayAction::Open => host.open_main(),
                            dualmix_ui::tray::TrayAction::AllGame => {
                                host.ctrl.borrow_mut().set_mix(-1.0, true);
                                host.drain_controller_events();
                            }
                            dualmix_ui::tray::TrayAction::Balanced => {
                                host.ctrl.borrow_mut().set_mix(0.0, true);
                                host.drain_controller_events();
                            }
                            dualmix_ui::tray::TrayAction::AllChat => {
                                host.ctrl.borrow_mut().set_mix(1.0, true);
                                host.drain_controller_events();
                            }
                            dualmix_ui::tray::TrayAction::SetTarget(tgt) => {
                                host.ctrl.borrow_mut().set_knob_target(tgt, true);
                                host.drain_controller_events();
                            }
                            dualmix_ui::tray::TrayAction::ToggleAutostart => {
                                let next = !host.ctrl.borrow().settings.start_with_windows;
                                host.ctrl.borrow_mut().handle_ui_command(dualmix_ui::state::UiCommand::SetAutostart { enabled: next });
                                host.drain_controller_events();
                            }
                            dualmix_ui::tray::TrayAction::Exit => {
                                host.shutdown(hwnd);
                            }
                        }
                    }
                    LRESULT(0)
                }
                WM_QUERYENDSESSION => LRESULT(1),
                WM_ENDSESSION => {
                    if wparam.0 != 0 {
                        host.shutdown(hwnd);
                    }
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default().into() };
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(host_proc),
            hInstance: hinst,
            lpszClassName: w!("DualMixHostMsgWindow"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }

    // Create host hidden window first, then bind TrayIcon & AppHostState
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("DualMixHostMsgWindow"),
            w!("DualMixHost"),
            Default::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            hinst,
            None,
        )
        .unwrap_or_default()
    };
    wake.set_hwnd(hwnd);

    let tray = dualmix_ui::tray::TrayIcon::new(hwnd);
    let osd = dualmix_ui::osd::OsdWindow::create();
    let host = Rc::new(AppHostState {
        ctrl: ctrl.clone(),
        wake: wake.clone(),
        host_hwnd: hwnd,
        tray,
        osd,
        main_win: RefCell::new(None),
        wiz_win: RefCell::new(None),
        show_ev,
        exit_ev,
        running: Cell::new(true),
        fast_timer: Cell::new(false),
        modal_open: Cell::new(false),
        auto_update_checked: Cell::new(false),
        auto_update_handled: Cell::new(false),
        started_at: Instant::now(),
    });

    let raw_host = Rc::into_raw(host.clone());
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_host as isize);
        SetTimer(hwnd, 1, 250, None);
    }

    let first_run = !ctrl.borrow().settings.first_run_done;
    let start_min = ctrl.borrow().settings.start_minimized || start_tray;
    if force_wizard || (first_run && !start_tray) {
        host.open_wizard();
    } else if !start_min {
        host.open_main();
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Rc::from_raw(raw_host));
        let _ = CloseHandle(show_ev);
        let _ = CloseHandle(exit_ev);
        let _ = CloseHandle(mutex);
    }

    0
}
