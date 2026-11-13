//! Audio endpoint enumeration, the devices DualMix cares about and Windows audio policy changes
//! (1:1 port of C# `DualMix.Audio.Devices` / `DeviceInfo`).
#![allow(non_snake_case)]

use std::cmp::Ordering;

use dualmix_core::log;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IAudioClient, IMMDevice, IMMDeviceEnumerator, WAVEFORMATEX, eCapture, eRender,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

use crate::com::{self, PKEY_ADAPTER, PKEY_DEVICE_DESC, PKEY_DEVICE_FORMAT, PKEY_FRIENDLY_NAME, PKEY_INTERFACE_NAME, PKEY_JACK_SUBTYPE};
use crate::format::{self, AudioFormat};

pub const STATE_ACTIVE: u32 = 0x1;
pub const STATE_DISABLED: u32 = 0x2;
pub const STATE_NOT_PRESENT: u32 = 0x4;
pub const STATE_UNPLUGGED: u32 = 0x8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Render,
    Capture,
}

impl Flow {
    fn raw(self) -> EDataFlow {
        match self {
            Flow::Render => eRender,
            Flow::Capture => eCapture,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Console = 0,
    Multimedia = 1,
    Communications = 2,
}

pub const SECONDARY_CABLE_NAME: &str = "CABLE In 16 Ch";
pub const GAME_ENDPOINT_NAME: &str = "DualMix Game";
pub const CHAT_ENDPOINT_NAME: &str = "DualMix Chat";
const SPEAKER_JACK: &str = "{DFF21CE1-F70F-11D0-B917-00A0C9223196}";

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub interface_name: String,
    pub flow: Flow,
    pub state: u32,
    pub is_default: bool,
    pub adapter: String,
    pub jack_sub_type: String,
}

fn contains_ci(h: &str, n: &str) -> bool {
    h.to_lowercase().contains(&n.to_lowercase())
}

impl DeviceInfo {
    /// A VB-Audio virtual cable (VB-CABLE, VB-CABLE A/B/C/D, Hi-Fi Cable). Voicemeeter devices are not used.
    pub fn is_vb_cable(&self) -> bool {
        self.interface_name.to_lowercase().starts_with("vb-audio")
            && contains_ci(&self.interface_name, "cable")
            && !contains_ci(&self.interface_name, "voicemeeter")
    }

    /// The main playback jack of its device (VB-CABLE also exposes a secondary "16 ch" input on the same stream).
    pub fn is_main_jack(&self) -> bool {
        self.jack_sub_type.eq_ignore_ascii_case(SPEAKER_JACK) || (self.jack_sub_type.is_empty() && !self.description.contains("16"))
    }

    /// The free VB-CABLE 3.x driver: its loopback is requantised (19 bit), so it is used only when nothing better exists.
    pub fn is_lossy_cable(&self) -> bool {
        self.is_vb_cable() && contains_ci(&self.interface_name, "virtual cable")
    }

    /// Factory name of the main playback endpoint of this cable.
    pub fn factory_name(&self) -> String {
        let low = self.interface_name.to_lowercase();
        for l in ["a", "b", "c", "d"] {
            if low.ends_with(&format!("cable {l}")) || low.ends_with(&format!("cable-{l}")) {
                return format!("CABLE-{} Input", l.to_uppercase());
            }
        }
        if low.contains("hi-fi") { "Hi-Fi Cable Input".into() } else { "CABLE Input".into() }
    }

    pub fn device_key(&self) -> String {
        if self.adapter.is_empty() { self.interface_name.to_lowercase() } else { self.adapter.to_lowercase() }
    }

    pub fn is_ae5(&self) -> bool {
        contains_ci(&self.name, "AE-5") || contains_ci(&self.interface_name, "AE-5")
    }

    pub fn is_active(&self) -> bool {
        self.state == STATE_ACTIVE
    }
}

fn id_of(dev: &IMMDevice) -> Option<String> {
    unsafe { dev.GetId().ok().map(|p| com::take_pwstr(p)) }
}

fn default_id_with(en: &IMMDeviceEnumerator, role: Role, flow: Flow) -> Option<String> {
    unsafe { en.GetDefaultAudioEndpoint(flow.raw(), ERole(role as i32)).ok().and_then(|d| id_of(&d)) }
}

fn test_real_output() -> Option<String> {
    std::env::var("DUALMIX_TEST_REAL_OUTPUT").ok().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty())
}

