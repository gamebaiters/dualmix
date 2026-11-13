//! Native Win32 notification-area icon (`Shell_NotifyIconW`) with dark/light context menu and balloon tips.

use std::mem::size_of;

use dualmix_core::loc;
use dualmix_core::settings::KnobTarget;
use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_NONE, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, HICON,
    IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED, LoadImageW, LookupIconIdFromDirectoryEx, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, SetForegroundWindow, SetMenuDefaultItem, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TrackPopupMenuEx,
};
use windows::core::PCWSTR;

pub const WM_TRAY_ICON: u32 = 0x8010; // WM_APP + 16
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_CONTEXTMENU: u32 = 0x007B;

const ICON_BYTES: &[u8] = include_bytes!("../assets/dualmix.ico");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    AllGame,
    Balanced,
    AllChat,
    SetTarget(KnobTarget),
    ToggleAutostart,
    Exit,
}

#[derive(Debug, Clone)]
pub struct TrayStatus {
    pub tooltip: String,
    pub mix_label: String,
    pub target: KnobTarget,
    pub autostart: bool,
    pub dark: bool,
}

pub struct TrayIcon {
    hwnd: HWND,
    icon: HICON,
    added: bool,
}

impl TrayIcon {
    pub fn new(hwnd: HWND) -> Self {
        let icon = load_tray_icon();
        let mut s = Self { hwnd, icon, added: false };
        s.add("DualMix");
        s
    }

    fn add(&mut self, tip: &str) {
        let mut nid = self.base_nid();
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_ICON;
        nid.hIcon = self.icon;
        write_wstr(&mut nid.szTip, tip);
        unsafe {
            self.added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
        }
    }

    pub fn update_tooltip(&self, tip: &str) {
        if !self.added {
            return;
        }
        let mut nid = self.base_nid();
        nid.uFlags = NIF_TIP;
        write_wstr(&mut nid.szTip, tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn show_balloon(&self, title: &str, body: &str) {
        if !self.added {
            return;
        }
        let mut nid = self.base_nid();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_NONE;
        write_wstr(&mut nid.szInfoTitle, title);
        write_wstr(&mut nid.szInfo, body);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn handle_message(&self, lparam: LPARAM, status: &TrayStatus) -> Option<TrayAction> {
        let msg = (lparam.0 & 0xFFFF) as u32;
        match msg {
            WM_LBUTTONUP => Some(TrayAction::Open),
            WM_RBUTTONUP | WM_CONTEXTMENU => self.show_menu(status),
            _ => None,
        }
    }

    fn show_menu(&self, status: &TrayStatus) -> Option<TrayAction> {
        apply_win11_menu_theme(status.dark);
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let sub = CreatePopupMenu().ok()?;

            let open_w = to_wide(&loc::get("Tray.Open"));
            let mix_w = to_wide(&status.mix_label);
            let game_w = to_wide(&loc::get("Tray.AllGame"));
            let bal_w = to_wide(&loc::get("Tray.Balanced"));
            let chat_w = to_wide(&loc::get("Tray.AllChat"));
            let knob_w = to_wide(&loc::get("Tray.KnobSubMenu"));
            let auto_w = to_wide(&loc::get("Tray.Autostart"));
            let exit_w = to_wide(&loc::get("Tray.Exit"));

            let _ = AppendMenuW(menu, MF_STRING, 1, PCWSTR(open_w.as_ptr()));
            let _ = SetMenuDefaultItem(menu, 1, 0);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 2, PCWSTR(mix_w.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, 3, PCWSTR(game_w.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, 4, PCWSTR(bal_w.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, 5, PCWSTR(chat_w.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

            let targets = [
                (10usize, KnobTarget::Mix, loc::get("Target.Mix")),
                (11, KnobTarget::Volume, loc::get("Target.Volume")),
                (12, KnobTarget::Game, loc::get("Target.Game")),
                (13, KnobTarget::Chat, loc::get("Target.Chat")),
            ];
            let mut sub_bufs = Vec::with_capacity(4);
            for (id, t, label) in targets {
                let w = to_wide(&label);
                let flag = if status.target == t { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED };
                let _ = AppendMenuW(sub, flag, id, PCWSTR(w.as_ptr()));
                sub_bufs.push(w);
            }
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(knob_w.as_ptr()));

            let auto_flag = if status.autostart { MF_STRING | MF_CHECKED } else { MF_STRING | MF_UNCHECKED };
            let _ = AppendMenuW(menu, auto_flag, 20, PCWSTR(auto_w.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, 99, PCWSTR(exit_w.as_ptr()));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenuEx(
                menu,
                (TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY).0,
                pt.x,
                pt.y,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(sub);
            let _ = DestroyMenu(menu);

            match cmd.0 {
                1 => Some(TrayAction::Open),
                3 => Some(TrayAction::AllGame),
                4 => Some(TrayAction::Balanced),
                5 => Some(TrayAction::AllChat),
                10 => Some(TrayAction::SetTarget(KnobTarget::Mix)),
                11 => Some(TrayAction::SetTarget(KnobTarget::Volume)),
                12 => Some(TrayAction::SetTarget(KnobTarget::Game)),
                13 => Some(TrayAction::SetTarget(KnobTarget::Chat)),
                20 => Some(TrayAction::ToggleAutostart),
                99 => Some(TrayAction::Exit),
                _ => None,
            }
        }
    }

    fn base_nid(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if self.added {
            let nid = self.base_nid();
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.added = false;
        }
        if !self.icon.is_invalid() {
            unsafe {
                let _ = DestroyIcon(self.icon);
            }
        }
    }
}

fn load_tray_icon() -> HICON {
    unsafe {
        if let Ok(hmod) = GetModuleHandleW(None) {
            if let Ok(h) = LoadImageW(hmod, PCWSTR(1usize as *const u16), IMAGE_ICON, 16, 16, LR_DEFAULTCOLOR | LR_SHARED) {
                if !h.is_invalid() {
                    return HICON(h.0);
                }
            }
        }
        // Fallback: parse embedded ICO directory
        let offset = LookupIconIdFromDirectoryEx(ICON_BYTES.as_ptr(), true, 16, 16, LR_DEFAULTCOLOR);
        if offset > 0 && (offset as usize) < ICON_BYTES.len() {
            if let Ok(h) = CreateIconFromResourceEx(&ICON_BYTES[offset as usize..], true, 0x0003_0000, 16, 16, LR_DEFAULTCOLOR) {
                if !h.is_invalid() {
                    return h;
                }
            }
        }
        HICON::default()
    }
}

fn apply_win11_menu_theme(dark: bool) {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::PCSTR;
    unsafe {
        if let Ok(ux) = LoadLibraryW(windows::core::w!("uxtheme.dll")) {
            // Ordinal 135: SetPreferredAppMode(AllowDark = 2, ForceLight = 3, ForceDark = 1)
            if let Some(p) = GetProcAddress(ux, PCSTR(135usize as *const u8)) {
                let f: extern "system" fn(i32) -> i32 = std::mem::transmute(p);
                f(if dark { 1 } else { 3 });
            }
            // Ordinal 136: FlushMenuThemes()
            if let Some(p) = GetProcAddress(ux, PCSTR(136usize as *const u8)) {
                let f: extern "system" fn() = std::mem::transmute(p);
                f();
            }
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn write_wstr(dst: &mut [u16], s: &str) {
    let mut n = 0usize;
    for u in s.encode_utf16() {
        if n + 1 >= dst.len() {
            break;
        }
        dst[n] = u;
        n += 1;
    }
    if !dst.is_empty() {
        dst[n.min(dst.len() - 1)] = 0;
    }
}
