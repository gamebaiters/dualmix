//! Resolves process ids to names / executable paths / friendly names (cached), port of C# `ProcessInfo`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::core::{HSTRING, PCWSTR, PWSTR};

static BY_PID: Mutex<Option<HashMap<u32, (String, Option<String>)>>> = Mutex::new(None);
static FRIENDLY: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn file_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => file[..i].to_string(),
        _ => file.to_string(),
    }
}

/// (name without .exe, full path when readable)
pub fn get(pid: u32) -> (String, Option<String>) {
    if let Some(v) = BY_PID.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&pid).cloned())) {
        return v;
    }
    let path = query_path(pid);
    let name = match &path {
        Some(p) => file_stem(p),
        None => snapshot_name(pid).map(|n| file_stem(&n)).unwrap_or_else(|| format!("pid{pid}")),
    };
    let r = (name, path);
    if let Ok(mut g) = BY_PID.lock() {
        g.get_or_insert_with(HashMap::new).insert(pid, r.clone());
    }
    r
}

/// Drops cache entries of processes that no longer own audio sessions.
pub fn prune(alive: &HashSet<u32>) {
    if let Ok(mut g) = BY_PID.lock() {
        if let Some(m) = g.as_mut() {
            m.retain(|k, _| alive.contains(k));
        }
    }
}

fn prettify(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "?".into(),
    }
}

pub fn friendly_name(path: Option<&str>, process_name: &str) -> String {
    let Some(path) = path.filter(|p| !p.is_empty()) else { return prettify(process_name) };
    let key = path.to_lowercase();
    if let Some(v) = FRIENDLY.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).cloned())) {
        return v;
    }
    let mut result = prettify(process_name);
    if let Some((desc, prod)) = version_strings(path) {
        let d = desc.trim();
        let p = prod.trim();
        if !d.is_empty() && d.chars().count() <= 48 && !d.to_lowercase().ends_with(".exe") {
            result = d.to_string();
        } else if !p.is_empty() && p.chars().count() <= 48 {
            result = p.to_string();
        }
    }
    if let Ok(mut g) = FRIENDLY.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, result.clone());
    }
    result
}

fn version_strings(path: &str) -> Option<(String, String)> {
    unsafe {
        let h = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(PCWSTR(h.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(h.as_ptr()), 0, size, buf.as_mut_ptr() as *mut _).ok()?;
        // First translation (language + code page).
        let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let tr = HSTRING::from(r"\VarFileInfo\Translation");
        let (lang, cp) = if VerQueryValueW(buf.as_ptr() as *const _, PCWSTR(tr.as_ptr()), &mut p, &mut len).as_bool() && len >= 4 {
            let w = p as *const u16;
            (*w, *w.add(1))
        } else {
            (0x0409, 0x04B0)
        };
        let read = |field: &str| -> String {
            let q = HSTRING::from(format!(r"\StringFileInfo\{lang:04x}{cp:04x}\{field}"));
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut len = 0u32;
            if VerQueryValueW(buf.as_ptr() as *const _, PCWSTR(q.as_ptr()), &mut p, &mut len).as_bool() && len > 0 && !p.is_null() {
                let s = std::slice::from_raw_parts(p as *const u16, len as usize);
                let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                String::from_utf16_lossy(&s[..end])
            } else {
                String::new()
            }
        };
        Some((read("FileDescription"), read("ProductName")))
    }
}

fn query_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).is_ok();
        let _ = CloseHandle(h);
        if ok { Some(String::from_utf16_lossy(&buf[..size as usize])) } else { None }
    }
}

fn each_process(mut f: impl FnMut(u32, &str)) {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(MAX_PATH as usize);
                let name = String::from_utf16_lossy(&e.szExeFile[..end]);
                f(e.th32ProcessID, &name);
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

fn snapshot_name(pid: u32) -> Option<String> {
    let mut found = None;
    each_process(|p, n| {
        if p == pid {
            found = Some(n.to_string());
        }
    });
    found
}

/// Process ids of every running process with this name (without ".exe", case-insensitive).
pub fn pids_by_name(process_name: &str) -> Vec<u32> {
    let want = process_name.to_lowercase();
    let mut v = Vec::new();
    each_process(|p, n| {
        if file_stem(n).to_lowercase() == want {
            v.push(p);
        }
    });
    v
}
