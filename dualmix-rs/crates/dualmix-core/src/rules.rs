//! Built-in voice/chat process catalog and case-insensitive rule resolution.

use std::collections::{HashMap, HashSet};
use crate::settings::{Channel, Settings};

pub const SYSTEM_SOUNDS_KEY: &str = "#system";

pub const CHAT_PROCESSES: &[&str] = &[
    // Discord and alternative clients
    "discord", "discordptb", "discordcanary", "discorddevelopment", "vesktop", "webcord",
    "legcord", "armcord", "ripcord",
    // TeamSpeak 3 / 5 / 6
    "ts3client_win64", "ts3client_win32", "teamspeak", "teamspeak3", "teamspeak5", "teamspeak6",
    // Other voice apps
    "mumble", "skype", "skypeapp", "lync", "ms-teams", "teams", "zoom", "slack", "signal",
    "whatsapp", "whatsapp.root", "telegram", "element", "guilded", "ventrilo", "raidcall",
    "revolt", "webex", "ciscowebexstart", "jitsi meet",
];

/// Trims whitespace and strips a trailing `.exe` (case-insensitive), matching C# `AppCatalog.Normalize`.
pub fn normalize_process(name: &str) -> String {
    let trimmed = name.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.ends_with(".exe") {
        trimmed[..trimmed.len() - 4].to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn is_built_in_chat(process_name: &str) -> bool {
    let norm = normalize_process(process_name).to_ascii_lowercase();
    CHAT_PROCESSES.iter().any(|&p| p == norm)
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    rules: HashMap<String, (Option<Channel>, f64)>,
    extra_chat: HashSet<String>,
    pub default_channel: Channel,
    pub system_sounds_channel: Channel,
}

impl RuleSet {
    pub fn from_settings(s: &Settings) -> Self {
        let mut rules = HashMap::with_capacity(s.apps.len());
        for (k, v) in &s.apps {
            rules.insert(k.to_ascii_lowercase(), (v.channel, v.volume));
        }
        let extra_chat = s
            .extra_chat_apps
            .iter()
            .map(|e| normalize_process(e).to_ascii_lowercase())
            .collect();
        Self {
            rules,
            extra_chat,
            default_channel: s.default_channel,
            system_sounds_channel: s.system_sounds_channel,
        }
    }

    pub fn has_rule(&self, key: &str) -> bool {
        self.rules.contains_key(&key.to_ascii_lowercase())
    }

    pub fn auto_channel(&self, process_name: &str, is_system_sounds: bool) -> Channel {
        if is_system_sounds {
            return self.system_sounds_channel;
        }
        let lower = normalize_process(process_name).to_ascii_lowercase();
        if CHAT_PROCESSES.iter().any(|&c| c == lower) || self.extra_chat.contains(&lower) {
            Channel::Chat
        } else {
            self.default_channel
        }
    }

    pub fn resolve(&self, key: &str, is_system_sounds: bool) -> Channel {
        if let Some((Some(ch), _)) = self.rules.get(&key.to_ascii_lowercase()) {
            return *ch;
        }
        self.auto_channel(key, is_system_sounds)
    }

    pub fn explicit_channel(&self, key: &str) -> Option<Channel> {
        self.rules.get(&key.to_ascii_lowercase()).and_then(|r| r.0)
    }

    pub fn volume_of(&self, key: &str) -> f64 {
        self.rules
            .get(&key.to_ascii_lowercase())
            .map(|r| r.1)
            .unwrap_or(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::AppRule;

    #[test]
    fn resolves_builtin_extra_and_explicit_rules() {
        let mut s = Settings::default();
        s.extra_chat_apps.push("myvoice.exe".into());
        s.apps.insert(
            "discord".into(),
            AppRule {
                channel: Some(Channel::Game),
                volume: 0.5,
                ..Default::default()
            },
        );
        let rs = RuleSet::from_settings(&s);
        assert_eq!(rs.auto_channel("Discord", false), Channel::Chat);
        assert_eq!(rs.auto_channel("myvoice", false), Channel::Chat);
        assert_eq!(rs.auto_channel("cyberpunk2077", false), Channel::Game);
        assert_eq!(rs.resolve("Discord", false), Channel::Game);
        assert!((rs.volume_of("DISCORD") - 0.5).abs() < 1e-9);
        assert_eq!(rs.resolve(SYSTEM_SOUNDS_KEY, true), Channel::Game);
    }
}
