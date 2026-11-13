//! Session engine, 1:1 port of C# `DualMix.Audio.SessionEngine`.
//!
//! Watches every audio session on every active playback device (dedicated MTA thread, `IAudioSessionManager2`
//! notifications plus a slow rescan as safety net) and classifies each app as Game/Chat. Sessions on a real device
//! get their channel gain through `ISimpleAudioVolume`; sessions on a DualMix virtual device only their per-app
//! volume (the router applies the channel gain). Apps are sent to the device of their channel through
//! `AudioPolicyConfig` (slots 25/26). Peaks come from `IAudioMeterInformation`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use dualmix_core::log;
use dualmix_core::math::ChannelGains;
use dualmix_core::rules::{RuleSet, SYSTEM_SOUNDS_KEY};
use dualmix_core::settings::Channel;
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, eRender,
};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject};
use windows::core::{GUID, HSTRING, Interface, implement};

use crate::com::{self, ComGuard};
use crate::router::tick_ms;
use crate::{app_routing, devices, process_info};

const RESCAN_INTERVAL_MS: i64 = 3000;
const VERIFY_INTERVAL_MS: i64 = 1000;

/// Parameters the session engine works with (immutable, swapped atomically).
#[derive(Clone, Debug)]
pub struct SessionParams {
    pub gains: ChannelGains,
    pub rules: Arc<RuleSet>,
    /// Changes whenever `rules` is rebuilt (reference identity of the C# version).
    pub rules_gen: u64,
    pub route_devices: bool,
    pub game_route_id: Option<String>,
    pub chat_route_id: Option<String>,
    pub direct_route_id: Option<String>,
    pub game_bus_id: Option<String>,
    pub chat_bus_id: Option<String>,
    pub meters_enabled: bool,
}

/// One application with audio, aggregated over all its sessions.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioAppInfo {
    pub key: String,
    pub display_name: String,
    pub exe_path: Option<String>,
    pub is_system_sounds: bool,
    pub channel: Channel,
    pub is_explicit: bool,
    pub auto_channel: Channel,
    pub relative_volume: f64,
    pub peak: f32,
    pub active: bool,
    pub devices: String,
    /// Playing on a real device although its channel has a virtual one (output fixed in the app's own settings).
    pub fixed_output: bool,
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// A process was seen for the first time; `volume` is its current Windows volume.
    AppDiscovered { key: String, display_name: String, exe_path: Option<String>, volume: f64 },
    /// The user changed an app volume in the Windows volume mixer: adopt it as relative volume.
    AppVolumeAdopted { key: String, volume: f64 },
    /// A process was routed to a virtual device.
    AppRouted { key: String },
}

type Command = Box<dyn FnOnce(&mut Inner) + Send>;

struct WakeEvent(HANDLE);
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

struct Shared {
    params: Mutex<(Arc<SessionParams>, u64)>,
    wake: WakeEvent,
    stop: AtomicBool,
    rescan: AtomicBool,
    restore_on_stop: AtomicBool,
    route_reset: AtomicBool,
    dirty: AtomicBool,
    running: AtomicBool,
    commands: Mutex<VecDeque<Command>>,
    events: Mutex<VecDeque<EngineEvent>>,
    snapshot: Mutex<Arc<Vec<AudioAppInfo>>>,
    game_peak: AtomicU32,
    chat_peak: AtomicU32,
    param_gen: AtomicU64,
}

