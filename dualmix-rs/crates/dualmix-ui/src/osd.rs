//! Glanceable click-through layered Win32 OSD (`WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_LAYERED`).
//!
//! Hidden cleanly via `ShowWindow(SW_HIDE)` after fade-out, on a safety-net timer, and on `WM_DISPLAYCHANGE` so no
//! transparent ghost frame can ever remain stuck on screen over fullscreen games or mode switches.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};

use dualmix_core::log;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetMonitorInfoW, HBITMAP, HDC, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, SelectObject,
};
use windows::Win32::Graphics::GdiPlus::{
    CombineModeReplace, GdipAddPathArc, GdipClosePathFigure, GdipCreateBitmapFromScan0, GdipCreateFont,
    GdipCreateFontFamilyFromName, GdipCreateFromHDC, GdipCreateLineBrush, GdipCreatePath, GdipCreatePen1,
    GdipCreateSolidFill, GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont, GdipDeleteFontFamily,
    GdipDeleteGraphics, GdipDeletePath, GdipDeletePen, GdipDeleteStringFormat, GdipDrawImageRectI, GdipDrawLine,
    GdipDrawPath, GdipDrawRectangle, GdipDrawString, GdipFillEllipse, GdipFillPath, GdipFillRectangle,
    GdipGetImageGraphicsContext, GdipSetSmoothingMode, GdipSetStringFormatAlign, GdipSetStringFormatLineAlign,
    GdipSetTextRenderingHint, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GpBrush, GpFont, GpFontFamily,
    GpGraphics, GpLineGradient, GpPath, GpPen, GpSolidFill, GpStringFormat, PointF, RectF, SmoothingModeAntiAlias,
    StringAlignmentCenter, StringAlignmentFar, StringAlignmentNear, TextRenderingHintClearTypeGridFit, UnitPixel,
    WrapModeTileFlipXY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, GetWindowLongPtrW, HWND_TOPMOST,
    KillTimer, RegisterClassExW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_DISPLAYCHANGE, WM_NCCREATE, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdKind {
    Mix,
    Volume,
    Game,
    Chat,
    Function,
}

#[derive(Debug, Clone)]
pub struct OsdInfo {
    pub kind: OsdKind,
    pub title: String,
    pub value_text: String,
    /// Mix: -1..1; levels: 0..1.
    pub value: f64,
    pub muted: bool,
}

const LOGICAL_W: f32 = 320.0;
const LOGICAL_H: f32 = 74.0;
const VISIBLE_MS: u32 = 1300;
const FADE_IN_MS: u32 = 90;
const FADE_OUT_MS: u32 = 300;
const FORCE_HIDE_MS: u32 = VISIBLE_MS + FADE_OUT_MS + 400;

const TIMER_HOLD: usize = 1;
const TIMER_ANIM: usize = 2;
const TIMER_FORCE_HIDE: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnimPhase {
    Idle,
    FadingIn,
    Visible,
    FadingOut,
}

struct OsdState {
    hwnd: HWND,
    gdip_token: usize,
    hdc_screen: HDC,
    hdc_mem: HDC,
    hbmp: HBITMAP,
    dib_bits: *mut u8,
    bmp_w: i32,
    bmp_h: i32,
    phase: AnimPhase,
    alpha: f32,
    anim_start: std::time::Instant,
    anim_from: f32,
    anim_to: f32,
    anim_dur_ms: u32,
    last_dark: Option<bool>,
    last_info: Option<OsdInfo>,
    visible: bool,
}

impl OsdState {
    unsafe fn ensure_surface(&mut self, w: i32, h: i32) {
        if self.bmp_w == w && self.bmp_h == h && !self.hbmp.is_invalid() {
            return;
        }
        unsafe {
            if !self.hbmp.is_invalid() {
                let _ = DeleteObject(self.hbmp);
                self.hbmp = HBITMAP::default();
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            if let Ok(hbmp) = CreateDIBSection(self.hdc_mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                SelectObject(self.hdc_mem, hbmp);
                self.hbmp = hbmp;
                self.dib_bits = bits as *mut u8;
                self.bmp_w = w;
                self.bmp_h = h;
            }
        }
    }

    unsafe fn paint_and_commit(&mut self, info: &OsdInfo, dark: bool, scale: f32, alpha_u8: u8) {
        let w = (LOGICAL_W * scale).round().max(1.0) as i32;
        let h = (LOGICAL_H * scale).round().max(1.0) as i32;
        unsafe {
            self.ensure_surface(w, h);
            if self.dib_bits.is_null() || self.bmp_w <= 0 || self.bmp_h <= 0 {
                return;
            }
            std::ptr::write_bytes(self.dib_bits, 0, (w * h * 4) as usize);

            let mut g: *mut GpGraphics = std::ptr::null_mut();
            if GdipCreateFromHDC(self.hdc_mem, &mut g) == windows::Win32::Graphics::GdiPlus::Ok && !g.is_null() {
                let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
                let _ = GdipSetTextRenderingHint(g, TextRenderingHintClearTypeGridFit);
                draw_osd(g, info, dark, scale, w as f32, h as f32);
                let _ = GdipDeleteGraphics(g);
            }

            let pt_src = POINT { x: 0, y: 0 };
            let sz = SIZE { cx: w, cy: h };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha_u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                self.hdc_screen,
                None,
                Some(&sz),
                self.hdc_mem,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    unsafe fn update_alpha_only(&mut self, alpha_u8: u8) {
        if self.bmp_w <= 0 || self.bmp_h <= 0 {
            return;
        }
        unsafe {
            let pt_src = POINT { x: 0, y: 0 };
            let sz = SIZE { cx: self.bmp_w, cy: self.bmp_h };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha_u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                self.hdc_screen,
                None,
                Some(&sz),
                self.hdc_mem,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    unsafe fn hide_now(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TIMER_HOLD);
            let _ = KillTimer(self.hwnd, TIMER_ANIM);
            let _ = KillTimer(self.hwnd, TIMER_FORCE_HIDE);
            self.phase = AnimPhase::Idle;
            self.alpha = 0.0;
            if self.visible {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                self.visible = false;
            }
        }
    }
}

pub struct OsdWindow {
    hwnd: HWND,
}

static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

impl OsdWindow {
    pub fn create() -> Option<Self> {
        unsafe {
            let hinst = HINSTANCE(GetModuleHandleW(None).ok()?.0);
            if !CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
                let wc = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(osd_wnd_proc),
                    hInstance: hinst,
                    lpszClassName: w!("DualMix.OsdWindow"),
                    ..Default::default()
                };
                RegisterClassExW(&wc);
            }

            let mut token = 0usize;
            let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
            let _ = GdiplusStartup(&mut token, &input, std::ptr::null_mut());

            let hdc_screen = windows::Win32::Graphics::Gdi::GetDC(None);
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let state = Box::new(OsdState {
                hwnd: HWND::default(),
                gdip_token: token,
                hdc_screen,
                hdc_mem,
                hbmp: HBITMAP::default(),
                dib_bits: std::ptr::null_mut(),
                bmp_w: 0,
                bmp_h: 0,
                phase: AnimPhase::Idle,
                alpha: 0.0,
                anim_start: std::time::Instant::now(),
                anim_from: 0.0,
                anim_to: 0.0,
                anim_dur_ms: 0,
                last_dark: None,
                last_info: None,
                visible: false,
            });
            let ptr = Box::into_raw(state);

            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TOPMOST,
                w!("DualMix.OsdWindow"),
                w!("DualMix OSD"),
                WS_POPUP,
                0,
                0,
                320,
                74,
                None,
                None,
                hinst,
                Some(ptr as *const c_void),
            )
            .ok()?;

            (*ptr).hwnd = hwnd;
            Some(Self { hwnd })
        }
    }

    pub fn show(&self, info: &OsdInfo, dark: bool) {
        unsafe {
            let ptr = GetWindowLongPtrW(self.hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA) as *mut OsdState;
            if ptr.is_null() {
                return;
            }
            let st = &mut *ptr;
            let (x, y, w_px, h_px, scale) = active_monitor_rect();

            let start_alpha = if st.visible { st.alpha } else { 0.0 };
            st.last_dark = Some(dark);
            st.last_info = Some(info.clone());

            st.paint_and_commit(info, dark, scale, (start_alpha * 255.0).round() as u8);
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, x, y, w_px, h_px, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            st.visible = true;

            let _ = KillTimer(self.hwnd, TIMER_HOLD);
            let _ = KillTimer(self.hwnd, TIMER_ANIM);
            let _ = KillTimer(self.hwnd, TIMER_FORCE_HIDE);

            if start_alpha < 0.99 {
                st.phase = AnimPhase::FadingIn;
                st.anim_start = std::time::Instant::now();
                st.anim_from = start_alpha;
                st.anim_to = 1.0;
                st.anim_dur_ms = FADE_IN_MS;
                SetTimer(self.hwnd, TIMER_ANIM, 16, None);
            } else {
                st.phase = AnimPhase::Visible;
                st.alpha = 1.0;
                st.update_alpha_only(255);
            }

            SetTimer(self.hwnd, TIMER_HOLD, VISIBLE_MS, None);
            SetTimer(self.hwnd, TIMER_FORCE_HIDE, FORCE_HIDE_MS, None);
        }
    }

    pub fn hide_now(&self) {
        unsafe {
            let ptr = GetWindowLongPtrW(self.hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA) as *mut OsdState;
            if !ptr.is_null() {
                (*ptr).hide_now();
            }
        }
    }
}

impl Drop for OsdWindow {
    fn drop(&mut self) {
        unsafe {
            let ptr = GetWindowLongPtrW(self.hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA) as *mut OsdState;
            let _ = DestroyWindow(self.hwnd);
            if !ptr.is_null() {
                let st = Box::from_raw(ptr);
                if !st.hbmp.is_invalid() {
                    let _ = DeleteObject(st.hbmp);
                }
                let _ = DeleteDC(st.hdc_mem);
                let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, st.hdc_screen);
                if st.gdip_token != 0 {
                    GdiplusShutdown(st.gdip_token);
                }
            }
        }
    }
}

