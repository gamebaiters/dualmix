//! Native Win32 host window + statically-linked Microsoft Edge WebView2 Fluent 2 UI.
//!
//! Renders `assets/ui.html` (both the 5-tab `MainWindow` and the 3-step `SetupWindow`) offline
//! with zero external DLLs (`WebView2LoaderStatic.lib` via `webview2-com`).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dualmix_core::loc;
use dualmix_core::log;
use serde::Serialize;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_COLOR,
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2Controller2,
};
use webview2_com::{
    CapturePreviewCompletedHandler, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, ExecuteScriptCompletedHandler,
    NavigationCompletedEventHandler, WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{E_FAIL, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{HBRUSH, UpdateWindow};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::StructuredStorage::GetHGlobalFromStream;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GWLP_USERDATA, HICON,
    ICON_BIG, ICON_SMALL, IDC_ARROW, LR_DEFAULTCOLOR, LoadCursorW, LookupIconIdFromDirectoryEx,
    MINMAXINFO, MSG, PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassExW, SC_CLOSE,
    SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_GETMINMAXINFO, WM_NCCREATE, WM_SETICON, WM_SHOWWINDOW, WM_SIZE,
    WM_SYSCOMMAND, WNDCLASSEXW, WS_CAPTION, WS_CLIPCHILDREN, WS_MINIMIZEBOX,
    WS_OVERLAPPEDWINDOW, WS_SYSMENU, HWND_TOP, SWP_NOMOVE, SWP_NOZORDER,
};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR, w};

use crate::dialogs::apply_window_mica_and_theme;
use crate::state::{UiCommand, UiState};

const HTML_TEMPLATE: &str = include_str!("../assets/ui.html");
const ICON_BYTES: &[u8] = include_bytes!("../assets/dualmix.ico");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Main,
    Wizard,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePatch {
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
    pub app_peaks: Vec<(String, f32, i32, bool)>,
    pub router_running: bool,
    pub router_idle: bool,
    pub engine_chip: String,
    pub engine_text: String,
    pub latency_text: String,
    pub format_text: String,
    pub technical_details: String,
    pub quality_summary: String,
    pub knob_target: dualmix_core::settings::KnobTarget,
    pub knob_target_text: String,
    pub last_key_seen: String,
    pub keyboard_status: String,
}

pub type CommandCallback = Box<dyn Fn(UiCommand)>;
pub type BoolCallback = Box<dyn Fn(bool)>;
pub type VoidCallback = Box<dyn Fn()>;

struct Inner {
    kind: WindowKind,
    hwnd: HWND,
    dark: Cell<bool>,
    visible: Cell<bool>,
    minimized: Cell<bool>,
    hide_on_close: Cell<bool>,
    force_closing: Cell<bool>,
    min_w_96: i32,
    min_h_96: i32,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    webview: RefCell<Option<ICoreWebView2>>,
    page_ready: Cell<bool>,
    js_ready: Cell<bool>,
    pending_state: RefCell<Option<String>>,
    on_command: CommandCallback,
    on_visibility: BoolCallback,
    on_closed: VoidCallback,
}

pub struct FluentWebWindow {
    inner: Rc<Inner>,
}

static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
static OLE_INIT: AtomicBool = AtomicBool::new(false);

fn ensure_ole() {
    if !OLE_INIT.swap(true, Ordering::AcqRel) {
        unsafe {
            let _ = OleInitialize(None);
        }
    }
}

fn ensure_class() {
    if CLASS_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            lpszClassName: w!("DualMixWebHostWindow"),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
}

