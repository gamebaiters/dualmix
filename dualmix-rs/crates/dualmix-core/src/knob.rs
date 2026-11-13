//! Knob controller state machine & chord matching (1:1 with `DualMix.Input.KnobController`).

use crate::settings::{HotkeyModifiers, KnobTarget, Settings};

pub const VK_VOLUME_MUTE: i32 = 0xAD;
pub const VK_VOLUME_DOWN: i32 = 0xAE;
pub const VK_VOLUME_UP: i32 = 0xAF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnobActionKind {
    Rotate,
    Press,
    CycleFunction,
    MixTowardsGame,
    MixTowardsChat,
    MixReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnobAction {
    pub kind: KnobActionKind,
    pub direction: i32,
    pub target: KnobTarget,
}

impl KnobAction {
    pub const fn rotate(direction: i32, target: KnobTarget) -> Self {
        Self { kind: KnobActionKind::Rotate, direction, target }
    }
    pub const fn press(target: KnobTarget) -> Self {
        Self { kind: KnobActionKind::Press, direction: 0, target }
    }
    pub const fn simple(kind: KnobActionKind) -> Self {
        Self { kind, direction: 0, target: KnobTarget::Mix }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobConfig {
    pub enabled: bool,
    pub target: KnobTarget,
    pub ctrl_alternate: bool,
    pub ctrl_press_cycles: bool,
    pub double_press_cycles: bool,
    pub hotkey_key: i32,
    pub hotkey_modifiers: HotkeyModifiers,
    pub key_game: i32,
    pub key_chat: i32,
    pub key_reset: i32,
}

impl KnobConfig {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            enabled: s.knob_enabled,
            target: s.knob_target,
            ctrl_alternate: s.knob_ctrl_alternate,
            ctrl_press_cycles: s.knob_ctrl_press_cycles,
            double_press_cycles: s.knob_double_press_cycles,
            hotkey_key: s.knob_hotkey_key,
            hotkey_modifiers: s.knob_hotkey_modifiers,
            key_game: s.key_mix_game,
            key_chat: s.key_mix_chat,
            key_reset: s.key_mix_reset,
        }
    }

    #[inline]
    pub const fn alternate(&self) -> KnobTarget {
        match self.target {
            KnobTarget::Mix => KnobTarget::Volume,
            _ => KnobTarget::Mix,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnobBinding {
    pub vk: i32,
    pub mods: HotkeyModifiers,
    pub action: KnobAction,
    pub no_repeat: bool,
}

pub fn build_bindings(cfg: &KnobConfig) -> Vec<KnobBinding> {
    let mut list = Vec::with_capacity(8);
    let mut add = |vk: i32, mods: HotkeyModifiers, action: KnobAction, no_repeat: bool| {
        if vk == 0 || list.iter().any(|b: &KnobBinding| b.vk == vk && b.mods == mods) {
            return;
        }
        list.push(KnobBinding { vk, mods, action, no_repeat });
    };

    if cfg.enabled {
        let main = cfg.target;
        if main != KnobTarget::Volume {
            add(VK_VOLUME_UP, HotkeyModifiers::NONE, KnobAction::rotate(1, main), false);
            add(VK_VOLUME_DOWN, HotkeyModifiers::NONE, KnobAction::rotate(-1, main), false);
            add(VK_VOLUME_MUTE, HotkeyModifiers::NONE, KnobAction::press(main), true);
        }
        if cfg.ctrl_alternate && cfg.alternate() != KnobTarget::Volume {
            add(VK_VOLUME_UP, HotkeyModifiers::CTRL, KnobAction::rotate(1, cfg.alternate()), false);
            add(VK_VOLUME_DOWN, HotkeyModifiers::CTRL, KnobAction::rotate(-1, cfg.alternate()), false);
        }
        if cfg.ctrl_press_cycles {
            add(VK_VOLUME_MUTE, HotkeyModifiers::CTRL, KnobAction::simple(KnobActionKind::CycleFunction), true);
        }
        add(cfg.hotkey_key, cfg.hotkey_modifiers, KnobAction::simple(KnobActionKind::CycleFunction), true);
    }

    add(cfg.key_game, HotkeyModifiers::NONE, KnobAction::simple(KnobActionKind::MixTowardsGame), false);
    add(cfg.key_chat, HotkeyModifiers::NONE, KnobAction::simple(KnobActionKind::MixTowardsChat), false);
    add(cfg.key_reset, HotkeyModifiers::NONE, KnobAction::simple(KnobActionKind::MixReset), true);
    list
}

/// Pure key-event state machine (unit-testable independently of Win32 hooks).
#[derive(Debug)]
pub struct KnobState {
    config: KnobConfig,
    bindings: Vec<KnobBinding>,
    swallowed_down: Vec<i32>,
    active_hotkey_ids: Vec<(i32, HotkeyModifiers)>,
}

impl KnobState {
    pub fn new(config: KnobConfig) -> Self {
        let bindings = build_bindings(&config);
        Self {
            config,
            bindings,
            swallowed_down: Vec::new(),
            active_hotkey_ids: Vec::new(),
        }
    }

    pub fn update_config(&mut self, config: KnobConfig) {
        self.config = config;
        self.bindings = build_bindings(&self.config);
    }

    pub fn bindings(&self) -> &[KnobBinding] {
        &self.bindings
    }

    pub fn set_active_hotkeys(&mut self, active: Vec<(i32, HotkeyModifiers)>) {
        self.active_hotkey_ids = active;
    }

    pub fn handle_hotkey(&self, id: i32) -> Option<KnobAction> {
        if id < 1 {
            return None;
        }
        self.bindings.get((id - 1) as usize).map(|b| b.action)
    }

    /// Evaluates a raw key transition. Returns `(swallow, optional_action)`.
    pub fn handle_key(
        &mut self,
        vk: i32,
        down: bool,
        injected: bool,
        accept_injected: bool,
        mods: HotkeyModifiers,
    ) -> (bool, Option<KnobAction>) {
        if injected && !accept_injected {
            return (false, None);
        }
        if !down {
            if let Some(pos) = self.swallowed_down.iter().position(|&k| k == vk) {
                self.swallowed_down.swap_remove(pos);
                return (true, None);
            }
            return (false, None);
        }
        // Let registered system hotkeys handle what Windows owns.
        if self.active_hotkey_ids.iter().any(|&(v, m)| v == vk && m == mods) {
            return (false, None);
        }
        if let Some(b) = self.bindings.iter().find(|b| b.vk == vk && b.mods == mods) {
            if !self.swallowed_down.contains(&vk) {
                self.swallowed_down.push(vk);
            }
            return (true, Some(b.action));
        }
        (false, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_csharp_knob_diagnostics() {
        let mut s = Settings::default();
        assert_eq!(s.knob_start_function, crate::settings::KnobStartFunction::Volume);
        assert!(s.knob_mix_press_mutes);

        // Mix mode binds Volume Up/Down/Mute
        s.knob_target = KnobTarget::Mix;
        let mut state = KnobState::new(KnobConfig::from_settings(&s));
        let (swallow, act) = state.handle_key(VK_VOLUME_UP, true, false, false, HotkeyModifiers::NONE);
        assert!(swallow);
        assert_eq!(act.unwrap().target, KnobTarget::Mix);
        assert!(state.handle_key(VK_VOLUME_UP, false, false, false, HotkeyModifiers::NONE).0);

        // Switch to Volume: rotation/press left to Windows, Ctrl+turn drives Mix
        s.knob_target = KnobTarget::Volume;
        state.update_config(KnobConfig::from_settings(&s));
        let (swallow_vol, _) = state.handle_key(VK_VOLUME_UP, true, false, false, HotkeyModifiers::NONE);
        assert!(!swallow_vol);
        assert!(state.bindings().iter().any(|b| {
            b.vk == VK_VOLUME_UP
                && b.mods == HotkeyModifiers::CTRL
                && b.action == KnobAction::rotate(1, KnobTarget::Mix)
        }));

        // Dedicated key works even when knob is disabled
        s.knob_enabled = false;
        s.key_mix_game = 0x7C; // F13
        state.update_config(KnobConfig::from_settings(&s));
        assert!(!state.handle_key(VK_VOLUME_MUTE, true, false, false, HotkeyModifiers::NONE).0);
        let (swallow_f13, act_f13) = state.handle_key(0x7C, true, false, false, HotkeyModifiers::NONE);
        assert!(swallow_f13);
        assert_eq!(act_f13.unwrap().kind, KnobActionKind::MixTowardsGame);
        assert!(!state.handle_key(0x41, true, false, false, HotkeyModifiers::NONE).0);
    }
}
