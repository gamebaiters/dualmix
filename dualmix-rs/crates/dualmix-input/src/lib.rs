//! Keyboard input for the knob (port of C# `KeyboardHook` + `KnobController`), on its own thread with its own
//! message loop:
//! * **System hot keys** (`RegisterHotKey`) are the main mechanism: Windows hands the key to DualMix whatever window
//!   is in the foreground (also elevated ones), for the media keys of a knob, and the Windows volume flyout stays away.
//! * A **low-level hook** (`WH_KEYBOARD_LL`) observes the keys (key capture in the settings, the test panel) and
//!   steps in for the combinations Windows refused to register (already taken by another program).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use dualmix_core::knob::{self, KnobAction, KnobBinding, KnobConfig, VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP, build_bindings};
use dualmix_core::log;
use dualmix_core::settings::HotkeyModifiers;

/// A system-wide hot key to register.
#[derive(Debug, Clone, Copy)]
pub struct HotkeySpec {
    pub id: i32,
    pub vk: i32,
    pub mods: HotkeyModifiers,
    pub no_repeat: bool,
}

pub fn modifiers_to_win32(mods: HotkeyModifiers, no_repeat: bool) -> u32 {
    let mut flags = 0u32;
    if mods.contains(HotkeyModifiers::ALT) {
        flags |= 0x0001;
    }
    if mods.contains(HotkeyModifiers::CTRL) {
        flags |= 0x0002;
    }
    if mods.contains(HotkeyModifiers::SHIFT) {
        flags |= 0x0004;
    }
    if mods.contains(HotkeyModifiers::WIN) {
        flags |= 0x0008;
    }
    if no_repeat {
        flags |= 0x4000;
    }
    flags
}

fn pack(vk: i32, mods: HotkeyModifiers) -> i64 {
    ((mods.0 as i64) << 16) | (vk as i64 & 0xFFFF)
}

/// Key handler: (vk, down, injected) → true to swallow the key. Must be fast: runs inside the system input chain.
pub type KeyHandler = Arc<dyn Fn(i32, bool, bool) -> bool + Send + Sync>;
pub type HotkeyHandler = Arc<dyn Fn(i32) + Send + Sync>;

static HOOK_HANDLER: OnceLock<Mutex<Option<(KeyHandler, Arc<AtomicBool>)>>> = OnceLock::new();

fn hook_slot() -> &'static Mutex<Option<(KeyHandler, Arc<AtomicBool>)>> {
    HOOK_HANDLER.get_or_init(|| Mutex::new(None))
}

#[derive(Default)]
struct HotkeyState {
    wanted: Vec<HotkeySpec>,
    active: HashSet<i64>,
    failed: Vec<String>,
}

pub struct KeyboardHook {
    key_handler: KeyHandler,
    hotkey_handler: HotkeyHandler,
    thread_id: AtomicU32,
    hook_installed: AtomicBool,
    state: Mutex<HotkeyState>,
    /// Treat keys sent by other programs (AutoHotkey, macro tools, remote desktop) like real key presses.
    pub accept_injected: Arc<AtomicBool>,
    applied: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

const WM_APPLY_HOTKEYS: u32 = 0x8001; // WM_APP + 1

impl KeyboardHook {
    pub fn new(key_handler: KeyHandler, hotkey_handler: HotkeyHandler) -> Arc<Self> {
        Arc::new(Self {
            key_handler,
            hotkey_handler,
            thread_id: AtomicU32::new(0),
            hook_installed: AtomicBool::new(false),
            state: Mutex::new(HotkeyState::default()),
            accept_injected: Arc::new(AtomicBool::new(false)),
            applied: Mutex::new(None),
            join: Mutex::new(None),
        })
    }