unsafe extern "system" fn osd_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = GetWindowLongPtrW(hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA) as *mut OsdState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let st = &mut *ptr;
        match msg {
            WM_DISPLAYCHANGE => {
                log::info("Display settings changed: hiding OSD");
                st.hide_now();
                LRESULT(0)
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_HOLD => {
                        let _ = KillTimer(hwnd, TIMER_HOLD);
                        st.phase = AnimPhase::FadingOut;
                        st.anim_start = std::time::Instant::now();
                        st.anim_from = st.alpha;
                        st.anim_to = 0.0;
                        st.anim_dur_ms = FADE_OUT_MS;
                        SetTimer(hwnd, TIMER_ANIM, 16, None);
                    }
                    TIMER_ANIM => {
                        let elapsed = st.anim_start.elapsed().as_secs_f32() * 1000.0;
                        let dur = st.anim_dur_ms.max(1) as f32;
                        let t = (elapsed / dur).clamp(0.0, 1.0);
                        st.alpha = st.anim_from + (st.anim_to - st.anim_from) * t;
                        st.update_alpha_only((st.alpha * 255.0).round() as u8);
                        if t >= 1.0 {
                            let _ = KillTimer(hwnd, TIMER_ANIM);
                            if st.phase == AnimPhase::FadingOut {
                                st.hide_now();
                            } else {
                                st.phase = AnimPhase::Visible;
                            }
                        }
                    }
                    TIMER_FORCE_HIDE => {
                        st.hide_now();
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn active_monitor_rect() -> (i32, i32, i32, i32, f32) {
    unsafe {
        let fg = GetForegroundWindow();
        let mon = windows::Win32::Graphics::Gdi::MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut scale = 1.0f32;
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() && dpi_x > 0 {
                scale = dpi_x as f32 / 96.0;
            }
            let area: RECT = mi.rcWork;
            let w_px = (LOGICAL_W * scale).round() as i32;
            let h_px = (LOGICAL_H * scale).round() as i32;
            let x = area.left + (area.right - area.left - w_px) / 2;
            let y = area.bottom - h_px - (56.0 * scale).round() as i32;
            return (x, y, w_px, h_px, scale);
        }
        (100, 100, 320, 74, 1.0)
    }
}

#[inline]
const fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

unsafe fn add_round_rect(path: *mut GpPath, x: f32, y: f32, w: f32, h: f32, r: f32) {
    unsafe {
        let d = (r * 2.0).min(w).min(h);
        if d <= 0.5 {
            let _ = GdipAddPathArc(path, x, y, 1.0, 1.0, 180.0, 90.0);
            return;
        }
        let _ = GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
        let _ = GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
        let _ = GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
        let _ = GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
        let _ = GdipClosePathFigure(path);
    }
}

unsafe fn fill_round_rect(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, r: f32, color: u32) {
    unsafe {
        let mut path: *mut GpPath = std::ptr::null_mut();
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        if GdipCreatePath(windows::Win32::Graphics::GdiPlus::FillModeAlternate, &mut path) == windows::Win32::Graphics::GdiPlus::Ok {
            add_round_rect(path, x, y, w, h, r);
            if GdipCreateSolidFill(color, &mut brush) == windows::Win32::Graphics::GdiPlus::Ok {
                let _ = GdipFillPath(g, brush as *mut GpBrush, path);
                let _ = GdipDeleteBrush(brush as *mut GpBrush);
            }
            let _ = GdipDeletePath(path);
        }
    }
}

unsafe fn stroke_round_rect(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, r: f32, color: u32, width: f32) {
    unsafe {
        let mut path: *mut GpPath = std::ptr::null_mut();
        let mut pen: *mut GpPen = std::ptr::null_mut();
        if GdipCreatePath(windows::Win32::Graphics::GdiPlus::FillModeAlternate, &mut path) == windows::Win32::Graphics::GdiPlus::Ok {
            add_round_rect(path, x, y, w, h, r);
            if GdipCreatePen1(color, width, UnitPixel, &mut pen) == windows::Win32::Graphics::GdiPlus::Ok {
                let _ = GdipDrawPath(g, pen, path);
                let _ = GdipDeletePen(pen);
            }
            let _ = GdipDeletePath(path);
        }
    }
}

unsafe fn draw_text(g: *mut GpGraphics, text: &str, x: f32, y: f32, w: f32, h: f32, size: f32, bold: bool, color: u32, right: bool) {
    unsafe {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut family: *mut GpFontFamily = std::ptr::null_mut();
        if GdipCreateFontFamilyFromName(w!("Segoe UI Variable Text"), std::ptr::null_mut(), &mut family) != windows::Win32::Graphics::GdiPlus::Ok {
            let _ = GdipCreateFontFamilyFromName(w!("Segoe UI"), std::ptr::null_mut(), &mut family);
        }
        if family.is_null() {
            return;
        }
        let mut font: *mut GpFont = std::ptr::null_mut();
        let style = if bold { 1 } else { 0 }; // FontStyleBold = 1
        let _ = GdipCreateFont(family, size, style, UnitPixel, &mut font);
        let mut fmt: *mut GpStringFormat = std::ptr::null_mut();
        let _ = GdipCreateStringFormat(0, 0, &mut fmt);
        if !fmt.is_null() {
            let _ = GdipSetStringFormatAlign(fmt, if right { StringAlignmentFar } else { StringAlignmentNear });
            let _ = GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);
        }
        let mut brush: *mut GpSolidFill = std::ptr::null_mut();
        let _ = GdipCreateSolidFill(color, &mut brush);
        if !font.is_null() && !brush.is_null() {
            let rect = RectF { X: x, Y: y, Width: w, Height: h };
            let _ = GdipDrawString(g, PCWSTR(wide.as_ptr()), -1, font, &rect, fmt, brush as *mut GpBrush);
        }
        if !brush.is_null() {
            let _ = GdipDeleteBrush(brush as *mut GpBrush);
        }
        if !fmt.is_null() {
            let _ = GdipDeleteStringFormat(fmt);
        }
        if !font.is_null() {
            let _ = GdipDeleteFont(font);
        }
        let _ = GdipDeleteFontFamily(family);
    }
}

unsafe fn draw_icon(g: *mut GpGraphics, kind: OsdKind, muted: bool, cx: f32, cy: f32, s: f32, color: u32) {
    unsafe {
        let mut pen: *mut GpPen = std::ptr::null_mut();
        if GdipCreatePen1(color, 1.8 * s, UnitPixel, &mut pen) != windows::Win32::Graphics::GdiPlus::Ok {
            return;
        }
        match kind {
            OsdKind::Mix => {
                // Three horizontal slider lines with knobs
                for (i, kx) in [-3.0f32, 3.0, -1.0].iter().enumerate() {
                    let yy = cy + (i as f32 - 1.0) * 5.0 * s;
                    let _ = GdipDrawLine(g, pen, cx - 8.0 * s, yy, cx + 8.0 * s, yy);
                    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
                    if GdipCreateSolidFill(color, &mut brush) == windows::Win32::Graphics::GdiPlus::Ok {
                        let _ = GdipFillEllipse(g, brush as *mut GpBrush, cx + *kx * s - 2.2 * s, yy - 2.2 * s, 4.4 * s, 4.4 * s);
                        let _ = GdipDeleteBrush(brush as *mut GpBrush);
                    }
                }
            }
            OsdKind::Volume | OsdKind::Game | OsdKind::Chat if muted => {
                // Speaker + X
                let _ = GdipDrawRectangle(g, pen, cx - 8.0 * s, cy - 3.0 * s, 4.0 * s, 6.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 4.0 * s, cy - 3.0 * s, cx + 1.0 * s, cy - 7.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 1.0 * s, cy - 7.0 * s, cx + 1.0 * s, cy + 7.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 1.0 * s, cy + 7.0 * s, cx - 4.0 * s, cy + 3.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 4.0 * s, cy - 3.5 * s, cx + 9.0 * s, cy + 3.5 * s);
                let _ = GdipDrawLine(g, pen, cx + 9.0 * s, cy - 3.5 * s, cx + 4.0 * s, cy + 3.5 * s);
            }
            OsdKind::Volume => {
                let _ = GdipDrawRectangle(g, pen, cx - 8.0 * s, cy - 3.0 * s, 4.0 * s, 6.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 4.0 * s, cy - 3.0 * s, cx + 1.0 * s, cy - 7.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 1.0 * s, cy - 7.0 * s, cx + 1.0 * s, cy + 7.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 1.0 * s, cy + 7.0 * s, cx - 4.0 * s, cy + 3.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 4.5 * s, cy - 3.0 * s, cx + 5.5 * s, cy + 3.0 * s);
                let _ = GdipDrawLine(g, pen, cx + 7.5 * s, cy - 5.5 * s, cx + 8.5 * s, cy + 5.5 * s);
            }
            OsdKind::Game => {
                let _ = GdipDrawRectangle(g, pen, cx - 8.5 * s, cy - 5.0 * s, 17.0 * s, 10.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 4.5 * s, cy - 2.0 * s, cx - 4.5 * s, cy + 2.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 6.5 * s, cy, cx - 2.5 * s, cy);
                let _ = GdipDrawLine(g, pen, cx + 3.5 * s, cy - 1.5 * s, cx + 3.5 * s, cy - 1.4 * s);
                let _ = GdipDrawLine(g, pen, cx + 5.5 * s, cy + 1.5 * s, cx + 5.5 * s, cy + 1.6 * s);
            }
            OsdKind::Chat => {
                let _ = GdipDrawRectangle(g, pen, cx - 8.0 * s, cy - 6.0 * s, 16.0 * s, 10.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 3.0 * s, cy + 4.0 * s, cx - 6.0 * s, cy + 7.5 * s);
                let _ = GdipDrawLine(g, pen, cx - 6.0 * s, cy + 7.5 * s, cx + 1.0 * s, cy + 4.0 * s);
            }
            OsdKind::Function => {
                let _ = GdipDrawRectangle(g, pen, cx - 9.0 * s, cy - 5.0 * s, 18.0 * s, 10.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 5.0 * s, cy + 2.0 * s, cx + 5.0 * s, cy + 2.0 * s);
                let _ = GdipDrawLine(g, pen, cx - 5.0 * s, cy - 1.5 * s, cx - 2.0 * s, cy - 1.5 * s);
                let _ = GdipDrawLine(g, pen, cx + 2.0 * s, cy - 1.5 * s, cx + 5.0 * s, cy - 1.5 * s);
            }
        }
        let _ = GdipDeletePen(pen);
    }
}

unsafe fn draw_osd(g: *mut GpGraphics, info: &OsdInfo, dark: bool, s: f32, w: f32, h: f32) {
    unsafe {
        let pad = 1.0 * s;
        let bg = if dark { argb(0xF0, 0x2C, 0x2C, 0x2C) } else { argb(0xF5, 0xFA, 0xFA, 0xFA) };
        let border = if dark { argb(0x38, 0xFF, 0xFF, 0xFF) } else { argb(0x22, 0x00, 0x00, 0x00) };
        let primary = if dark { argb(0xFF, 0xFF, 0xFF, 0xFF) } else { argb(0xFF, 0x1A, 0x1A, 0x1A) };
        let secondary = if dark { argb(0xC8, 0xFF, 0xFF, 0xFF) } else { argb(0x9E, 0x00, 0x00, 0x00) };
        let game_col = if dark { argb(0xFF, 0xFF, 0x9A, 0x52) } else { argb(0xFF, 0xC2, 0x4E, 0x00) };
        let chat_col = if dark { argb(0xFF, 0x5C, 0xC8, 0xFF) } else { argb(0xFF, 0x00, 0x67, 0xC0) };
        let accent = if dark { argb(0xFF, 0x60, 0xCD, 0xFF) } else { argb(0xFF, 0x00, 0x5F, 0xB8) };
        let track_col = if dark { argb(0x28, 0xFF, 0xFF, 0xFF) } else { argb(0x24, 0x00, 0x00, 0x00) };

        fill_round_rect(g, pad, pad, w - 2.0 * pad, h - 2.0 * pad, 10.0 * s, bg);
        stroke_round_rect(g, pad, pad, w - 2.0 * pad, h - 2.0 * pad, 10.0 * s, border, 1.0 * s);

        let has_bar = info.kind != OsdKind::Function;
        let icon_cx = 28.0 * s;
        let icon_cy = if has_bar { 25.0 * s } else { h * 0.5 };
        draw_icon(g, info.kind, info.muted, icon_cx, icon_cy, s, primary);

        let tx = 48.0 * s;
        let tw = w - tx - 18.0 * s;
        if has_bar {
            draw_text(g, &info.title, tx, 14.0 * s, tw * 0.45, 22.0 * s, 12.5 * s, false, secondary, false);
            draw_text(g, &info.value_text, tx + tw * 0.35, 13.0 * s, tw * 0.65, 24.0 * s, 14.0 * s, true, primary, true);

            let bx = tx;
            let bw = tw;
            let bh = 5.0 * s;
            let by = 48.0 * s;
            if info.kind == OsdKind::Mix {
                // Game -> Chat gradient track
                let mut path: *mut GpPath = std::ptr::null_mut();
                if GdipCreatePath(windows::Win32::Graphics::GdiPlus::FillModeAlternate, &mut path) == windows::Win32::Graphics::GdiPlus::Ok {
                    add_round_rect(path, bx, by, bw, bh, bh * 0.5);
                    let mut lg: *mut GpLineGradient = std::ptr::null_mut();
                    let p1 = PointF { X: bx, Y: by };
                    let p2 = PointF { X: bx + bw, Y: by };
                    if GdipCreateLineBrush(&p1, &p2, game_col, chat_col, WrapModeTileFlipXY, &mut lg) == windows::Win32::Graphics::GdiPlus::Ok {
                        let _ = GdipFillPath(g, lg as *mut GpBrush, path);
                        let _ = GdipDeleteBrush(lg as *mut GpBrush);
                    }
                    let _ = GdipDeletePath(path);
                }
                // Center tick
                let mut brush: *mut GpSolidFill = std::ptr::null_mut();
                if GdipCreateSolidFill(secondary, &mut brush) == windows::Win32::Graphics::GdiPlus::Ok {
                    let _ = GdipFillRectangle(g, brush as *mut GpBrush, bx + bw * 0.5 - 1.0 * s, by - 3.0 * s, 2.0 * s, bh + 6.0 * s);
                    let _ = GdipDeleteBrush(brush as *mut GpBrush);
                }
                // Thumb
                let thumb_d = 12.0 * s;
                let frac = ((info.value.clamp(-1.0, 1.0) + 1.0) * 0.5) as f32;
                let tx_pos = bx + frac * (bw - thumb_d);
                let ty_pos = by + bh * 0.5 - thumb_d * 0.5;
                let stroke_c = if dark { argb(0xFF, 0x2C, 0x2C, 0x2C) } else { argb(0xFF, 0xFF, 0xFF, 0xFF) };
                let mut b_fill: *mut GpSolidFill = std::ptr::null_mut();
                if GdipCreateSolidFill(primary, &mut b_fill) == windows::Win32::Graphics::GdiPlus::Ok {
                    let _ = GdipFillEllipse(g, b_fill as *mut GpBrush, tx_pos, ty_pos, thumb_d, thumb_d);
                    let _ = GdipDeleteBrush(b_fill as *mut GpBrush);
                }
                let mut pen_t: *mut GpPen = std::ptr::null_mut();
                if GdipCreatePen1(stroke_c, 1.5 * s, UnitPixel, &mut pen_t) == windows::Win32::Graphics::GdiPlus::Ok {
                    let _ = windows::Win32::Graphics::GdiPlus::GdipDrawEllipse(g, pen_t, tx_pos, ty_pos, thumb_d, thumb_d);
                    let _ = GdipDeletePen(pen_t);
                }
            } else {
                fill_round_rect(g, bx, by, bw, bh, bh * 0.5, track_col);
                let mut fill_col = match info.kind {
                    OsdKind::Game => game_col,
                    OsdKind::Chat => chat_col,
                    _ => accent,
                };
                if info.muted {
                    fill_col = (fill_col & 0x00FF_FFFF) | 0x5900_0000;
                }
                let fw = (info.value.clamp(0.0, 1.0) as f32 * bw).max(0.0);
                if fw >= 2.0 * s {
                    fill_round_rect(g, bx, by, fw, bh, bh * 0.5, fill_col);
                }
            }
        } else {
            draw_text(g, &info.title, tx, 14.0 * s, tw, 20.0 * s, 12.5 * s, false, secondary, false);
            draw_text(g, &info.value_text, tx, 34.0 * s, tw, 24.0 * s, 15.0 * s, true, primary, false);
        }

        // Unused GDI+ symbols avoided; keep silence if any helper unused
        let _ = (CombineModeReplace, GdipCreateBitmapFromScan0, GdipDrawImageRectI, GdipGetImageGraphicsContext);
    }
}