pub fn list(flow: Flow, states: u32) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    let Ok(en) = com::enumerator() else {
        log::error("Device enumeration failed: no MMDeviceEnumerator");
        return out;
    };
    let def = default_id_with(&en, Role::Multimedia, flow);
    unsafe {
        let col = match en.EnumAudioEndpoints(flow.raw(), DEVICE_STATE(states)) {
            Ok(c) => c,
            Err(e) => {
                log::error(format!("Device enumeration failed: {e}"));
                return out;
            }
        };
        let n = col.GetCount().unwrap_or(0);
        for i in 0..n {
            let Ok(dev) = col.Item(i) else { continue };
            let Some(id) = id_of(&dev) else { continue };
            let state = dev.GetState().map(|s| s.0).unwrap_or(STATE_NOT_PRESENT);
            let (mut name, mut desc, mut iface, mut adapter, mut jack) = (String::new(), String::new(), String::new(), String::new(), String::new());
            if let Ok(store) = dev.OpenPropertyStore(STGM_READ) {
                name = com::read_string(&store, &PKEY_FRIENDLY_NAME).unwrap_or_default();
                desc = com::read_string(&store, &PKEY_DEVICE_DESC).unwrap_or_default();
                iface = com::read_string(&store, &PKEY_INTERFACE_NAME).unwrap_or_default();
                adapter = com::read_string(&store, &PKEY_ADAPTER).unwrap_or_default();
                jack = com::read_string(&store, &PKEY_JACK_SUBTYPE).unwrap_or_default();
            }
            if name.is_empty() {
                name = id.clone();
            }
            if desc.is_empty() {
                desc = name.clone();
            }
            // Test rigs without a sound card (CI / VMs): DUALMIX_TEST_REAL_OUTPUT=<name part> makes that endpoint
            // count as a real output instead of a virtual cable.
            if let Some(t) = test_real_output() {
                if name.to_lowercase().contains(&t) {
                    iface = format!("TEST OUTPUT {iface}").replace("VB-Audio", "VBA").replace("Cable", "Cbl");
                }
            }
            out.push(DeviceInfo {
                is_default: def.as_deref() == Some(id.as_str()),
                id,
                name,
                description: desc,
                interface_name: iface,
                flow,
                state,
                adapter,
                jack_sub_type: jack,
            });
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

pub fn list_render() -> Vec<DeviceInfo> {
    list(Flow::Render, STATE_ACTIVE)
}

/// The real output DualMix should use: the active Windows default or the best analog/headphone device on any PC.
pub fn find_preferred_output<'a>(render: &'a [DeviceInfo], preferred_id: Option<&str>) -> Option<&'a DeviceInfo> {
    let score = |d: &DeviceInfo| -> i32 {
        if d.is_vb_cable() {
            return -1000;
        }
        let mut s = 0;
        if let Some(p) = preferred_id {
            if !p.is_empty() && d.id.eq_ignore_ascii_case(p) {
                s += 200;
            }
        }
        if d.is_default {
            s += 100;
        }
        if d.is_ae5() {
            s += 15;
        }
        let n = format!("{} {}", d.name, d.interface_name).to_lowercase();
        let any = |words: &[&str]| words.iter().any(|w| n.contains(w));
        if any(&["spdif", "optical", "digital", "digitale"]) {
            s -= 45;
        }
        if any(&["hdmi", "displayport", "nvidia", "amd high definition", "intel(r) display"]) {
            s -= 35;
        }
        if any(&["steam streaming", "oculus", "virtual"]) {
            s -= 60;
        }
        if any(&["altoparlanti", "speakers", "cuffie", "headphones", "headset", "earphones", "auricolari"]) {
            s += 25;
        }
        if any(&["usb", "dac", "realtek"]) {
            s += 10;
        }
        s
    };
    let mut best: Option<(&DeviceInfo, i32)> = None;
    for d in render.iter().filter(|d| !d.is_vb_cable()) {
        let sc = score(d);
        match best {
            None => best = Some((d, sc)),
            Some((b, bs)) => {
                if sc > bs || (sc == bs && d.is_default && !b.is_default) {
                    best = Some((d, sc));
                }
            }
        }
    }
    best.map(|(d, _)| d)
}

/// The usable virtual buses: one per cable device (main jack), bit-transparent cables first.
pub fn find_buses(render: &[DeviceInfo]) -> Vec<DeviceInfo> {
    let mut groups: Vec<(String, Vec<&DeviceInfo>)> = Vec::new();
    for d in render.iter().filter(|d| d.is_vb_cable() && d.is_active()) {
        let k = d.device_key();
        match groups.iter_mut().find(|(gk, _)| *gk == k) {
            Some((_, v)) => v.push(d),
            None => groups.push((k, vec![d])),
        }
    }
    let mut buses: Vec<DeviceInfo> = groups
        .into_iter()
        .map(|(_, mut v)| {
            v.sort_by_key(|d| if d.is_main_jack() { 0 } else { 1 });
            v[0].clone()
        })
        .collect();
    buses.sort_by(|a, b| match (a.is_lossy_cable() as u8).cmp(&(b.is_lossy_cable() as u8)) {
        Ordering::Equal => a.interface_name.to_lowercase().cmp(&b.interface_name.to_lowercase()),
        o => o,
    });
    buses
}