impl Shared {
    fn wake(&self) {
        unsafe {
            let _ = SetEvent(self.wake.0);
        }
    }
    fn params(&self) -> (Arc<SessionParams>, u64) {
        self.params.lock().map(|g| g.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }
}

pub struct SessionEngine {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl SessionEngine {
    pub fn new(initial: SessionParams) -> Self {
        let wake = unsafe { CreateEventW(None, false, false, None).unwrap_or_default() };
        Self {
            shared: Arc::new(Shared {
                params: Mutex::new((Arc::new(initial), 1)),
                wake: WakeEvent(wake),
                stop: AtomicBool::new(false),
                rescan: AtomicBool::new(true),
                restore_on_stop: AtomicBool::new(true),
                route_reset: AtomicBool::new(false),
                dirty: AtomicBool::new(true),
                running: AtomicBool::new(false),
                commands: Mutex::new(VecDeque::new()),
                events: Mutex::new(VecDeque::new()),
                snapshot: Mutex::new(Arc::new(Vec::new())),
                game_peak: AtomicU32::new(0),
                chat_peak: AtomicU32::new(0),
                param_gen: AtomicU64::new(1),
            }),
            thread: None,
        }
    }

    pub fn start(&mut self) {
        if self.thread.is_some() {
            return;
        }
        let sh = Arc::clone(&self.shared);
        sh.running.store(true, Ordering::Release);
        self.thread = std::thread::Builder::new().name("DualMix.Sessions".into()).spawn(move || run(sh)).ok();
    }

    pub fn update(&self, p: SessionParams) {
        let sh = &self.shared;
        let pgen = sh.param_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let mut g = sh.params.lock().unwrap_or_else(|e| e.into_inner());
        let old = Arc::clone(&g.0);
        if old.rules_gen != p.rules_gen || old.route_devices != p.route_devices || old.game_bus_id != p.game_bus_id || old.chat_bus_id != p.chat_bus_id {
            sh.dirty.store(true, Ordering::Relaxed);
        }
        if old.game_route_id != p.game_route_id
            || old.chat_route_id != p.chat_route_id
            || old.direct_route_id != p.direct_route_id
            || old.route_devices != p.route_devices
            || old.rules_gen != p.rules_gen
        {
            sh.route_reset.store(true, Ordering::Relaxed);
        }
        *g = (Arc::new(p), pgen);
        drop(g);
        sh.wake();
    }

    pub fn request_rescan(&self) {
        self.shared.rescan.store(true, Ordering::Relaxed);
        self.shared.wake();
    }

    pub fn try_dequeue_event(&self) -> Option<EngineEvent> {
        self.shared.events.lock().ok().and_then(|mut q| q.pop_front())
    }

    pub fn snapshot(&self) -> Arc<Vec<AudioAppInfo>> {
        self.shared.snapshot.lock().map(|g| Arc::clone(&g)).unwrap_or_default()
    }

    pub fn game_peak(&self) -> f32 {
        f32::from_bits(self.shared.game_peak.load(Ordering::Relaxed))
    }

    pub fn chat_peak(&self) -> f32 {
        f32::from_bits(self.shared.chat_peak.load(Ordering::Relaxed))
    }

    fn invoke(&self, f: impl FnOnce(&mut Inner) + Send + 'static) -> mpsc::Receiver<bool> {
        let (tx, rx) = mpsc::channel();
        if self.shared.stop.load(Ordering::Acquire) || !self.shared.running.load(Ordering::Acquire) {
            let _ = tx.send(false);
            return rx;
        }
        let cmd: Command = Box::new(move |inner: &mut Inner| {
            f(inner);
            let _ = tx.send(true);
        });
        if let Ok(mut q) = self.shared.commands.lock() {
            q.push_back(cmd);
        }
        self.shared.wake();
        rx
    }

    /// Resets the per-app output device of every app DualMix routed (plus the given process names).
    /// The receiver yields `true` once done.
    pub fn unroute_all(&self, extra_keys: Vec<String>) -> mpsc::Receiver<bool> {
        self.invoke(move |inner| inner.unroute_all(&extra_keys))
    }

