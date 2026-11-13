//! Dedicated keyboard input thread (`RegisterHotKey` + low-level `WH_KEYBOARD_LL` fallback).

use std::sync::{Arc, Mutex};
use dualmix_core::knob::{KnobAction, KnobConfig, KnobState};
use dualmix_core::settings::HotkeyModifiers;

#[derive(Debug, Clone, Copy)]
pub struct HotkeySpec {
    pub id: i32,
    pub vk: u32,
    pub mods: HotkeyModifiers,
    pub no_repeat: bool,
}

pub fn modifiers_to_win32(mods: HotkeyModifiers, no_repeat: bool) -> u32 {
    let mut flags = 0u32;
    if mods.contains(HotkeyModifiers::CTRL) { flags |= 0x0002; } // MOD_CONTROL
    if mods.contains(HotkeyModifiers::ALT) { flags |= 0x0001; }  // MOD_ALT
    if mods.contains(HotkeyModifiers::SHIFT) { flags |= 0x0004; } // MOD_SHIFT
    if mods.contains(HotkeyModifiers::WIN) { flags |= 0x0008; }  // MOD_WIN
    if no_repeat { flags |= 0x4000; }                            // MOD_NOREPEAT
    flags
}

pub struct KeyboardEngine {
    pub state: Arc<Mutex<KnobState>>,
    #[cfg(windows)]
    thread_id: std::sync::atomic::AtomicU32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl KeyboardEngine {
    pub fn new(config: KnobConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(KnobState::new(config))),
            #[cfg(windows)]
            thread_id: std::sync::atomic::AtomicU32::new(0),
            join: None,
        }
    }

    #[cfg(windows)]
    pub fn start(&mut self, on_action: Arc<dyn Fn(KnobAction) + Send + Sync>) {
        use std::sync::atomic::Ordering;
        use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, MSG, PeekMessageW, PM_NOREMOVE,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            KBDLLHOOKSTRUCT, WH_KEYBOARD_LL, WM_APP, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP,
            WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
        };

        if self.join.is_some() {
            return;
        }
        let state = Arc::clone(&self.state);
        let tid_out = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tid_clone = Arc::clone(&tid_out);

        let handle = std::thread::Builder::new()
            .name("DualMix.Keyboard".into())
            .spawn(move || {
                // Static hook state for the thread-local LL hook.
                thread_local! {
                    static HOOK_STATE: std::cell::RefCell<Option<(Arc<Mutex<KnobState>>, Arc<dyn Fn(KnobAction) + Send + Sync>)>> =
                        const { std::cell::RefCell::new(None) };
                }

                unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
                    unsafe {
                        if code >= 0 {
                            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                            let msg = wparam.0 as u32;
                            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                            let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                            if down || up {
                                let injected = (kb.flags.0 & 0x10) != 0; // LLKHF_INJECTED
                                let mut mods = HotkeyModifiers::NONE;
                                if (GetAsyncKeyState(0x11) as u16 & 0x8000) != 0 { mods = mods.union(HotkeyModifiers::CTRL); }
                                if (GetAsyncKeyState(0x12) as u16 & 0x8000) != 0 { mods = mods.union(HotkeyModifiers::ALT); }
                                if (GetAsyncKeyState(0x10) as u16 & 0x8000) != 0 { mods = mods.union(HotkeyModifiers::SHIFT); }

                                let mut swallow = false;
                                let mut fired = None;
                                HOOK_STATE.with(|cell| {
                                    if let Some((st, _cb)) = cell.borrow().as_ref() {
                                        if let Ok(mut guard) = st.lock() {
                                            let (s, a) = guard.handle_key(kb.vkCode as i32, down, injected, false, mods);
                                            swallow = s;
                                            fired = a;
                                        }
                                    }
                                });
                                if let Some(act) = fired {
                                    HOOK_STATE.with(|cell| {
                                        if let Some((_, cb)) = cell.borrow().as_ref() {
                                            cb(act);
                                        }
                                    });
                                }
                                if swallow {
                                    return LRESULT(1);
                                }
                            }
                        }
                        CallNextHookEx(None, code, wparam, lparam)
                    }
                }

                HOOK_STATE.with(|c| *c.borrow_mut() = Some((Arc::clone(&state), Arc::clone(&on_action))));
                unsafe {
                    let mut msg = MSG::default();
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    tid_clone.store(GetCurrentThreadId(), Ordering::Release);

                    let hinst = GetModuleHandleW(None)
                        .map(|m| HINSTANCE(m.0))
                        .unwrap_or_default();
                    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), hinst, 0).ok();

                    // Register initial bindings
                    let mut active_ids = Vec::<i32>::new();
                    let mut active_pairs = Vec::<(i32, HotkeyModifiers)>::new();
                    if let Ok(guard) = state.lock() {
                        for (idx, b) in guard.bindings().iter().enumerate() {
                            let id = (idx + 1) as i32;
                            let flags = HOT_KEY_MODIFIERS(modifiers_to_win32(b.mods, b.no_repeat));
                            if RegisterHotKey(None, id, flags, b.vk as u32).is_ok() {
                                active_ids.push(id);
                                active_pairs.push((b.vk, b.mods));
                            }
                        }
                    }
                    if let Ok(mut guard) = state.lock() {
                        guard.set_active_hotkeys(active_pairs);
                    }

                    const WM_RELOAD_HOTKEYS: u32 = WM_APP + 1;
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        if msg.message == WM_HOTKEY {
                            let id = msg.wParam.0 as i32;
                            let act = state.lock().ok().and_then(|g| g.handle_hotkey(id));
                            if let Some(a) = act {
                                on_action(a);
                            }
                        } else if msg.message == WM_RELOAD_HOTKEYS {
                            for id in active_ids.drain(..) {
                                let _ = UnregisterHotKey(None, id);
                            }
                        } else if msg.message == WM_QUIT {
                            break;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    for id in active_ids {
                        let _ = UnregisterHotKey(None, id);
                    }
                    if let Some(h) = hook {
                        let _ = UnhookWindowsHookEx(h);
                    }
                }
            })
            .expect("spawn DualMix.Keyboard");

        // Wait briefly for thread ID publication.
        for _ in 0..50 {
            let tid = tid_out.load(Ordering::Acquire);
            if tid != 0 {
                self.thread_id.store(tid, Ordering::Release);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.join = Some(handle);
    }

    pub fn stop(&mut self) {
        #[cfg(windows)]
        {
            let tid = self.thread_id.swap(0, std::sync::atomic::Ordering::AcqRel);
            if tid != 0 {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                        tid,
                        windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                        windows::Win32::Foundation::WPARAM(0),
                        windows::Win32::Foundation::LPARAM(0),
                    );
                }
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for KeyboardEngine {
    fn drop(&mut self) {
        self.stop();
    }
}