/// One cable: Chat (games on the real output). Two or more: Game gets the first transparent cable, Chat the next.
pub fn find_cables(render: &[DeviceInfo]) -> (Option<DeviceInfo>, Option<DeviceInfo>) {
    let buses = find_buses(render);
    match buses.len() {
        0 => (None, None),
        1 => (None, Some(buses[0].clone())),
        _ => (Some(buses[0].clone()), Some(buses[1].clone())),
    }
}

pub fn get_default_id(role: Role, flow: Flow) -> Option<String> {
    let en = com::enumerator().ok()?;
    default_id_with(&en, role, flow)
}

pub fn get_device(id: &str) -> Option<IMMDevice> {
    let en = com::enumerator().ok()?;
    unsafe { en.GetDevice(&HSTRING::from(id)).ok() }
}

pub fn is_active(id: Option<&str>) -> bool {
    let Some(id) = id.filter(|s| !s.is_empty()) else { return false };
    get_device(id).map(|d| unsafe { d.GetState().map(|s| s.0 == STATE_ACTIVE).unwrap_or(false) }).unwrap_or(false)
}

pub fn name_of(id: Option<&str>) -> String {
    let Some(id) = id.filter(|s| !s.is_empty()) else { return dualmix_core::loc::get("Common.None") };
    match get_device(id) {
        Some(d) => unsafe {
            d.OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|s| com::read_string(&s, &PKEY_FRIENDLY_NAME))
                .unwrap_or_else(|| id.to_string())
        },
        None => {
            log::warn(format!("Cannot read name of device {id}"));
            dualmix_core::loc::get("Common.Unavailable")
        }
    }
}

/// The endpoint's configured device format (Sound control panel → Advanced), read from its property store.
pub fn get_device_format(id: &str) -> Option<AudioFormat> {
    let d = get_device(id)?;
    unsafe {
        let store = d.OpenPropertyStore(STGM_READ).ok()?;
        match com::read_prop(&store, &PKEY_DEVICE_FORMAT)? {
            com::PropValue::Blob(b) => format::from_bytes(&b),
            _ => None,
        }
    }
}

/// Mix format of the shared-mode engine of a device.
pub fn get_mix_format(id: &str) -> Option<AudioFormat> {
    let d = get_device(id)?;
    unsafe {
        let client: IAudioClient = d.Activate(CLSCTX_ALL, None).ok()?;
        let p: *mut WAVEFORMATEX = client.GetMixFormat().ok()?;
        let f = format::from_ptr(p as *const u8);
        CoTaskMemFree(Some(p as *const _));
        f
    }
}

/// Checks that the endpoint can be opened. None when fine, otherwise the failing HRESULT.
pub fn probe_open(id: &str) -> Option<i32> {
    let Some(d) = get_device(id) else { return Some(0x80070490u32 as i32) };
    unsafe {
        match d.Activate::<IAudioClient>(CLSCTX_ALL, None) {
            Ok(client) => match client.GetMixFormat() {
                Ok(p) => {
                    CoTaskMemFree(Some(p as *const _));
                    None
                }
                Err(e) => Some(e.code().0),
            },
            Err(e) => Some(e.code().0),
        }
    }
}

// ------------------------------------------------------------------ policy (undocumented, stable since Vista)

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self, id: PCWSTR, format: *mut *mut WAVEFORMATEX) -> HRESULT;
    fn GetDeviceFormat(&self, id: PCWSTR, use_default: i32, format: *mut *mut WAVEFORMATEX) -> HRESULT;
    fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
    fn SetDeviceFormat(&self, id: PCWSTR, endpoint: *const u8, mix: *const u8) -> HRESULT;
    fn GetProcessingPeriod(&self, id: PCWSTR, use_default: i32, def: *mut i64, min: *mut i64) -> HRESULT;
    fn SetProcessingPeriod(&self, id: PCWSTR, period: *const i64) -> HRESULT;
    fn GetShareMode(&self, id: PCWSTR, mode: *mut u8) -> HRESULT;
    fn SetShareMode(&self, id: PCWSTR, mode: *const u8) -> HRESULT;
    fn GetPropertyValue(&self, id: PCWSTR, fx_store: i32, key: *const PROPERTYKEY, value: *mut u8) -> HRESULT;
    fn SetPropertyValue(&self, id: PCWSTR, fx_store: i32, key: *const PROPERTYKEY, value: *const u8) -> HRESULT;
    fn SetDefaultEndpoint(&self, id: PCWSTR, role: i32) -> HRESULT;
    fn SetEndpointVisibility(&self, id: PCWSTR, visible: i32) -> HRESULT;
}

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