    pub fn stop(&mut self, restore_volumes: bool) {
        self.shared.restore_on_stop.store(restore_volumes, Ordering::Relaxed);
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake();
        if let Some(h) = self.thread.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(3000);
            while self.shared.running.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.shared.running.load(Ordering::Acquire) {
                log::warn("Session engine thread did not stop in time");
            } else {
                let _ = h.join();
            }
        }
    }
}

impl Drop for SessionEngine {
    fn drop(&mut self) {
        if !self.shared.stop.load(Ordering::Acquire) {
            self.stop(true);
        }
        unsafe {
            let _ = CloseHandle(self.shared.wake.0);
        }
    }
}

// ------------------------------------------------------------------------------------------ engine thread

#[implement(IAudioSessionNotification)]
struct SessionSink {
    shared: Arc<Shared>,
}

impl IAudioSessionNotification_Impl for SessionSink_Impl {
    fn OnSessionCreated(&self, _new: Option<&IAudioSessionControl>) -> windows::core::Result<()> {
        // Do not touch the session here (re-entrancy rules): just ask for a rescan.
        self.shared.rescan.store(true, Ordering::Relaxed);
        self.shared.wake();
        Ok(())
    }
}

struct Tracked {
    control: IAudioSessionControl2,
    volume: ISimpleAudioVolume,
    meter: Option<IAudioMeterInformation>,
    pid: u32,
    key: String,
    display_name: String,
    exe_path: Option<String>,
    is_system: bool,
    device_id: String,
    state: i32,
    last_set: f32,
    touched: bool,
    peak: f32,
    seen: bool,
}

struct DeviceHolder {
    name: String,
    _device: IMMDevice,
    manager: IAudioSessionManager2,
    registered: bool,
}

struct Inner {
    shared: Arc<Shared>,
    sink: IAudioSessionNotification,
    tracked: HashMap<String, Tracked>,
    devices: HashMap<String, DeviceHolder>,
    device_names: HashMap<String, String>,
    learned: HashMap<String, f64>,
    routed_pids: HashMap<u32, String>,
    routed_keys: HashSet<String>,
    own_pid: u32,
    enumerator: Option<IMMDeviceEnumerator>,
    applied_gen: u64,
    route_blocked_gen: u64,
    last_publish: i64,
    dirty: bool,
}

fn run(sh: Arc<Shared>) {
    let _com = ComGuard::mta();
    log::info("Session engine started");
    let sink: IAudioSessionNotification = SessionSink { shared: Arc::clone(&sh) }.into();
    let mut inner = Inner {
        shared: Arc::clone(&sh),
        sink,
        tracked: HashMap::new(),
        devices: HashMap::new(),
        device_names: HashMap::new(),
        learned: HashMap::new(),
        routed_pids: HashMap::new(),
        routed_keys: HashSet::new(),
        own_pid: std::process::id(),
        enumerator: None,
        applied_gen: 0,
        route_blocked_gen: u64::MAX,
        last_publish: 0,
        dirty: true,
    };
    let (mut next_scan, mut next_verify) = (0i64, 0i64);
    let mut failures = 0u64;
    while !sh.stop.load(Ordering::Acquire) {
        let (mut p, mut pgen) = sh.params();
        let res: Result<(), String> = (|| {
            if inner.enumerator.is_none() {
                inner.enumerator = Some(com::enumerator().map_err(|e| e.to_string())?);
            }
            inner.drain_commands();
            (p, pgen) = sh.params(); // a command refers to the newest parameters
            if sh.dirty.swap(false, Ordering::Relaxed) {
                inner.dirty = true;
            }
            let now = tick_ms();
            let mut scanned = false;
            if sh.rescan.swap(false, Ordering::Relaxed) || now >= next_scan {
                inner.scan(&p)?;
                next_scan = now + RESCAN_INTERVAL_MS;
                scanned = true;
            }
            // Apply immediately when something changed, otherwise verify once per second (Windows mixer changes).
            if scanned || pgen != inner.applied_gen || now >= next_verify {
                inner.apply(&p);
                inner.applied_gen = pgen;
                next_verify = now + VERIFY_INTERVAL_MS;
                if sh.route_reset.swap(false, Ordering::Relaxed) {
                    // Re-check every app against the new targets (keeps the "routed" bookkeeping).
                    for v in inner.routed_pids.values_mut() {
                        *v = if v.is_empty() { String::new() } else { "?".into() };
                    }
                }
                // After "unroute everything" nothing is routed again until the controller sends new parameters.
                if p.route_devices && pgen != inner.route_blocked_gen {
                    inner.route(&p);
                }
            }
            if p.meters_enabled {
                inner.sample_meters(&p);
            } else {
                sh.game_peak.store(0, Ordering::Relaxed);
                sh.chat_peak.store(0, Ordering::Relaxed);
            }
            inner.publish(&p, now);
            Ok(())
        })();
        match res {
            Ok(()) => failures = 0,
            Err(e) => {
                failures += 1;
                log::error(format!("Session engine loop error (#{failures}) | {e}"));
                inner.reset_all();
                sh.rescan.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis((500 * failures).min(10_000)));
            }
        }
        unsafe {
            WaitForSingleObject(sh.wake.0, if p.meters_enabled { 50 } else { 250 });
        }
    }
    inner.drain_commands();
    if sh.restore_on_stop.load(Ordering::Relaxed) {
        let (p, _) = sh.params();
        inner.restore_all(&p);
    }
    inner.reset_all();
    inner.enumerator = None;
    // Commands queued after the stop are completed (dropped senders report "not done").
    if let Ok(mut q) = sh.commands.lock() {
        q.clear();
    }
    log::info("Session engine stopped");
    sh.running.store(false, Ordering::Release);
}

