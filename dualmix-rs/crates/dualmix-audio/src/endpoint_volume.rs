//! Volume/mute of one playback endpoint with change notifications (port of C# `EndpointVolume`), plus the
//! device notification client (`IMMNotificationClient`).

use std::sync::Arc;

use dualmix_core::log;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{GUID, PCWSTR, implement};

use crate::devices;

const OWN_CONTEXT: GUID = GUID::from_u128(0x6d1f6c0a_2f0e_4b6e_9b56_d0a1f3c2e7a1);

pub type Callback = Arc<dyn Fn() + Send + Sync>;

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeSink {
    f: Callback,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeSink_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        // Changes made by DualMix are applied synchronously by the caller: only report foreign ones.
        if !data.is_null() && unsafe { (*data).guidEventContext } == OWN_CONTEXT {
            return Ok(());
        }
        (self.f)();
        Ok(())
    }
}

/// Lives on the UI thread; notifications arrive on a system thread and must be marshalled by the callback.
pub struct EndpointVolume {
    volume: Option<IAudioEndpointVolume>,
    sink: Option<IAudioEndpointVolumeCallback>,
    device_id: Option<String>,
    min_db: f32,
    changed: Callback,
}

impl EndpointVolume {
    pub fn new(changed: Callback) -> Self {
        Self { volume: None, sink: None, device_id: None, min_db: -96.0, changed }
    }

    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    pub fn is_available(&self) -> bool {
        self.volume.is_some()
    }

    pub fn attach(&mut self, device_id: Option<&str>) {
        if device_id == self.device_id.as_deref() && self.volume.is_some() {
            return;
        }
        self.detach();
        let Some(id) = device_id.filter(|s| !s.is_empty()) else { return };
        let Some(dev) = devices::get_device(id) else {
            log::warn(format!("Endpoint volume unavailable ({id})"));
            return;
        };
        unsafe {
            match dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
                Ok(v) => {
                    let (mut min, mut max, mut inc) = (0f32, 0f32, 0f32);
                    if v.GetVolumeRange(&mut min, &mut max, &mut inc).is_ok() {
                        self.min_db = min;
                    }
                    let sink: IAudioEndpointVolumeCallback = VolumeSink { f: self.changed.clone() }.into();
                    if v.RegisterControlChangeNotify(&sink).is_ok() {
                        self.sink = Some(sink);
                    }
                    self.volume = Some(v);
                    self.device_id = Some(id.to_string());
                }
                Err(e) => log::warn(format!("Endpoint volume unavailable | {e}")),
            }
        }
    }

    pub fn detach(&mut self) {
        if let (Some(v), Some(s)) = (&self.volume, &self.sink) {
            unsafe {
                let _ = v.UnregisterControlChangeNotify(s);
            }
        }
        self.sink = None;
        self.volume = None;
        self.device_id = None;
    }

    /// Slider position 0..1 (the scale Windows shows).
    pub fn level(&self) -> f64 {
        self.volume.as_ref().and_then(|v| unsafe { v.GetMasterVolumeLevelScalar().ok() }).unwrap_or(0.0) as f64
    }

    pub fn set_level(&self, value: f64) {
        if let Some(v) = &self.volume {
            if let Err(e) = unsafe { v.SetMasterVolumeLevelScalar(value.clamp(0.0, 1.0) as f32, &OWN_CONTEXT) } {
                log::warn(format!("Cannot set endpoint volume | {e}"));
            }
        }
    }

    pub fn muted(&self) -> bool {
        self.volume.as_ref().and_then(|v| unsafe { v.GetMute().ok() }).map(|b| b.as_bool()).unwrap_or(false)
    }

    pub fn set_muted(&self, value: bool) {
        if let Some(v) = &self.volume {
            if self.muted() != value {
                if let Err(e) = unsafe { v.SetMute(BOOL::from(value), &OWN_CONTEXT) } {
                    log::warn(format!("Cannot set endpoint mute | {e}"));
                }
            }
        }
    }

    /// Linear gain equivalent to the current level (the endpoint dB curve); exactly 1 at 100%, 0 at 0%.
    pub fn amplitude(&self) -> f64 {
        let Some(v) = &self.volume else { return 1.0 };
        match unsafe { v.GetMasterVolumeLevel() } {
            Ok(db) => {
                if db >= -0.001 {
                    1.0
                } else if db <= self.min_db + 0.01 {
                    0.0
                } else {
                    10f64.powf(db as f64 / 20.0)
                }
            }
            Err(_) => 1.0,
        }
    }
}

impl Drop for EndpointVolume {
    fn drop(&mut self) {
        self.detach();
    }
}

// ------------------------------------------------------------------------------------------ device notifications

/// What changed in the device list.
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A device was added, removed, enabled or disabled.
    Changed,
    /// The default render device of a role changed (role 0..2, new id).
    DefaultChanged { role: i32, id: String },
    /// The device format of an endpoint changed.
    FormatChanged { id: String },
}

pub type DeviceCallback = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

#[implement(IMMNotificationClient)]
struct DeviceSink {
    f: DeviceCallback,
}

fn pcwstr(p: &PCWSTR) -> String {
    if p.is_null() { String::new() } else { unsafe { p.to_string().unwrap_or_default() } }
}

impl IMMNotificationClient_Impl for DeviceSink_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        (self.f)(DeviceEvent::Changed);
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.f)(DeviceEvent::Changed);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.f)(DeviceEvent::Changed);
        Ok(())
    }

    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, id: &PCWSTR) -> windows::core::Result<()> {
        if flow.0 == 0 {
            (self.f)(DeviceEvent::DefaultChanged { role: role.0, id: pcwstr(id) });
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, id: &PCWSTR, key: &PROPERTYKEY) -> windows::core::Result<()> {
        if key.fmtid == crate::com::PKEY_DEVICE_FORMAT.fmtid {
            (self.f)(DeviceEvent::FormatChanged { id: pcwstr(id) });
        }
        Ok(())
    }
}

/// Registration of a device notification client; unregisters on drop.
pub struct DeviceNotifications {
    en: IMMDeviceEnumerator,
    sink: IMMNotificationClient,
}

impl DeviceNotifications {
    pub fn register(f: DeviceCallback) -> Option<Self> {
        let en = crate::com::enumerator().ok()?;
        let sink: IMMNotificationClient = DeviceSink { f }.into();
        match unsafe { en.RegisterEndpointNotificationCallback(&sink) } {
            Ok(()) => Some(Self { en, sink }),
            Err(e) => {
                log::warn(format!("Device notifications unavailable | {e}"));
                None
            }
        }
    }
}

impl Drop for DeviceNotifications {
    fn drop(&mut self) {
        unsafe {
            let _ = self.en.UnregisterEndpointNotificationCallback(&self.sink);
        }
    }
}