    /// Called (keyboard thread) after the hot keys were (re)registered.
    pub fn on_hotkeys_applied(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.applied.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    pub fn is_installed(&self) -> bool {
        self.hook_installed.load(Ordering::Relaxed)
    }

    pub fn failed_hotkeys(&self) -> Vec<String> {
        self.state.lock().map(|s| s.failed.clone()).unwrap_or_default()
    }

    pub fn active_hotkeys(&self) -> usize {
        self.state.lock().map(|s| s.active.len()).unwrap_or(0)
    }

    /// True when Windows delivers this combination as a hot key (the hook must then leave it alone).
    pub fn is_hotkey_active(&self, vk: i32, mods: HotkeyModifiers) -> bool {
        self.state.lock().map(|s| s.active.contains(&pack(vk, mods))).unwrap_or(false)
    }

    /// Replaces the set of system hot keys. Thread safe; applied on the keyboard thread.
    pub fn set_hotkeys(&self, specs: Vec<HotkeySpec>) {
        if let Ok(mut s) = self.state.lock() {
            s.wanted = specs;
        }
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            #[cfg(windows)]
            unsafe {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(id, WM_APPLY_HOTKEYS, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn is_down(vk: i32) -> bool {
        #[cfg(windows)]
        unsafe {
            (windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) as u16 & 0x8000) != 0
        }
        #[cfg(not(windows))]
        {
            let _ = vk;
            false
        }
    }

    pub fn current_modifiers() -> HotkeyModifiers {
        let mut m = HotkeyModifiers::NONE;
        if Self::is_down(0x11) {
            m = m.union(HotkeyModifiers::CTRL);
        }
        if Self::is_down(0x12) {
            m = m.union(HotkeyModifiers::ALT);
        }
        if Self::is_down(0x10) {
            m = m.union(HotkeyModifiers::SHIFT);
        }
        if Self::is_down(0x5B) || Self::is_down(0x5C) {
            m = m.union(HotkeyModifiers::WIN);
        }
        m
    }

    #[cfg(windows)]
    pub fn start(self: &Arc<Self>) {
        use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        use windows::Win32::UI::Input::KeyboardAndMouse::{HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey};
        use windows::Win32::UI::WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW, SetWindowsHookExW,
            TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_HOTKEY,
        };

        let mut join = self.join.lock().unwrap_or_else(|e| e.into_inner());
        if join.is_some() {
            return;
        }
        *hook_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some((Arc::clone(&self.key_handler), Arc::clone(&self.accept_injected)));

        unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            unsafe {
                if code >= 0 {
                    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                    let msg = wparam.0 as u32;
                    let down = msg == 0x0100 || msg == 0x0104; // WM_KEYDOWN / WM_SYSKEYDOWN
                    let handler = hook_slot().lock().ok().and_then(|g| g.clone());
                    if let Some((h, accept)) = handler {
                        let injected = (kb.flags.0 & 0x10) != 0 && !accept.load(Ordering::Relaxed); // LLKHF_INJECTED
                        if h(kb.vkCode as i32, down, injected) {
                            return LRESULT(1);
                        }
                    }
                }
                CallNextHookEx(None, code, wparam, lparam)
            }
        }

        let me = Arc::clone(self);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("DualMix.Keyboard".into())
            .spawn(move || unsafe {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                // Create the message queue before anyone can post to this thread.
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let hinst = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default();
                match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), hinst, 0) {
                    Ok(h) => {
                        me.hook_installed.store(true, Ordering::Relaxed);
                        log::info("Keyboard hook installed");
                        me.thread_id.store(GetCurrentThreadId(), Ordering::Release);
                        let mut registered: Vec<i32> = Vec::new();
                        let apply = |registered: &mut Vec<i32>| {
                            for id in registered.drain(..) {
                                let _ = UnregisterHotKey(None, id);
                            }
                            let wanted = me.state.lock().map(|s| s.wanted.clone()).unwrap_or_default();
                            let mut active = HashSet::new();
                            let mut failed = Vec::new();
                            for h in &wanted {
                                let flags = HOT_KEY_MODIFIERS(modifiers_to_win32(h.mods, h.no_repeat));
                                if RegisterHotKey(None, h.id, flags, h.vk as u32).is_ok() {
                                    registered.push(h.id);
                                    active.insert(pack(h.vk, h.mods));
                                } else {
                                    failed.push(knob::chord_name(h.vk, h.mods));
                                }
                            }
                            let n = active.len();
                            if let Ok(mut s) = me.state.lock() {
                                s.active = active;
                                s.failed = failed.clone();
                            }
                            log::info(format!(
                                "Hot keys: {n} of {} registered{}",
                                wanted.len(),
                                if failed.is_empty() { String::new() } else { format!(" (taken by another program, handled by the hook: {})", failed.join(", ")) }
                            ));
                            let cb = me.applied.lock().ok().and_then(|g| g.clone());
                            if let Some(cb) = cb {
                                cb();
                            }
                        };
                        apply(&mut registered);
                        let _ = tx.send(());
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            if msg.message == WM_HOTKEY {
                                (me.hotkey_handler)(msg.wParam.0 as i32);
                                continue;
                            }
                            if msg.message == WM_APPLY_HOTKEYS {
                                apply(&mut registered);
                                continue;
                            }
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        for id in registered {
                            let _ = UnregisterHotKey(None, id);
                        }
                        if let Ok(mut s) = me.state.lock() {
                            s.active.clear();
                        }
                        let _ = UnhookWindowsHookEx(h);
                        me.hook_installed.store(false, Ordering::Relaxed);
                        log::info("Keyboard hook removed");
                    }
                    Err(e) => {
                        log::error(format!("SetWindowsHookEx failed | {e}"));
                        me.thread_id.store(GetCurrentThreadId(), Ordering::Release);
                        let _ = tx.send(());
                    }
                }
                me.thread_id.store(0, Ordering::Release);
            })
            .expect("spawn DualMix.Keyboard");
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        *join = Some(handle);
    }

