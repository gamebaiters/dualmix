//! Windows integration helpers: Task Scheduler elevated autostart and Windows Communications ducking preference.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use dualmix_core::log;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RRF_RT_REG_DWORD, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows::core::w;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TASK_NAME: &str = "DualMix";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const DUCKING_KEY: &str = r"Software\Microsoft\Multimedia\Audio";
const DUCKING_VAL: &str = "UserDuckingPreference";

#[allow(dead_code)]
pub fn autostart_is_enabled() -> bool {
    // Check Scheduled Task first, then legacy Run key
    if run_schtasks(&["/Query", "/TN", TASK_NAME]).is_ok() {
        return true;
    }
    false
}

pub fn autostart_set(enabled: bool) {
    remove_legacy_run_key();
    if enabled {
        let Ok(exe) = std::env::current_exe() else { return };
        let Some(user) = current_user_id() else { return };
        let xml = build_task_xml(&user, &exe);
        let tmp = std::env::temp_dir().join(format!("dualmix-task-{}.xml", std::process::id()));
        if std::fs::write(&tmp, xml.as_bytes()).is_err() {
            return;
        }
        let xml_path = tmp.to_string_lossy().to_string();
        let res = run_schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &xml_path, "/F"]);
        let _ = std::fs::remove_file(&tmp);
        if let Err(e) = res {
            log::warn(format!("Failed to register autostart task: {e}"));
        } else {
            log::info("Autostart scheduled task registered");
        }
    } else {
        let _ = run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
    }
}

fn current_user_id() -> Option<String> {
    let domain = std::env::var("USERDOMAIN").ok();
    let user = std::env::var("USERNAME").ok()?;
    Some(match domain {
        Some(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    })
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn build_task_xml(user: &str, exe: &Path) -> String {
    let exe_esc = escape_xml(&exe.to_string_lossy());
    let work_esc = escape_xml(&exe.parent().unwrap_or(Path::new(".")).to_string_lossy());
    let user_esc = escape_xml(user);
    // Use --supervise --tray so the audio engine starts minimized and self-restarts on unexpected crash.
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Principals>
    <Principal id="Author">
      <UserId>{user_esc}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user_esc}</UserId>
    </LogonTrigger>
  </Triggers>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_esc}</Command>
      <Arguments>--supervise --tray</Arguments>
      <WorkingDirectory>{work_esc}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#
    )
}

fn run_schtasks(args: &[&str]) -> Result<(), String> {
    let out = Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        Err(format!("{}{}", stdout.trim(), stderr.trim()))
    }
}

fn remove_legacy_run_key() {
    unsafe {
        let mut hkey = Default::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .is_ok()
        {
            let _ = RegDeleteValueW(hkey, w!("DualMix"));
            let _ = RegCloseKey(hkey);
        }
    }
    let _ = RUN_KEY;
}

/// Reads Windows "Communications -> Do nothing" ducking setting (`3` = Do nothing).
pub fn is_ducking_disabled() -> bool {
    let mut val = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Multimedia\Audio"),
            w!("UserDuckingPreference"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut val as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    ok.is_ok() && val == 3
}

/// Sets Windows Communications ducking preference (`3` = Do nothing, `0` = Mute all other sounds / default 80% = `1`).
pub fn set_ducking_disabled(disabled: bool) {
    let val: u32 = if disabled { 3 } else { 1 };
    let mut hkey = Default::default();
    let mut disp = Default::default();
    unsafe {
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Multimedia\Audio"),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            Some(&mut disp),
        )
        .is_ok()
        {
            let bytes = val.to_le_bytes();
            let _ = RegSetValueExW(hkey, w!("UserDuckingPreference"), 0, REG_DWORD, Some(&bytes));
            let _ = RegCloseKey(hkey);
        }
    }
    let _ = (DUCKING_KEY, DUCKING_VAL);
}