/// Name of the mutex a DualMix helper process (diagnostics) holds so that its test tones are left alone.
pub fn auxiliary_mutex_name(pid: u32) -> String {
    format!(r"Local\DualMix.Aux.{pid}")
}

fn is_auxiliary_process(pid: u32) -> bool {
    let name = HSTRING::from(auxiliary_mutex_name(pid));
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &name) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

impl Inner {
    fn drain_commands(&mut self) {
        loop {
            let c = self.shared.commands.lock().ok().and_then(|mut q| q.pop_front());
            match c {
                Some(c) => c(self),
                None => break,
            }
        }
    }

    fn unroute_all(&mut self, extra: &[String]) {
        let mut keys: HashSet<String> = self.routed_keys.iter().cloned().collect();
        for k in extra {
            keys.insert(k.to_lowercase());
        }
        let mut done = HashSet::new();
        for (pid, dev) in &self.routed_pids {
            if !dev.is_empty() && done.insert(*pid) {
                app_routing::set_app_device(*pid, None);
            }
        }
        for key in &keys {
            for pid in process_info::pids_by_name(key) {
                if done.insert(pid) {
                    app_routing::set_app_device(pid, None);
                }
            }
        }
        self.routed_pids.clear();
        self.routed_keys.clear();
        self.route_blocked_gen = self.shared.params().1;
        let mut list: Vec<_> = keys.into_iter().collect();
        list.sort();
        log::info(format!("Unrouted {} processes ({})", done.len(), list.join(", ")));
    }

    // -------------------------------------------------------------------------------------- scan