    #[cfg(not(windows))]
    pub fn start(self: &Arc<Self>) {}

    pub fn stop(&self) {
        let id = self.thread_id.swap(0, Ordering::AcqRel);
        #[cfg(windows)]
        if id != 0 {
            unsafe {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(id, windows::Win32::UI::WindowsAndMessaging::WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        let _ = id;
        if let Some(j) = self.join.lock().ok().and_then(|mut g| g.take()) {
            let _ = j.join();
        }
        if let Ok(mut g) = hook_slot().lock() {
            *g = None;
        }
    }
}

// ------------------------------------------------------------------------------------------ knob controller

pub type Dispatch = Arc<dyn Fn(KnobAction) + Send + Sync>;
pub type CaptureFn = Box<dyn FnOnce(i32, HotkeyModifiers) + Send>;

/// Decides what each key of the knob does (port of C# `KnobController`).
pub struct KnobController {
    dispatch: Dispatch,
    config: RwLock<KnobConfig>,
    bindings: RwLock<Vec<KnobBinding>>,
    swallowed_down: Mutex<HashSet<i32>>,
    keyboard: RwLock<Option<Arc<KeyboardHook>>>,
    logged: AtomicUsize,
    last_handled: AtomicI64,
    /// When set, the next non-modifier key (with its modifiers) is reported here instead of being processed.
    capture_next: Mutex<Option<CaptureFn>>,
    /// Called (keyboard thread) for every knob key, used by the test panel.
    key_seen: Mutex<Option<Arc<dyn Fn(String) + Send + Sync>>>,
}

impl KnobController {
    pub fn new(config: KnobConfig, dispatch: Dispatch) -> Arc<Self> {
        let b = build_bindings(&config);
        Arc::new(Self {
            dispatch,
            config: RwLock::new(config),
            bindings: RwLock::new(b),
            swallowed_down: Mutex::new(HashSet::new()),
            keyboard: RwLock::new(None),
            logged: AtomicUsize::new(0),
            last_handled: AtomicI64::new(0),
            capture_next: Mutex::new(None),
            key_seen: Mutex::new(None),
        })
    }

    pub fn bindings(&self) -> Vec<KnobBinding> {
        self.bindings.read().map(|b| b.clone()).unwrap_or_default()
    }

    pub fn config(&self) -> KnobConfig {
        *self.config.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn last_handled_tick(&self) -> i64 {
        self.last_handled.load(Ordering::Relaxed)
    }

    pub fn set_capture_next(&self, f: Option<CaptureFn>) {
        *self.capture_next.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    pub fn set_key_seen(&self, f: Option<Arc<dyn Fn(String) + Send + Sync>>) {
        *self.key_seen.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    /// Connects the keyboard thread that registers the hot keys.
    pub fn attach(&self, keyboard: Arc<KeyboardHook>) {
        *self.keyboard.write().unwrap_or_else(|e| e.into_inner()) = Some(keyboard);
        self.apply();
    }

    pub fn update_config(&self, config: KnobConfig) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
        *self.bindings.write().unwrap_or_else(|e| e.into_inner()) = build_bindings(&config);
        self.apply();
    }

    fn apply(&self) {
        let specs: Vec<HotkeySpec> = self
            .bindings()
            .iter()
            .enumerate()
            .map(|(i, b)| HotkeySpec { id: i as i32 + 1, vk: b.vk, mods: b.mods, no_repeat: b.no_repeat })
            .collect();
        if let Some(k) = self.keyboard.read().ok().and_then(|g| g.clone()) {
            k.set_hotkeys(specs);
        }
    }

    fn now_tick() -> i64 {
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_millis() as i64
    }

    /// A registered system hot key was pressed (keyboard thread).
    pub fn handle_hotkey(&self, id: i32) {
        let b = self.bindings();
        if id < 1 || id as usize > b.len() {
            return;
        }
        let binding = b[id as usize - 1];
        self.last_handled.store(Self::now_tick(), Ordering::Relaxed);
        self.note(binding.vk, binding.mods, &dualmix_core::loc::current().pick("system hot key", "tasto di sistema").to_string());
        (self.dispatch)(binding.action);
    }

    /// Low-level hook (keyboard thread). Returns true to swallow the key.
    pub fn handle_key(&self, vk: i32, down: bool, injected: bool) -> bool {
        // Ignore synthetic input (other tools) unless the user asked for it.
        if injected {
            return false;
        }
        if down && !knob::is_modifier_key(vk) {
            let cap = self.capture_next.lock().ok().and_then(|mut g| g.take());
            if let Some(cap) = cap {
                if let Ok(mut s) = self.swallowed_down.lock() {
                    s.insert(vk);
                }
                cap(vk, KeyboardHook::current_modifiers());
                return true;
            }
        }
        // A key whose "down" we swallowed must have its "up" swallowed as well.
        if !down {
            return self.swallowed_down.lock().map(|mut s| s.remove(&vk)).unwrap_or(false);
        }
        let bindings = self.bindings();
        let knob_key = matches!(vk, VK_VOLUME_UP | VK_VOLUME_DOWN | VK_VOLUME_MUTE);
        let mods = if knob_key || !bindings.is_empty() { KeyboardHook::current_modifiers() } else { HotkeyModifiers::NONE };
        let Some(m) = bindings.iter().find(|b| b.vk == vk && b.mods == mods).copied() else {
            if knob_key {
                self.note(vk, mods, &dualmix_core::loc::get("Knob.Key.LeftToWin"));
            }
            return false;
        };
        // Windows delivers it to us as a hot key a moment later: nothing to do here.
        if let Some(k) = self.keyboard.read().ok().and_then(|g| g.clone()) {
            if k.is_hotkey_active(vk, mods) {
                return false;
            }
        }
        // Not registered (taken by another program, or no keyboard thread): handle it from the hook.
        self.last_handled.store(Self::now_tick(), Ordering::Relaxed);
        self.note(vk, mods, &dualmix_core::loc::get("Knob.Key.Hooked"));
        if let Ok(mut s) = self.swallowed_down.lock() {
            s.insert(vk);
        }
        (self.dispatch)(m.action);
        true
    }

    fn note(&self, vk: i32, mods: HotkeyModifiers, how: &str) {
        use dualmix_core::loc::get;
        let mut what = match vk {
            VK_VOLUME_UP => get("Knob.Key.VolUp"),
            VK_VOLUME_DOWN => get("Knob.Key.VolDown"),
            VK_VOLUME_MUTE => get("Knob.Key.VolMute"),
            _ => knob::key_name(vk),
        };
        if mods != HotkeyModifiers::NONE {
            what = format!("{} + {what}", knob::chord_name(0, mods));
        }
        // The first events of every run go to the log: they show how the keyboard really reaches DualMix.
        if self.logged.fetch_add(1, Ordering::Relaxed) < 12 {
            log::info(format!("Key: {what} ({how})"));
        }
        if let Some(f) = self.key_seen.lock().ok().and_then(|g| g.clone()) {
            f(format!("{what} · {how}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dualmix_core::settings::{KnobTarget, Settings};

    #[test]
    fn bindings_and_hook_fallback() {
        let mut s = Settings::default();
        s.knob_target = KnobTarget::Mix;
        let got = Arc::new(Mutex::new(Vec::new()));
        let g2 = Arc::clone(&got);
        let k = KnobController::new(KnobConfig::from_settings(&s), Arc::new(move |a| g2.lock().unwrap().push(a)));
        assert!(k.bindings().iter().any(|b| b.vk == VK_VOLUME_UP && b.mods == HotkeyModifiers::NONE));
        // No keyboard thread: the hook handles the binding itself.
        assert!(k.handle_key(VK_VOLUME_UP, true, false));
        assert!(k.handle_key(VK_VOLUME_UP, false, false));
        assert_eq!(got.lock().unwrap().len(), 1);
        assert!(!k.handle_key(VK_VOLUME_UP, true, true)); // injected ignored
        k.handle_hotkey(1);
        assert_eq!(got.lock().unwrap().len(), 2);
    }
}
