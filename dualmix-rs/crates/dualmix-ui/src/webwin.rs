//! Top-level Win32 window hosting a WebView2 page (`assets/ui.html`): the settings window and the setup wizard.
//!
//! Design rules (the previous version froze the UI):
//! * every WebView2 callback only forwards a [`WebEvent`] to the host sink: no application logic runs inside them,
//!   nothing is borrowed across a callback, nothing pumps messages;
//! * the native title bar is kept (dark mode + Mica through DWM), so moving/resizing/snap layouts are Windows' own;
//! * messages to the page are queued until the page says it is ready, then posted with `PostWebMessageAsJson`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};


use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_COLOR, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Controller2,
};
use webview2_com::{
    CapturePreviewCompletedHandler, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetClientRect,
    GetSystemMetrics, GetWindowLongPtrW, HICON, ICON_BIG, ICON_SMALL, IDC_ARROW, IsIconic, IsWindowVisible, LR_DEFAULTCOLOR,
    LoadCursorW, LookupIconIdFromDirectoryEx, MINMAXINFO, MSG, PM_REMOVE, PeekMessageW, RegisterClassExW, SM_CXSCREEN,
    SM_CYSCREEN, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WM_CLOSE,
    WM_DPICHANGED, WM_GETMINMAXINFO, WM_MOVE, WM_NCCREATE, WM_NCDESTROY, WM_SETICON, WM_SIZE, WNDCLASSEXW, WS_CLIPCHILDREN,
    WS_OVERLAPPEDWINDOW, WS_EX_APPWINDOW,
};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR, w};

const HTML_TEMPLATE: &str = include_str!("../assets/ui.html");
const ICON_BYTES: &[u8] = include_bytes!("../assets/dualmix.ico");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinKind {
    Main,
    Wizard,
}

#[derive(Debug, Clone)]
pub enum WebEvent {
    /// The page sent a JSON message.
    Message(WinKind, String),
    /// The page finished loading and can receive messages.
    Ready(WinKind),
    /// The user clicked the close button (the host decides: hide or destroy).
    CloseRequested(WinKind),
    /// The window was shown / hidden / minimized.
    Visibility(WinKind, bool),
    /// WebView2 could not be created (runtime missing).
    Failed(WinKind, String),
}

pub type Sink = Rc<dyn Fn(WebEvent)>;

struct Inner {
    kind: WinKind,
    hwnd: Cell<HWND>,
    controller: RefCell<Option<ICoreWebView2Controller>>,
    webview: RefCell<Option<ICoreWebView2>>,
    ready: Cell<bool>,
    pending: RefCell<Vec<String>>,
    sink: Sink,
    min_w: i32,
    min_h: i32,
    dark: Cell<bool>,
    visible: Cell<bool>,
}

#[derive(Clone)]
pub struct WebWindow {
    inner: Rc<Inner>,
}

static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