    fn scan(&mut self, p: &SessionParams) -> Result<(), String> {
        let en = self.enumerator.clone().ok_or("no enumerator")?;
        let mut seen_devices = HashSet::new();
        for t in self.tracked.values_mut() {
            t.seen = false;
        }
        unsafe {
            let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).map_err(|e| format!("EnumAudioEndpoints failed: {e}"))?;
            let n = col.GetCount().unwrap_or(0);
            for d in 0..n {
                let Ok(dev) = col.Item(d) else { continue };
                let Ok(idp) = dev.GetId() else { continue };
                let id = com::take_pwstr(idp);
                seen_devices.insert(id.clone());
                if !self.devices.contains_key(&id) {
                    if let Some(h) = self.open_device(&id, dev) {
                        self.devices.insert(id, h);
                    }
                }
            }
        }
        // Devices that disappeared
        let gone: Vec<String> = self.devices.keys().filter(|k| !seen_devices.contains(*k)).cloned().collect();
        for id in gone {
            if let Some(h) = self.devices.remove(&id) {
                self.close_device(h);
            }
            self.dirty = true;
        }
        let ids: Vec<String> = self.devices.keys().cloned().collect();
        for id in ids {
            if let Err(e) = self.enumerate_sessions(p, &id) {
                let name = self.devices.get(&id).map(|h| h.name.clone()).unwrap_or_default();
                log::warn(format!("Session enumeration failed on {name} | {e}"));
                if let Some(h) = self.devices.remove(&id) {
                    self.close_device(h);
                }
            }
        }
        // Sessions that disappeared / expired
        let before = self.tracked.len();
        self.tracked.retain(|_, t| t.seen && t.state != AudioSessionStateExpired.0);
        if self.tracked.len() != before {
            self.dirty = true;
        }
        let alive: HashSet<u32> = self.tracked.values().map(|t| t.pid).collect();
        process_info::prune(&alive);
        self.routed_pids.retain(|pid, _| alive.contains(pid));
        Ok(())
    }

    fn open_device(&mut self, id: &str, dev: IMMDevice) -> Option<DeviceHolder> {
        unsafe {
            let mgr: IAudioSessionManager2 = match dev.Activate(CLSCTX_ALL, None) {
                Ok(m) => m,
                Err(_) => {
                    log::warn(format!("Cannot activate session manager on {id}"));
                    return None;
                }
            };
            let name = self.device_names.entry(id.to_string()).or_insert_with(|| devices::name_of(Some(id))).clone();
            // The session manager only starts sending notifications after the first GetSessionEnumerator call.
            let _ = mgr.GetSessionEnumerator();
            let registered = mgr.RegisterSessionNotification(&self.sink).is_ok();
            if !registered {
                log::warn(format!("RegisterSessionNotification failed on {name}"));
            }
            log::info(format!("Watching sessions on '{name}'"));
            Some(DeviceHolder { name, _device: dev, manager: mgr, registered })
        }
    }

    fn close_device(&self, h: DeviceHolder) {
        if h.registered {
            unsafe {
                let _ = h.manager.UnregisterSessionNotification(&self.sink);
            }
        }
        log::info(format!("Stopped watching '{}'", h.name));
    }

    fn enumerate_sessions(&mut self, p: &SessionParams, device_id: &str) -> windows::core::Result<()> {
        let (mgr, dev_name) = match self.devices.get(device_id) {
            Some(h) => (h.manager.clone(), h.name.clone()),
            None => return Ok(()),
        };
        unsafe {
            let sessions = mgr.GetSessionEnumerator()?;
            let n = sessions.GetCount()?;
            for i in 0..n {
                let Ok(s) = sessions.GetSession(i) else { continue };
                let Ok(s2) = s.cast::<IAudioSessionControl2>() else { continue };
                let _ = self.track_session(p, s2, device_id, &dev_name);
            }
        }
        Ok(())
    }

    fn track_session(&mut self, p: &SessionParams, s: IAudioSessionControl2, device_id: &str, dev_name: &str) -> windows::core::Result<()> {
        unsafe {
            let instance = com::take_pwstr(s.GetSessionInstanceIdentifier()?);
            if instance.is_empty() {
                return Ok(());
            }
            let state = s.GetState().map(|x| x.0).unwrap_or(0);
            if state == AudioSessionStateExpired.0 {
                return Ok(());
            }
            if let Some(t) = self.tracked.get_mut(&instance) {
                t.seen = true;
                if t.state != state {
                    self.dirty = true;
                }
                t.state = state;
                return Ok(());
            }
            let pid = s.GetProcessId().unwrap_or(0);
            let is_system = s.IsSystemSoundsSession().0 == 0;
            if pid == self.own_pid && !is_system {
                return Ok(());
            }
            if !is_system && is_auxiliary_process(pid) {
                return Ok(());
            }
            let (key, display, path) = if is_system {
                (SYSTEM_SOUNDS_KEY.to_string(), dualmix_core::loc::get("Apps.SystemSounds"), None)
            } else {
                let (name, path) = process_info::get(pid);
                let mut display = process_info::friendly_name(path.as_deref(), &name);
                if path.is_none() {
                    if let Ok(dn) = s.GetDisplayName() {
                        let dn = com::take_pwstr(dn);
                        if !dn.trim().is_empty() && !dn.starts_with('@') {
                            display = dn;
                        }
                    }
                }
                (name, display, path)
            };
            let volume: ISimpleAudioVolume = s.cast()?;
            let meter: Option<IAudioMeterInformation> = s.cast().ok();
            let lkey = key.to_lowercase();
            if !p.rules.has_rule(&key) && !self.learned.contains_key(&lkey) {
                // Inherit whatever level the user had set in the Windows mixer: never make anything louder.
                let current = volume.GetMasterVolume().map(|v| v as f64).unwrap_or(1.0);
                let current = (current.clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
                self.learned.insert(lkey.clone(), current);
                if let Ok(mut q) = self.shared.events.lock() {
                    q.push_back(EngineEvent::AppDiscovered { key: key.clone(), display_name: display.clone(), exe_path: path.clone(), volume: current });
                }
                log::info(format!("Discovered app '{key}' ({display}) on {dev_name}, volume {:.0}%", current * 100.0));
            }
            self.tracked.insert(
                instance,
                Tracked {
                    control: s,
                    volume,
                    meter,
                    pid,
                    key,
                    display_name: display,
                    exe_path: path,
                    is_system,
                    device_id: device_id.to_string(),
                    state,
                    last_set: -1.0,
                    touched: false,
                    peak: 0.0,
                    seen: true,
                },
            );
            self.dirty = true;
        }
        Ok(())
    }

    // -------------------------------------------------------------------------------------- apply

    fn relative_volume(&self, p: &SessionParams, key: &str) -> f64 {
        if p.rules.has_rule(key) {
            return p.rules.volume_of(key);
        }
        self.learned.get(&key.to_lowercase()).copied().unwrap_or(1.0)
    }

    fn apply(&mut self, p: &SessionParams) {
        let keys: Vec<String> = self.tracked.keys().cloned().collect();
        for k in keys {
            let (tkey, is_system) = {
                let t = &self.tracked[&k];
                (t.key.clone(), t.is_system)
            };
            let channel = p.rules.resolve(&tkey, is_system);
            let mut relative = self.relative_volume(p, &tkey);
            let mut adopted_event = None;
            {
                let t = self.tracked.get_mut(&k).unwrap();
                if channel == Channel::Ignore {
                    if t.touched {
                        try_set(t, relative as f32);
                        t.touched = false;
                        t.last_set = -1.0;
                    }
                    continue;
                }
                let Ok(current) = (unsafe { t.volume.GetMasterVolume() }) else { continue };
                // On a real device: app volume = channel gain × relative volume. On a virtual device: relative only.
                let on_bus = Some(&t.device_id) == p.chat_bus_id.as_ref() || Some(&t.device_id) == p.game_bus_id.as_ref();
                let gain = if on_bus { 1.0 } else { p.gains.for_channel(channel) };
                // The user changed the app volume in the Windows mixer: adopt it instead of fighting it.
                if t.touched && t.last_set >= 0.0 && (current - t.last_set).abs() > 0.015 && gain > 0.05 {
                    let adopted = ((current / gain).clamp(0.0, 1.0) as f64 * 100.0).round() / 100.0;
                    if (adopted - relative).abs() > 0.01 {
                        relative = adopted;
                        adopted_event = Some(adopted);
                    }
                }
                let target = (gain as f64 * relative).clamp(0.0, 1.0) as f32;
                if !t.touched || (current - target).abs() > 0.002 {
                    if try_set(t, target) {
                        t.last_set = target;
                        t.touched = true;
                    }
                } else {
                    t.last_set = target;
                }
            }
            if let Some(a) = adopted_event {
                self.learned.insert(tkey.to_lowercase(), a);
                if let Ok(mut q) = self.shared.events.lock() {
                    q.push_back(EngineEvent::AppVolumeAdopted { key: tkey.clone(), volume: a });
                }
                log::info(format!("Adopted Windows mixer volume for '{tkey}': {:.0}%", a * 100.0));
            }
        }
    }

    fn route(&mut self, p: &SessionParams) {
        let mut handled = HashSet::new();
        let list: Vec<(u32, String, bool)> = self.tracked.values().map(|t| (t.pid, t.key.clone(), t.is_system)).collect();
        for (pid, key, is_system) in list {
            if is_system || pid == 0 || !handled.insert(pid) {
                continue;
            }
            let channel = p.rules.resolve(&key, false);
            // None = the app follows the Windows default device.
            let target = match channel {
                Channel::Game => p.game_route_id.clone(),
                Channel::Chat => p.chat_route_id.clone(),
                _ => p.direct_route_id.clone(),
            };
            let routed_to = self.routed_pids.get(&pid).cloned();
            let Some(target) = target else {
                if routed_to.as_deref().is_some_and(|r| !r.is_empty()) {
                    app_routing::set_app_device(pid, None);
                    log::info(format!("Unrouted '{key}' (pid {pid}): follows the Windows default device"));
                    self.shared.rescan.store(true, Ordering::Relaxed);
                }
                self.routed_pids.insert(pid, String::new());
                continue;
            };
            if routed_to.as_deref() == Some(target.as_str()) {
                continue;
            }
            let Some(current) = app_routing::get_app_device(pid) else { continue }; // API unavailable
            let lkey = key.to_lowercase();
            if !current.eq_ignore_ascii_case(&target) {
                if app_routing::set_app_device(pid, Some(&target)) {
                    let label = match channel {
                        Channel::Game => "Game",
                        Channel::Chat => "Chat",
                        _ => "direct output",
                    };
                    log::info(format!("Routed '{key}' (pid {pid}) to {label}"));
                    if self.routed_keys.insert(lkey) {
                        self.push_event(EngineEvent::AppRouted { key: key.clone() });
                    }
                    self.shared.rescan.store(true, Ordering::Relaxed);
                }
            } else if self.routed_keys.insert(lkey) {
                self.push_event(EngineEvent::AppRouted { key: key.clone() });
            }
            self.routed_pids.insert(pid, target);
        }
    }

    fn push_event(&self, e: EngineEvent) {
        if let Ok(mut q) = self.shared.events.lock() {
            q.push_back(e);
        }
    }

    fn sample_meters(&mut self, p: &SessionParams) {
        let (mut game, mut chat) = (0f32, 0f32);
        for t in self.tracked.values_mut() {
            let mut peak = 0f32;
            if t.state == AudioSessionStateActive.0 {
                if let Some(m) = &t.meter {
                    peak = unsafe { m.GetPeakValue().unwrap_or(0.0) };
                }
            }
            let ch = p.rules.resolve(&t.key, t.is_system);
            // An app "playing" on the virtual device of the other channel is really capturing it.
            if (ch == Channel::Chat && Some(&t.device_id) == p.game_bus_id.as_ref()) || (ch == Channel::Game && Some(&t.device_id) == p.chat_bus_id.as_ref()) {
                peak = 0.0;
            }
            t.peak = peak;
            match ch {
                Channel::Game => game = game.max(peak),
                Channel::Chat => chat = chat.max(peak),
                _ => {}
            }
        }
        self.shared.game_peak.store(game.to_bits(), Ordering::Relaxed);
        self.shared.chat_peak.store(chat.to_bits(), Ordering::Relaxed);
    }

    // -------------------------------------------------------------------------------------- snapshot

    fn publish(&mut self, p: &SessionParams, now: i64) {
        if !self.dirty && (!p.meters_enabled || now - self.last_publish < 50) {
            return;
        }
        self.dirty = false;
        self.last_publish = now;
        let mut groups: Vec<(String, Vec<&Tracked>)> = Vec::new();
        for t in self.tracked.values() {
            let k = t.key.to_lowercase();
            match groups.iter_mut().find(|(gk, _)| *gk == k) {
                Some((_, v)) => v.push(t),
                None => groups.push((k, vec![t])),
            }
        }
        let mut list = Vec::with_capacity(groups.len());
        for (_, g) in groups {
            let first = g[0];
            let channel = p.rules.resolve(&first.key, first.is_system);
            let expected = match channel {
                Channel::Game => p.game_bus_id.as_ref(),
                Channel::Chat => p.chat_bus_id.as_ref(),
                _ => None,
            };
            let active = |x: &&Tracked| x.state == AudioSessionStateActive.0;
            let fixed_output = p.route_devices
                && expected.is_some()
                && !first.is_system
                && !g.iter().any(|x| active(x) && Some(&x.device_id) == expected)
                && g.iter().any(|x| active(x) && Some(&x.device_id) != p.game_bus_id.as_ref() && Some(&x.device_id) != p.chat_bus_id.as_ref());
            let mut devs: Vec<String> = Vec::new();
            for x in &g {
                let n = self.device_names.get(&x.device_id).cloned().unwrap_or_else(|| "?".into());
                if !devs.contains(&n) {
                    devs.push(n);
                }
            }
            list.push(AudioAppInfo {
                fixed_output,
                key: first.key.clone(),
                display_name: first.display_name.clone(),
                exe_path: g.iter().find_map(|x| x.exe_path.clone()),
                is_system_sounds: first.is_system,
                channel,
                auto_channel: p.rules.auto_channel(&first.key, first.is_system),
                is_explicit: p.rules.explicit_channel(&first.key).is_some(),
                relative_volume: self.relative_volume(p, &first.key),
                peak: g.iter().map(|x| x.peak).fold(0.0, f32::max),
                active: g.iter().any(active),
                devices: devs.join(", "),
            });
        }
        list.sort_by(|a, b| match (a.is_system_sounds, b.is_system_sounds) {
            (false, true) => std::cmp::Ordering::Less,
            (true, false) => std::cmp::Ordering::Greater,
            _ => a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()),
        });
        if let Ok(mut s) = self.shared.snapshot.lock() {
            *s = Arc::new(list);
        }
    }

    // -------------------------------------------------------------------------------------- teardown

    fn restore_all(&mut self, p: &SessionParams) {
        let mut n = 0;
        let vols: Vec<(String, f64)> = self.tracked.iter().map(|(k, t)| (k.clone(), self.relative_volume(p, &t.key))).collect();
        for (k, rel) in vols {
            let t = self.tracked.get_mut(&k).unwrap();
            if !t.touched {
                continue;
            }
            if try_set(t, rel as f32) {
                n += 1;
            }
            t.touched = false;
        }
        if n > 0 {
            log::info(format!("Restored {n} session volumes"));
        }
    }

    fn reset_all(&mut self) {
        self.tracked.clear();
        let devs: Vec<DeviceHolder> = self.devices.drain().map(|(_, v)| v).collect();
        for d in devs {
            self.close_device(d);
        }
        self.dirty = true;
    }
}

fn try_set(t: &Tracked, value: f32) -> bool {
    unsafe { t.volume.SetMasterVolume(value.clamp(0.0, 1.0), &GUID::zeroed()).is_ok() }
}

// keep the control alive alongside the volume interface
impl Tracked {
    #[allow(dead_code)]
    fn control(&self) -> &IAudioSessionControl2 {
        &self.control
    }
}