fn load_icon_size(px: i32) -> HICON {
    unsafe {
        let offset = LookupIconIdFromDirectoryEx(ICON_BYTES.as_ptr(), true, px, px, LR_DEFAULTCOLOR);
        if offset > 0 && (offset as usize) < ICON_BYTES.len() {
            if let Ok(ic) = CreateIconFromResourceEx(
                &ICON_BYTES[offset as usize..],
                true,
                0x00030000,
                px,
                px,
                LR_DEFAULTCOLOR,
            ) {
                return ic;
            }
        }
    }
    HICON::default()
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn build_html() -> String {
    let icon_b64 = base64_encode(ICON_BYTES);
    let icon_uri = format!("data:image/x-icon;base64,{icon_b64}");
    HTML_TEMPLATE
        .replace("__LOC_JSON__", loc::TABLE_JSON)
        .replace("__ICON_URI__", &icon_uri)
}

fn webview_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(dualmix_core::log::data_dir);
    let dir = base.join("DualMix").join("WebView2");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

impl FluentWebWindow {
    pub fn new(
        kind: WindowKind,
        title: &str,
        dark: bool,
        on_command: CommandCallback,
        on_visibility: BoolCallback,
        on_closed: VoidCallback,
    ) -> Result<Self, String> {
        ensure_ole();
        ensure_class();

        let (logical_w, logical_h, min_w, min_h, style) = match kind {
            WindowKind::Main => (1040, 740, 820, 560, WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN),
            WindowKind::Wizard => (
                680,
                600,
                640,
                540,
                (WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN),
            ),
        };

        let inner = Rc::new(Inner {
            kind,
            hwnd: HWND::default(),
            dark: Cell::new(dark),
            visible: Cell::new(false),
            minimized: Cell::new(false),
            hide_on_close: Cell::new(kind == WindowKind::Main),
            force_closing: Cell::new(false),
            min_w_96: min_w,
            min_h_96: min_h,
            controller: RefCell::new(None),
            webview: RefCell::new(None),
            page_ready: Cell::new(false),
            js_ready: Cell::new(false),
            pending_state: RefCell::new(None),
            on_command,
            on_visibility,
            on_closed,
        });

        let hinst: HINSTANCE = unsafe { GetModuleHandleW(None).unwrap_or_default().into() };
        let title_w = HSTRING::from(title);

        let raw_ptr = Rc::into_raw(inner.clone());
        let hwnd = unsafe {
            let sx = GetSystemMetrics(SM_CXSCREEN).max(1280);
            let sy = GetSystemMetrics(SM_CYSCREEN).max(720);
            let x = (sx - logical_w).max(40) / 2;
            let y = (sy - logical_h).max(20) / 2;
            CreateWindowExW(
                Default::default(),
                w!("DualMixWebHostWindow"),
                &title_w,
                style,
                x,
                y,
                logical_w,
                logical_h,
                None,
                None,
                hinst,
                Some(raw_ptr as *const _),
            )
            .map_err(|e| format!("CreateWindowExW failed: {e}"))?
        };

        // Rebalance Rc: `WM_NCCREATE` took ownership of `raw_ptr` only if stored; drop extra if not stored.
        unsafe {
            let stored = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Inner;
            if stored.is_null() {
                drop(Rc::from_raw(raw_ptr));
                return Err("Window userdata missing".to_string());
            }
            // Scale to current monitor DPI cleanly.
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let w_scaled = logical_w * dpi / 96;
            let h_scaled = logical_h * dpi / 96;
            let sx = GetSystemMetrics(SM_CXSCREEN).max(w_scaled);
            let sy = GetSystemMetrics(SM_CYSCREEN).max(h_scaled);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOP,
                (sx - w_scaled).max(0) / 2,
                (sy - h_scaled).max(0) / 2,
                w_scaled,
                h_scaled,
                SWP_NOZORDER,
            );
            let big = load_icon_size(32);
            let small = load_icon_size(16);
            if !big.is_invalid() {
                PostMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(big.0 as isize)).ok();
            }
            if !small.is_invalid() {
                PostMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(small.0 as isize)).ok();
            }
        }
        apply_window_mica_and_theme(hwnd, dark);
        Self::init_webview(&inner)?;
        Ok(Self { inner })
    }

    fn init_webview(inner: &Rc<Inner>) -> Result<(), String> {
        let user_data = webview_data_dir();
        let user_data_h = HSTRING::from(user_data.to_string_lossy().as_ref());
        let weak = Rc::downgrade(inner);

        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |err, env| {
            if let Err(e) = err {
                log::error(format!("WebView2 env creation error: {e}"));
                return Ok(());
            }
            let Some(env) = env else { return Ok(()) };
            let Some(inner) = weak.upgrade() else { return Ok(()) };
            let hwnd = inner.hwnd;
            let weak2 = Rc::downgrade(&inner);

            let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |err, ctrl| {
                if let Err(e) = err {
                    log::error(format!("WebView2 controller creation error: {e}"));
                    return Ok(());
                }
                let Some(ctrl) = ctrl else { return Ok(()) };
                let Some(inner) = weak2.upgrade() else { return Ok(()) };

                unsafe {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(inner.hwnd, &mut rc);
                    let _ = ctrl.SetBounds(rc);
                    let _ = ctrl.SetIsVisible(inner.visible.get() || inner.kind == WindowKind::Wizard);
                    if let Ok(ctrl2) = ctrl.cast::<ICoreWebView2Controller2>() {
                        let bg = if inner.dark.get() {
                            COREWEBVIEW2_COLOR { A: 255, R: 32, G: 32, B: 32 }
                        } else {
                            COREWEBVIEW2_COLOR { A: 255, R: 243, G: 243, B: 243 }
                        };
                        let _ = ctrl2.SetDefaultBackgroundColor(bg);
                    }
                }

                let Ok(wv) = (unsafe { ctrl.CoreWebView2() }) else {
                    return Ok(());
                };
                unsafe {
                    if let Ok(settings) = wv.Settings() {
                        let _ = settings.SetAreDefaultContextMenusEnabled(false);
                        let _ = settings.SetAreDevToolsEnabled(false);
                        let _ = settings.SetIsStatusBarEnabled(false);
                        let _ = settings.SetIsZoomControlEnabled(false);
                        let _ = settings.SetAreDefaultScriptDialogsEnabled(false);
                    }
                }

                let weak_msg = Rc::downgrade(&inner);
                let mut msg_tok = Default::default();
                let msg_handler = WebMessageReceivedEventHandler::create(Box::new(move |_wv, args| {
                    let Some(args) = args else { return Ok(()) };
                    let Some(inner) = weak_msg.upgrade() else { return Ok(()) };
                    let mut raw = PWSTR::null();
                    if unsafe { args.TryGetWebMessageAsString(&mut raw) }.is_ok() && !raw.is_null() {
                        let txt = unsafe { raw.to_string().unwrap_or_default() };
                        if let Ok(cmd) = serde_json::from_str::<UiCommand>(&txt) {
                            if matches!(cmd, UiCommand::Ready) {
                                inner.js_ready.set(true);
                                inner.flush_pending();
                            }
                            (inner.on_command)(cmd);
                        } else {
                            log::warn(format!("WebMessage unparsed: {txt}"));
                        }
                    }
                    Ok(())
                }));
                let _ = unsafe { wv.add_WebMessageReceived(&msg_handler, &mut msg_tok) };

                let weak_nav = Rc::downgrade(&inner);
                let mut nav_tok = Default::default();
                let nav_handler = NavigationCompletedEventHandler::create(Box::new(move |_wv, _args| {
                    if let Some(inner) = weak_nav.upgrade() {
                        inner.page_ready.set(true);
                        inner.flush_pending();
                    }
                    Ok(())
                }));
                let _ = unsafe { wv.add_NavigationCompleted(&nav_handler, &mut nav_tok) };

                let html = build_html();
                let html_h = HSTRING::from(html);
                let _ = unsafe { wv.NavigateToString(&html_h) };

                *inner.controller.borrow_mut() = Some(ctrl);
                *inner.webview.borrow_mut() = Some(wv);
                Ok(())
            }));

            let _ = unsafe { env.CreateCoreWebView2Controller(hwnd, &ctrl_handler) };
            Ok(())
        }));

        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR(user_data_h.as_ptr()),
                None,
                &handler,
            )
            .map_err(|e| format!("CreateCoreWebView2EnvironmentWithOptions failed: {e}"))?;
        }
        Ok(())
    }

    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd
    }

    pub fn set_hide_on_close(&self, hide: bool) {
        self.inner.hide_on_close.set(hide);
    }

    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.inner.hwnd, SW_SHOW);
            let _ = ShowWindow(self.inner.hwnd, SW_RESTORE);
            let _ = UpdateWindow(self.inner.hwnd);
            let _ = SetForegroundWindow(self.inner.hwnd);
        }
        self.inner.visible.set(true);
        self.inner.minimized.set(false);
        if let Some(ctrl) = self.inner.controller.borrow().as_ref() {
            let _ = unsafe { ctrl.SetIsVisible(true) };
        }
        (self.inner.on_visibility)(true);
    }

    pub fn show_unfocused(&self) {
        unsafe {
            let _ = ShowWindow(self.inner.hwnd, SW_SHOWNORMAL);
            let _ = UpdateWindow(self.inner.hwnd);
        }
        self.inner.visible.set(true);
        self.inner.minimized.set(false);
        if let Some(ctrl) = self.inner.controller.borrow().as_ref() {
            let _ = unsafe { ctrl.SetIsVisible(true) };
        }
        (self.inner.on_visibility)(true);
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.inner.hwnd, SW_HIDE);
        }
        self.inner.visible.set(false);
        if let Some(ctrl) = self.inner.controller.borrow().as_ref() {
            let _ = unsafe { ctrl.SetIsVisible(false) };
        }
        (self.inner.on_visibility)(false);
    }

    pub fn really_close(&self) {
        self.inner.force_closing.set(true);
        unsafe {
            let _ = DestroyWindow(self.inner.hwnd);
        }
    }

    pub fn is_visible(&self) -> bool {
        self.inner.visible.get() && !self.inner.minimized.get()
    }

    pub fn set_dark(&self, dark: bool) {
        if self.inner.dark.get() != dark {
            self.inner.dark.set(dark);
            apply_window_mica_and_theme(self.inner.hwnd, dark);
            if let Some(ctrl) = self.inner.controller.borrow().as_ref() {
                if let Ok(ctrl2) = ctrl.cast::<ICoreWebView2Controller2>() {
                    let bg = if dark {
                        COREWEBVIEW2_COLOR { A: 255, R: 32, G: 32, B: 32 }
                    } else {
                        COREWEBVIEW2_COLOR { A: 255, R: 243, G: 243, B: 243 }
                    };
                    let _ = unsafe { ctrl2.SetDefaultBackgroundColor(bg) };
                }
            }
        }
    }

    pub fn set_title(&self, title: &str) {
        let h = HSTRING::from(title);
        let _ = unsafe { SetWindowTextW(self.inner.hwnd, &h) };
    }

    pub fn push_state(&self, state: &UiState) {
        self.set_dark(state.dark);
        if let Ok(json) = serde_json::to_string(state) {
            *self.inner.pending_state.borrow_mut() = Some(json);
            self.inner.flush_pending();
        }
    }

    pub fn push_live(&self, live: &LivePatch) {
        if !self.inner.js_ready.get() || !self.is_visible() {
            return;
        }
        if let Ok(json) = serde_json::to_string(live) {
            let script = format!("if(window.__dualmixLive)window.__dualmixLive({json});");
            self.inner.exec_script(&script);
        }
    }

    pub fn scroll_to_end(&self) {
        self.inner.exec_script("if(window.__dualmixScrollEnd)window.__dualmixScrollEnd();");
    }

    /// Pumps the Win32 message loop until WebView2 navigation + JS bridge are ready (used by `--snapshot`).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.inner.js_ready.get() && self.inner.page_ready.get() {
                pump_messages_for(Duration::from_millis(80));
                return true;
            }
            pump_messages_once();
        }
        false
    }

    pub fn pump_ms(&self, ms: u64) {
        pump_messages_for(Duration::from_millis(ms));
    }

    /// Captures the rendered WebView2 viewport to a PNG file using `ICoreWebView2::CapturePreview`.
    pub fn capture_png(&self, path: &Path) -> Result<(), String> {
        let wv = self
            .inner
            .webview
            .borrow()
            .clone()
            .ok_or_else(|| "WebView2 not initialized".to_string())?;
        let stream: IStream = unsafe {
            CreateStreamOnHGlobal(HGLOBAL::default(), true)
                .map_err(|e| format!("CreateStreamOnHGlobal failed: {e}"))?
        };
        let done = Rc::new(Cell::new(false));
        let failed = Rc::new(RefCell::new(None::<String>));
        let done2 = done.clone();
        let failed2 = failed.clone();

        let handler = CapturePreviewCompletedHandler::create(Box::new(move |res| {
            if let Err(e) = res {
                *failed2.borrow_mut() = Some(format!("CapturePreview error: {e}"));
            }
            done2.set(true);
            Ok(())
        }));

        unsafe {
            wv.CapturePreview(
                COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                &stream,
                &handler,
            )
            .map_err(|e| format!("CapturePreview call failed: {e}"))?;
        }

        let start = Instant::now();
        while !done.get() && start.elapsed() < Duration::from_secs(10) {
            pump_messages_once();
        }
        if !done.get() {
            return Err("CapturePreview timed out".to_string());
        }
        if let Some(err) = failed.borrow().as_ref() {
            return Err(err.clone());
        }

        let hglobal = unsafe { GetHGlobalFromStream(&stream).map_err(|e| format!("GetHGlobalFromStream: {e}"))? };
        let bytes = unsafe {
            let size = GlobalSize(hglobal);
            if size == 0 {
                return Err("CapturePreview produced empty buffer".to_string());
            }
            let ptr = GlobalLock(hglobal) as *const u8;
            if ptr.is_null() {
                return Err("GlobalLock failed".to_string());
            }
            let slice = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(hglobal);
            slice
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, bytes).map_err(|e| format!("Write {}: {e}", path.display()))
    }
}

