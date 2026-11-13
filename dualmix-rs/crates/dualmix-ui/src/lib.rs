//! Native Win32/GDI+ OSD, notification tray icon, Fluent 2 WebView2 UI, and IPC helpers.

#[cfg(windows)]
pub mod dialogs;
#[cfg(windows)]
pub mod osd;
pub mod state;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod webview;

pub const INSTANCE_MUTEX: &str = "Local\\DualMix.SingleInstance.7f3c2a";
pub const SHOW_EVENT: &str = "Local\\DualMix.Show.7f3c2a";
pub const EXIT_EVENT: &str = "Local\\DualMix.Exit.7f3c2a";

#[cfg(windows)]
pub fn signal_exit_existing() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    use windows::core::HSTRING;

    let name = HSTRING::from(EXIT_EVENT);
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, &name) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
pub fn signal_exit_existing() -> bool {
    false
}

#[cfg(windows)]
pub fn signal_show_existing() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    use windows::core::HSTRING;

    let name = HSTRING::from(SHOW_EVENT);
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, &name) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
pub fn signal_show_existing() -> bool {
    false
}

#[cfg(windows)]
pub fn is_app_running() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};
    use windows::core::HSTRING;

    let name = HSTRING::from(INSTANCE_MUTEX);
    unsafe {
        if let Ok(h) = OpenMutexW(SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000), false, &name) {
            let _ = CloseHandle(h);
            return true;
        }
    }
    false
}

#[cfg(not(windows))]
pub fn is_app_running() -> bool {
    false
}
