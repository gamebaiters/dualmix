//! Win32 file-open dialogs, shell launch helpers, and system theme detection.

use std::path::PathBuf;

use dualmix_core::settings::ThemePreference;
use windows::Win32::Foundation::HWND;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};
use windows::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMSBT_MAINWINDOW, DwmSetWindowAttribute,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OPENFILENAMEW};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn is_system_light() -> bool {
    let mut val = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut val as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && val != 0
}

pub fn is_dark_theme(pref: ThemePreference) -> bool {
    match pref {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::System => !is_system_light(),
    }
}

pub fn apply_window_mica_and_theme(hwnd: HWND, dark: bool) {
    unsafe {
        let dark_val: i32 = if dark { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_val as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        );
        let backdrop: DWM_SYSTEMBACKDROP_TYPE = DWMSBT_MAINWINDOW;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const _,
            std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        );
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const _,
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

/// Opens a Win32 file picker. `filter` uses `|` separators like C# (`"Programs (*.exe)|*.exe"`).
pub fn open_file_dialog(owner: HWND, title: &str, filter: &str) -> Option<PathBuf> {
    let mut buf = vec![0u16; 4096];
    let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let mut filter_w: Vec<u16> = Vec::with_capacity(filter.len() + 2);
    for ch in filter.chars() {
        filter_w.push(if ch == '|' { 0 } else { ch as u16 });
    }
    filter_w.push(0);
    filter_w.push(0);

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter_w.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title_w.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    unsafe {
        if GetOpenFileNameW(&mut ofn).as_bool() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let s = String::from_utf16_lossy(&buf[..end]);
            if !s.is_empty() {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

pub fn shell_open(target: &str, args: Option<&str>) {
    let t = HSTRING::from(target);
    let a = args.map(HSTRING::from);
    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            w!("open"),
            &t,
            a.as_ref().map(|x| PCWSTR(x.as_ptr())).unwrap_or(PCWSTR::null()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
