//! Small COM helpers shared by the audio modules.

use std::ffi::c_void;

use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};
use windows::core::{GUID, PWSTR};

/// Keeps COM initialised on the current thread for its lifetime.
pub struct ComGuard {
    owned: bool,
}

impl ComGuard {
    pub fn mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self { owned: hr.is_ok() }
    }

    pub fn sta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self { owned: hr.is_ok() }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

pub fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Converts a COM-allocated wide string to `String` and frees it.
///
/// # Safety
/// `p` must be a string allocated with `CoTaskMemAlloc` (or null).
pub unsafe fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.as_ptr() as *const c_void));
        s
    }
}

pub const fn pkey(fmtid: u128, pid: u32) -> PROPERTYKEY {
    PROPERTYKEY { fmtid: GUID::from_u128(fmtid), pid }
}

pub const PKEY_DEVICE_DESC: PROPERTYKEY = pkey(0xa45c254e_df1c_4efd_8020_67d146a850e0, 2);
pub const PKEY_FRIENDLY_NAME: PROPERTYKEY = pkey(0xa45c254e_df1c_4efd_8020_67d146a850e0, 14);
pub const PKEY_INTERFACE_NAME: PROPERTYKEY = pkey(0x026e516e_b814_414b_83cd_856d6fef4822, 2);
pub const PKEY_DEVICE_FORMAT: PROPERTYKEY = pkey(0xf19f064d_082c_4e27_bc73_6882a1bb8e4c, 0);
pub const PKEY_ADAPTER: PROPERTYKEY = pkey(0xb3f8fa53_0004_438e_9003_51a46e139bfc, 2);
pub const PKEY_JACK_SUBTYPE: PROPERTYKEY = pkey(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e, 8);

/// Value of a property read from a property store.
pub enum PropValue {
    Str(String),
    Blob(Vec<u8>),
    Other,
}

/// Reads a property as string / blob, decoding the PROPVARIANT by its fixed C layout.
pub fn read_prop(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<PropValue> {
    unsafe {
        let pv = store.GetValue(key).ok()?;
        let raw = &pv as *const _ as *const u8;
        let vt = *(raw as *const u16);
        let value = match vt {
            31 => {
                // VT_LPWSTR
                let p = *(raw.add(8) as *const *const u16);
                if p.is_null() {
                    PropValue::Other
                } else {
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    PropValue::Str(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)))
                }
            }
            65 => {
                // VT_BLOB
                let cb = *(raw.add(8) as *const u32) as usize;
                let p = *(raw.add(16) as *const *const u8);
                if p.is_null() { PropValue::Other } else { PropValue::Blob(std::slice::from_raw_parts(p, cb).to_vec()) }
            }
            72 => {
                // VT_CLSID
                let p = *(raw.add(8) as *const *const GUID);
                if p.is_null() { PropValue::Other } else { PropValue::Str(format!("{{{:?}}}", *p).to_lowercase()) }
            }
            0 => return None,
            _ => PropValue::Other,
        };
        drop(pv);
        Some(value)
    }
}

pub fn read_string(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    match read_prop(store, key)? {
        PropValue::Str(s) => Some(s),
        _ => None,
    }
}

/// Human readable HRESULT of audio errors (same wording as the C# router).
pub fn describe_hresult(hr: i32) -> String {
    let key = match hr as u32 {
        0x88890004 => Some(("device unplugged or format changed", "dispositivo scollegato o formato cambiato")),
        0x8889000A => Some(("device in exclusive use by another application", "dispositivo in uso esclusivo da un'altra applicazione")),
        0x88890008 => Some(("audio format not supported", "formato audio non supportato")),
        0x80070490 => Some(("device not found", "dispositivo non trovato")),
        0x80040154 => Some((
            "the Windows audio service lost the device (the audio service needs a restart)",
            "il servizio audio di Windows ha perso il dispositivo (serve un riavvio del servizio audio)",
        )),
        _ => None,
    };
    match key {
        Some((en, it)) => dualmix_core::loc::current().pick(en, it).to_string(),
        None => format!("{} 0x{:08X}", dualmix_core::loc::current().pick("audio error", "errore audio"), hr as u32),
    }
}