pub fn load_icon(px: i32) -> HICON {
    unsafe {
        let off = LookupIconIdFromDirectoryEx(ICON_BYTES.as_ptr(), true, px, px, LR_DEFAULTCOLOR);
        if off > 0 && (off as usize) < ICON_BYTES.len() {
            if let Ok(h) = CreateIconFromResourceEx(&ICON_BYTES[off as usize..], true, 0x0003_0000, px, px, LR_DEFAULTCOLOR) {
                return h;
            }
        }
        HICON::default()
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn build_html(kind: WinKind) -> String {
    HTML_TEMPLATE
        .replace("__LOC_JSON__", dualmix_core::loc::TABLE_JSON)
        .replace("__ICON_URI__", &format!("data:image/x-icon;base64,{}", base64(ICON_BYTES)))
        .replace("__WINDOW_KIND__", if kind == WinKind::Main { "main" } else { "wizard" })
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("DualMix").join("WebView2")
}

/// Dark title bar and Mica backdrop (Windows 11; ignored elsewhere).
pub fn apply_dwm(hwnd: HWND, dark: bool) {
    unsafe {
        let v: i32 = dark as i32;
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(20), &v as *const _ as *const _, 4); // DWMWA_USE_IMMERSIVE_DARK_MODE
        let mica: i32 = 2; // DWMSBT_MAINWINDOW
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(38), &mica as *const _ as *const _, 4);
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
            hIcon: load_icon(32),
            hIconSm: load_icon(16),
            lpszClassName: w!("DualMixWebHostWindow"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

impl WebWindow {
    /// Creates the window (hidden) and starts loading the page asynchronously.
    pub fn create(kind: WinKind, title: &str, dark: bool, sink: Sink) -> Result<Self, String> {
        ensure_class();
        let (w96, h96, min_w, min_h) = match kind {
            WinKind::Main => (1040, 780, 760, 560),
            WinKind::Wizard => (720, 640, 640, 560),
        };
        let inner = Rc::new(Inner {
            kind,
            hwnd: Cell::new(HWND::default()),
            controller: RefCell::new(None),
            webview: RefCell::new(None),
            ready: Cell::new(false),
            pending: RefCell::new(Vec::new()),
            sink,
            min_w,
            min_h,
            dark: Cell::new(dark),
            visible: Cell::new(false),
        });
        let raw = Rc::into_raw(Rc::clone(&inner));
        let hwnd = unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            CreateWindowExW(
                WS_EX_APPWINDOW,
                w!("DualMixWebHostWindow"),
                &HSTRING::from(title),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                0,
                0,
                w96,
                h96,
                None,
                None,
                hinst,
                Some(raw as *const _),
            )
        };
        let hwnd = match hwnd {
            Ok(h) => h,
            Err(e) => {
                unsafe { drop(Rc::from_raw(raw)) };
                return Err(format!("CreateWindowExW failed: {e}"));
            }
        };
        inner.hwnd.set(hwnd);
        unsafe {
            // Size for the monitor DPI and centre on the primary screen.
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let (w, h) = (w96 * dpi / 96, h96 * dpi / 96);
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let (w, h) = (w.min(sw - 20), h.min(sh - 60));
            let _ = SetWindowPos(hwnd, None, ((sw - w) / 2).max(0), ((sh - h) / 2 - 20).max(0), w, h, SWP_NOZORDER | SWP_NOACTIVATE);
            let big = load_icon(32);
            let small = load_icon(16);
            SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(big.0 as isize));
            SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(small.0 as isize));
        }
        apply_dwm(hwnd, dark);
        let me = Self { inner };
        me.init_webview()?;
        Ok(me)
    }

    fn init_webview(&self) -> Result<(), String> {
        let weak = Rc::downgrade(&self.inner);
        let data = data_dir();
        let _ = std::fs::create_dir_all(&data);
        let data_h = HSTRING::from(data.as_os_str());
        let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |res, env| {
            let Some(inner) = weak.upgrade() else { return Ok(()) };
            let env = match (res, env) {
                (Ok(()), Some(env)) => env,
                (r, _) => {
                    (inner.sink)(WebEvent::Failed(inner.kind, format!("WebView2 environment: {r:?}")));
                    return Ok(());
                }
            };
            let weak2 = Rc::downgrade(&inner);
            let ctrl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |res, ctrl| {
                let Some(inner) = weak2.upgrade() else { return Ok(()) };
                let ctrl = match (res, ctrl) {
                    (Ok(()), Some(c)) => c,
                    (r, _) => {
                        (inner.sink)(WebEvent::Failed(inner.kind, format!("WebView2 controller: {r:?}")));
                        return Ok(());
                    }
                };
                unsafe {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(inner.hwnd.get(), &mut rc);
                    let _ = ctrl.SetBounds(rc);
                    let _ = ctrl.SetIsVisible(IsWindowVisible(inner.hwnd.get()).as_bool());
                    if let Ok(c2) = ctrl.cast::<ICoreWebView2Controller2>() {
                        let bg = if inner.dark.get() { COREWEBVIEW2_COLOR { A: 255, R: 32, G: 32, B: 32 } } else { COREWEBVIEW2_COLOR { A: 255, R: 243, G: 243, B: 243 } };
                        let _ = c2.SetDefaultBackgroundColor(bg);
                    }
                }
                let wv = match unsafe { ctrl.CoreWebView2() } {
                    Ok(w) => w,
                    Err(e) => {
                        (inner.sink)(WebEvent::Failed(inner.kind, format!("CoreWebView2: {e}")));
                        return Ok(());
                    }
                };
                unsafe {
                    if let Ok(s) = wv.Settings() {
                        let _ = s.SetAreDefaultContextMenusEnabled(false);
                        let _ = s.SetAreDevToolsEnabled(std::env::var_os("DUALMIX_DEVTOOLS").is_some());
                        let _ = s.SetIsStatusBarEnabled(false);
                        let _ = s.SetIsZoomControlEnabled(false);
                        let _ = s.SetAreDefaultScriptDialogsEnabled(false);
                        let _ = s.SetIsBuiltInErrorPageEnabled(false);
                    }
                }
                let weak3 = Rc::downgrade(&inner);
                let msg_handler = WebMessageReceivedEventHandler::create(Box::new(move |_wv, args| {
                    let (Some(args), Some(inner)) = (args, weak3.upgrade()) else { return Ok(()) };
                    let mut raw = PWSTR::null();
                    if unsafe { args.TryGetWebMessageAsString(&mut raw) }.is_ok() && !raw.is_null() {
                        let text = unsafe { raw.to_string().unwrap_or_default() };
                        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(raw.as_ptr() as *const _)) };
                        if text.contains("\"cmd\":\"ready\"") {
                            inner.ready.set(true);
                            inner.flush();
                            (inner.sink)(WebEvent::Ready(inner.kind));
                        } else {
                            (inner.sink)(WebEvent::Message(inner.kind, text));
                        }
                    }
                    Ok(())
                }));
                let mut token = Default::default();
                unsafe {
                    let _ = wv.add_WebMessageReceived(&msg_handler, &mut token);
                    let _ = wv.NavigateToString(&HSTRING::from(build_html(inner.kind)));
                }
                *inner.controller.borrow_mut() = Some(ctrl);
                *inner.webview.borrow_mut() = Some(wv);
                Ok(())
            }));
            unsafe {
                let _ = env.CreateCoreWebView2Controller(inner.hwnd.get(), &ctrl_handler);
            }
            Ok(())
        }));
        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), PCWSTR(data_h.as_ptr()), None, &env_handler)
                .map_err(|e| format!("WebView2 runtime not available: {e}"))
        }
    }

    pub fn kind(&self) -> WinKind {
        self.inner.kind
    }

    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd.get()
    }

    pub fn is_ready(&self) -> bool {
        self.inner.ready.get()
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd()).as_bool() && !IsIconic(self.hwnd()).as_bool() }
    }

    /// Sends a JSON message to the page (queued until the page is ready).
    pub fn post(&self, json: &str) {
        if self.inner.ready.get() {
            self.inner.send(json);
        } else {
            let mut p = self.inner.pending.borrow_mut();
            if p.len() > 64 {
                p.remove(0);
            }
            p.push(json.to_string());
        }
    }

    pub fn exec(&self, script: &str) {
        self.inner.exec(script);
    }

    pub fn set_title(&self, title: &str) {
        unsafe {
            let _ = SetWindowTextW(self.hwnd(), &HSTRING::from(title));
        }
    }

    pub fn set_dark(&self, dark: bool) {
        self.inner.dark.set(dark);
        apply_dwm(self.hwnd(), dark);
    }

    pub fn show(&self, activate: bool) {
        let h = self.hwnd();
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            } else {
                let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
            }
            if activate {
                let _ = SetForegroundWindow(h);
                if let Some(c) = self.inner.controller.borrow().as_ref() {
                    let _ = c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                }
            }
        }
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd(), SW_HIDE);
        }
    }

    pub fn destroy(&self) {
        if let Some(c) = self.inner.controller.borrow_mut().take() {
            unsafe {
                let _ = c.Close();
            }
        }
        self.inner.webview.borrow_mut().take();
        let h = self.hwnd();
        if !h.is_invalid() {
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
    }

    /// Developer aid (`--snapshot`): renders the page to a PNG. Pumps messages while waiting: snapshot mode only.
    pub fn capture_png(&self, path: &Path) -> Result<(), String> {
        let wv = self.inner.webview.borrow().clone().ok_or("WebView2 not ready")?;
        let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true).map_err(|e| e.to_string())? };
        let done = Rc::new(Cell::new(None::<bool>));
        let d2 = Rc::clone(&done);
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |res| {
            d2.set(Some(res.is_ok()));
            Ok(())
        }));
        unsafe {
            wv.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler).map_err(|e| e.to_string())?;
        }
        let start = Instant::now();
        while done.get().is_none() && start.elapsed() < Duration::from_secs(10) {
            pump_for(Duration::from_millis(10));
        }
        if done.get() != Some(true) {
            return Err("CapturePreview failed".into());
        }
        unsafe {
            let hg = GetHGlobalFromStream(&stream).map_err(|e| e.to_string())?;
            let size = GlobalSize(hg);
            let p = GlobalLock(hg) as *const u8;
            if p.is_null() || size == 0 {
                return Err("empty capture".into());
            }
            let bytes = std::slice::from_raw_parts(p, size).to_vec();
            let _ = GlobalUnlock(hg);
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::write(path, bytes).map_err(|e| e.to_string())
        }
    }
}