fn with_policy(what: &str, f: impl FnOnce(&IPolicyConfig) -> HRESULT) -> bool {
    match unsafe { CoCreateInstance::<_, IPolicyConfig>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) } {
        Ok(p) => {
            let hr = f(&p);
            if hr.is_err() {
                log::warn(format!("{what} hr=0x{:08X}", hr.0 as u32));
            }
            hr.is_ok()
        }
        Err(e) => {
            log::error(format!("IPolicyConfig call failed: {e}"));
            false
        }
    }
}

/// True when the undocumented IPolicyConfig COM class can be created on this Windows.
pub fn policy_config_available() -> bool {
    unsafe { CoCreateInstance::<_, IPolicyConfig>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL).is_ok() }
}

/// Sets the Windows default playback device for a role.
pub fn set_default(id: &str, role: Role) -> bool {
    let h = HSTRING::from(id);
    with_policy(&format!("SetDefaultEndpoint({role:?})"), |p| unsafe { p.SetDefaultEndpoint(PCWSTR(h.as_ptr()), role as i32) })
}

/// Changes the device format (what the Sound control panel "Advanced" tab does).
pub fn set_device_format(id: &str, f: &AudioFormat) -> bool {
    let dev = format::to_bytes(f);
    let mix = format::to_bytes(&f.as_float_mix());
    let h = HSTRING::from(id);
    with_policy(&format!("SetDeviceFormat({}) on {id}", format::describe(f)), |p| unsafe {
        p.SetDeviceFormat(PCWSTR(h.as_ptr()), dev.as_ptr(), mix.as_ptr())
    })
}

/// Renames an endpoint (its "device description", the first part of the friendly name).
pub fn rename(id: &str, description: &str) -> bool {
    let h = HSTRING::from(id);
    let text = HSTRING::from(description);
    // PROPVARIANT VT_LPWSTR: vt (u16) + 3 reserved u16, then the pointer.
    let mut pv = [0u8; 24];
    pv[0..2].copy_from_slice(&31u16.to_le_bytes());
    let ptr = text.as_ptr() as usize;
    pv[8..16].copy_from_slice(&ptr.to_le_bytes());
    let key = PKEY_DEVICE_DESC;
    with_policy(&format!("Rename endpoint {id}"), |p| unsafe { p.SetPropertyValue(PCWSTR(h.as_ptr()), 0, &key, pv.as_ptr()) })
}

/// Enables/disables an endpoint (hidden endpoints do not appear in app device lists).
pub fn set_visible(id: &str, visible: bool) -> bool {
    let h = HSTRING::from(id);
    with_policy(&format!("SetEndpointVisibility({visible}) on {id}"), |p| unsafe {
        p.SetEndpointVisibility(PCWSTR(h.as_ptr()), visible as i32)
    })
}

/// Endpoint volume to 100% and unmuted (bit-transparent path).
pub fn set_unity_volume(id: &str) {
    let Some(d) = get_device(id) else { return };
    unsafe {
        if let Ok(v) = d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            if v.GetMasterVolumeLevelScalar().map(|l| (l - 1.0).abs() > 0.0001).unwrap_or(false) {
                let _ = v.SetMasterVolumeLevelScalar(1.0, std::ptr::null());
            }
            if v.GetMute().map(|m| m.as_bool()).unwrap_or(false) {
                let _ = v.SetMute(false, std::ptr::null());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, iface: &str, default: bool) -> DeviceInfo {
        DeviceInfo {
            id: name.into(),
            name: name.into(),
            description: name.into(),
            interface_name: iface.into(),
            flow: Flow::Render,
            state: STATE_ACTIVE,
            is_default: default,
            adapter: String::new(),
            jack_sub_type: String::new(),
        }
    }

    #[test]
    fn preferred_output_and_cables() {
        let list = vec![
            dev("Speakers (Realtek)", "Realtek High Definition Audio", false),
            dev("NVIDIA HDMI", "NVIDIA High Definition Audio", true),
            dev("CABLE-A Input", "VB-Audio Cable A", false),
            dev("CABLE Input", "VB-Audio Virtual Cable", false),
            dev("CABLE-B Input", "VB-Audio Cable B", false),
        ];
        let out = find_preferred_output(&list, None).unwrap();
        assert_eq!(out.name, "NVIDIA HDMI"); // default wins (+100 −35 vs +35)
        let out = find_preferred_output(&list, Some("Speakers (Realtek)")).unwrap();
        assert_eq!(out.name, "Speakers (Realtek)");
        let (g, c) = find_cables(&list);
        assert_eq!(g.unwrap().name, "CABLE-A Input");
        assert_eq!(c.unwrap().name, "CABLE-B Input");
        assert_eq!(list[2].factory_name(), "CABLE-A Input");
        assert!(list[3].is_lossy_cable());
    }
}
