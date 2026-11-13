//! Per-application output device ("App volume and device preferences" in Windows settings) through the internal
//! `Windows.Media.Internal.AudioPolicyConfig` factory (same API used by EarTrumpet), called by raw vtable slot
//! (25 = SetPersistedDefaultAudioEndpoint, 26 = GetPersistedDefaultAudioEndpoint).

use std::ffi::c_void;
use std::sync::Mutex;

use dualmix_core::log;
use windows::core::GUID;

const RENDER_INTERFACE: &str = "{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const DEVICE_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const FACTORY_IIDS: [GUID; 2] = [
    GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324),
    GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258),
];
const SLOT_SET_PERSISTED: usize = 25;
const SLOT_GET_PERSISTED: usize = 26;
const E_INVALIDARG: i32 = 0x80070057u32 as i32;

#[link(name = "combase", kind = "raw-dylib")]
unsafe extern "system" {
    fn RoInitialize(init_type: i32) -> i32;
    fn RoGetActivationFactory(class_id: *mut c_void, iid: *const GUID, factory: *mut *mut c_void) -> i32;
    fn WindowsCreateString(source: *const u16, length: u32, string: *mut *mut c_void) -> i32;
    fn WindowsDeleteString(string: *mut c_void) -> i32;
    fn WindowsGetStringRawBuffer(string: *mut c_void, length: *mut u32) -> *const u16;
}

struct Factory {
    ptr: usize,
    initialized: bool,
}

static FACTORY: Mutex<Factory> = Mutex::new(Factory { ptr: 0, initialized: false });

struct HStr(*mut c_void);

impl HStr {
    fn new(s: &str) -> Option<Self> {
        let w: Vec<u16> = s.encode_utf16().collect();
        let mut h = std::ptr::null_mut();
        let hr = unsafe { WindowsCreateString(w.as_ptr(), w.len() as u32, &mut h) };
        if hr == 0 { Some(Self(h)) } else { None }
    }
}

impl Drop for HStr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WindowsDeleteString(self.0) };
        }
    }
}

fn factory() -> usize {
    let mut f = FACTORY.lock().unwrap_or_else(|e| e.into_inner());
    if f.initialized {
        return f.ptr;
    }
    f.initialized = true;
    unsafe {
        let _ = RoInitialize(1); // multithreaded; S_FALSE / RPC_E_CHANGED_MODE are fine
        let Some(class) = HStr::new("Windows.Media.Internal.AudioPolicyConfig") else { return 0 };
        for iid in &FACTORY_IIDS {
            let mut p = std::ptr::null_mut();
            if RoGetActivationFactory(class.0, iid, &mut p) == 0 && !p.is_null() {
                f.ptr = p as usize;
                log::info(format!("AudioPolicyConfig factory available ({iid:?})"));
                return f.ptr;
            }
        }
    }
    log::warn("AudioPolicyConfig factory not available: per-app routing disabled");
    0
}

pub fn is_available() -> bool {
    factory() != 0
}

type SetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> i32;
type GetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut *mut c_void) -> i32;

unsafe fn slot(this: *mut c_void, index: usize) -> *const c_void {
    unsafe {
        let vtbl = *(this as *const *const *const c_void);
        *vtbl.add(index)
    }
}

/// Routes a process to a playback device. `device_id` None resets it to the Windows default.
pub fn set_app_device(pid: u32, device_id: Option<&str>) -> bool {
    let f = factory();
    if f == 0 {
        return false;
    }
    let this = f as *mut c_void;
    let h = match device_id.filter(|s| !s.is_empty()) {
        Some(id) => match HStr::new(&format!("{DEVICE_PREFIX}{id}#{RENDER_INTERFACE}")) {
            Some(h) => h,
            None => return false,
        },
        None => HStr(std::ptr::null_mut()),
    };
    unsafe {
        let set: SetFn = std::mem::transmute(slot(this, SLOT_SET_PERSISTED));
        let hr1 = set(this, pid, 0, 0, h.0); // eRender / eConsole
        let hr2 = set(this, pid, 0, 1, h.0); // eMultimedia
        let _ = set(this, pid, 0, 2, h.0); // eCommunications, best effort
        if (hr1 != 0 || hr2 != 0) && !(h.0.is_null() && hr1 == E_INVALIDARG) {
            log::warn(format!("SetPersistedDefaultAudioEndpoint pid={pid} hr=0x{:08X}/0x{:08X}", hr1 as u32, hr2 as u32));
        }
        hr1 == 0 && hr2 == 0
    }
}

/// The MMDevice id a process is routed to, empty string for "default", None on error.
pub fn get_app_device(pid: u32) -> Option<String> {
    let f = factory();
    if f == 0 {
        return None;
    }
    let this = f as *mut c_void;
    unsafe {
        let get: GetFn = std::mem::transmute(slot(this, SLOT_GET_PERSISTED));
        let mut h = std::ptr::null_mut();
        if get(this, pid, 0, 1, &mut h) != 0 {
            return None;
        }
        if h.is_null() {
            return Some(String::new());
        }
        let hs = HStr(h);
        let mut len = 0u32;
        let p = WindowsGetStringRawBuffer(hs.0, &mut len);
        let mut s = if p.is_null() { String::new() } else { String::from_utf16_lossy(std::slice::from_raw_parts(p, len as usize)) };
        if s.len() >= DEVICE_PREFIX.len() && s[..DEVICE_PREFIX.len()].eq_ignore_ascii_case(DEVICE_PREFIX) {
            s = s[DEVICE_PREFIX.len()..].to_string();
        }
        if let Some(i) = s.rfind('#') {
            if i > 0 {
                s.truncate(i);
            }
        }
        Some(s)
    }
}