impl Inner {
    fn send(&self, json: &str) {
        if let Some(wv) = self.webview.borrow().as_ref() {
            unsafe {
                let _ = wv.PostWebMessageAsJson(&HSTRING::from(json));
            }
        }
    }

    fn flush(&self) {
        let items: Vec<String> = self.pending.borrow_mut().drain(..).collect();
        for j in items {
            self.send(&j);
        }
    }

    fn exec(&self, script: &str) {
        if let Some(wv) = self.webview.borrow().as_ref() {
            let cb = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
            unsafe {
                let _ = wv.ExecuteScript(&HSTRING::from(script), &cb);
            }
        }
    }

    fn resize(&self) {
        if let Ok(c) = self.controller.try_borrow() {
            if let Some(c) = c.as_ref() {
                unsafe {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(self.hwnd.get(), &mut rc);
                    let _ = c.SetBounds(rc);
                }
            }
        }
    }
}

/// Pumps the message queue for a while (snapshot mode only).
pub fn pump_for(d: Duration) {
    let end = Instant::now() + d;
    unsafe {
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if Instant::now() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Inner;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let inner = &*ptr;
        match msg {
            WM_SIZE => {
                inner.resize();
                let vis = wparam.0 != 1 && IsWindowVisible(hwnd).as_bool();
                if vis != inner.visible.replace(vis) {
                    if let Ok(c) = inner.controller.try_borrow() {
                        if let Some(c) = c.as_ref() {
                            let _ = c.SetIsVisible(vis);
                        }
                    }
                    (inner.sink)(WebEvent::Visibility(inner.kind, vis));
                }
                LRESULT(0)
            }
            0x0018 => {
                // WM_SHOWWINDOW
                let vis = wparam.0 != 0;
                if vis != inner.visible.replace(vis) {
                    if let Ok(c) = inner.controller.try_borrow() {
                        if let Some(c) = c.as_ref() {
                            let _ = c.SetIsVisible(vis);
                        }
                    }
                    (inner.sink)(WebEvent::Visibility(inner.kind, vis));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_MOVE => {
                if let Ok(c) = inner.controller.try_borrow() {
                    if let Some(c) = c.as_ref() {
                        let _ = c.NotifyParentWindowPositionChanged();
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                mmi.ptMinTrackSize.x = inner.min_w * dpi / 96;
                mmi.ptMinTrackSize.y = inner.min_h * dpi / 96;
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_CLOSE => {
                (inner.sink)(WebEvent::CloseRequested(inner.kind));
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Rc::from_raw(ptr));
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