impl Inner {
    fn exec_script(&self, script: &str) {
        if let Some(wv) = self.webview.borrow().as_ref() {
            let h = HSTRING::from(script);
            let cb = ExecuteScriptCompletedHandler::create(Box::new(|_res, _out| Ok(())));
            let _ = unsafe { wv.ExecuteScript(&h, &cb) };
        }
    }

    fn flush_pending(&self) {
        if !self.js_ready.get() {
            return;
        }
        if let Some(json) = self.pending_state.borrow_mut().take() {
            let script = format!("if(window.__dualmixSetState)window.__dualmixSetState({json});");
            self.exec_script(&script);
        }
    }
}

pub fn pump_messages_once() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    std::thread::sleep(Duration::from_millis(8));
}

pub fn pump_messages_for(dur: Duration) {
    let start = Instant::now();
    while start.elapsed() < dur {
        pump_messages_once();
    }
}

pub fn run_message_loop_until<F: FnMut() -> bool>(mut should_stop: F) {
    unsafe {
        let mut msg = MSG::default();
        while !should_stop() {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if !ret.as_bool() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
            if !cs.lpCreateParams.is_null() {
                let inner = &mut *(cs.lpCreateParams as *mut Inner);
                inner.hwnd = hwnd;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Inner;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let inner = &*ptr;

        match msg {
            WM_SIZE => {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                if let Some(ctrl) = inner.controller.borrow().as_ref() {
                    let _ = ctrl.SetBounds(rc);
                }
                // SIZE_MINIMIZED = 1
                let is_min = wparam.0 == 1;
                if inner.minimized.get() != is_min {
                    inner.minimized.set(is_min);
                    if let Some(ctrl) = inner.controller.borrow().as_ref() {
                        let _ = ctrl.SetIsVisible(!is_min && inner.visible.get());
                    }
                    (inner.on_visibility)(!is_min && inner.visible.get());
                }
                LRESULT(0)
            }
            WM_SHOWWINDOW => {
                let vis = wparam.0 != 0;
                inner.visible.set(vis);
                if let Some(ctrl) = inner.controller.borrow().as_ref() {
                    let _ = ctrl.SetIsVisible(vis && !inner.minimized.get());
                }
                (inner.on_visibility)(vis && !inner.minimized.get());
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                mmi.ptMinTrackSize.x = inner.min_w_96 * dpi / 96;
                mmi.ptMinTrackSize.y = inner.min_h_96 * dpi / 96;
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER,
                );
                LRESULT(0)
            }
            WM_SYSCOMMAND => {
                if (wparam.0 & 0xFFF0) as u32 == SC_CLOSE && inner.hide_on_close.get() && !inner.force_closing.get() {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    inner.visible.set(false);
                    if let Some(ctrl) = inner.controller.borrow().as_ref() {
                        let _ = ctrl.SetIsVisible(false);
                    }
                    (inner.on_visibility)(false);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                if inner.hide_on_close.get() && !inner.force_closing.get() {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    inner.visible.set(false);
                    if let Some(ctrl) = inner.controller.borrow().as_ref() {
                        let _ = ctrl.SetIsVisible(false);
                    }
                    (inner.on_visibility)(false);
                    return LRESULT(0);
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(ctrl) = inner.controller.borrow_mut().take() {
                    let _ = ctrl.Close();
                }
                *inner.webview.borrow_mut() = None;
                (inner.on_closed)();
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Rc::from_raw(ptr));
                LRESULT(0)
            }
            _ => {
                let _ = SWP_NOMOVE;
                let _ = E_FAIL;
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }
}
