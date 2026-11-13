//! Localization (English default; automatic Italian on Italian Windows UI language).
//! The string table is the one of the C# app (`Core/Loc.cs`, 478 keys), shared with the WebView2 UI as JSON.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::settings::LanguagePreference;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    It,
}

impl Lang {
    pub fn resolve(pref: LanguagePreference, cli_override: Option<&str>) -> Self {
        if let Some(ov) = cli_override {
            let low = ov.trim().to_ascii_lowercase();
            if low.starts_with("it") {
                return Self::It;
            }
            if low.starts_with("en") {
                return Self::En;
            }
        }
        match pref {
            LanguagePreference::Italian => Self::It,
            LanguagePreference::English => Self::En,
            LanguagePreference::Auto => detect_os_language(),
        }
    }

    #[inline]
    pub fn pick<'a>(self, en: &'a str, it: &'a str) -> &'a str {
        match self {
            Self::En => en,
            Self::It => it,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::It => "it",
        }
    }
}

/// Raw JSON of the table: `{ "Key": ["english", "italiano"], ... }`.
pub const TABLE_JSON: &str = include_str!("../assets/loc.json");

static TABLE: OnceLock<HashMap<String, (String, String)>> = OnceLock::new();
static CURRENT: AtomicU8 = AtomicU8::new(0);
static CLI_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();

fn table() -> &'static HashMap<String, (String, String)> {
    TABLE.get_or_init(|| {
        let raw: HashMap<String, (String, String)> = serde_json::from_str(TABLE_JSON).unwrap_or_default();
        raw
    })
}

/// Sets the language from the settings preference (and, the first time, the `--lang` override).
pub fn initialize(pref: LanguagePreference, cli_override: Option<&str>) -> Lang {
    let _ = CLI_OVERRIDE.set(cli_override.map(str::to_string));
    apply(pref)
}

/// Changes the preference at run time (Settings → General → Language). Clears nothing: the `--lang` override wins.
pub fn apply(pref: LanguagePreference) -> Lang {
    let ov = CLI_OVERRIDE.get().and_then(|o| o.clone());
    let l = Lang::resolve(pref, ov.as_deref());
    CURRENT.store(if l == Lang::It { 1 } else { 0 }, Ordering::Relaxed);
    l
}

/// Changes the preference ignoring the command-line override (the user picked a language in the UI).
pub fn set_language(pref: LanguagePreference) -> Lang {
    let l = Lang::resolve(pref, None);
    CURRENT.store(if l == Lang::It { 1 } else { 0 }, Ordering::Relaxed);
    l
}

pub fn current() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 1 { Lang::It } else { Lang::En }
}

pub fn is_italian() -> bool {
    current() == Lang::It
}

/// Localized string for `key` (the key itself when missing, like the C# version).
pub fn get(key: &str) -> String {
    match table().get(key) {
        Some((en, it)) => if is_italian() { it.clone() } else { en.clone() },
        None => key.to_string(),
    }
}

/// `string.Format`-style substitution of `{0}`, `{1:...}` with pre-formatted arguments.
pub fn fmt(key: &str, args: &[&str]) -> String {
    format_with(&get(key), args)
}

pub fn format_with(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let inner = &after[..end];
            let idx_part = inner.split(':').next().unwrap_or("");
            if let Ok(i) = idx_part.parse::<usize>() {
                out.push_str(args.get(i).copied().unwrap_or(""));
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('{');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Number with one decimal and explicit sign (`{0:+0.0;-0.0}`), decimal comma in Italian.
pub fn signed1(v: f64) -> String {
    let s = format!("{v:+.1}");
    if is_italian() { s.replace('.', ",") } else { s }
}

/// Number without decimals (`{0:0}`).
pub fn int0(v: f64) -> String {
    format!("{:.0}", v)
}

#[cfg(windows)]
fn detect_os_language() -> Lang {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // Primary language ID: bits 0..10. 0x10 = LANG_ITALIAN.
    let lang_id = unsafe { GetUserDefaultUILanguage() } & 0x03FF;
    if lang_id == 0x10 { Lang::It } else { Lang::En }
}

#[cfg(not(windows))]
fn detect_os_language() -> Lang {
    if let Ok(lang) = std::env::var("LANG") {
        if lang.to_ascii_lowercase().starts_with("it") {
            return Lang::It;
        }
    }
    Lang::En
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_override_and_preferences_work() {
        assert_eq!(Lang::resolve(LanguagePreference::English, Some("it")), Lang::It);
        assert_eq!(Lang::resolve(LanguagePreference::Italian, Some("en")), Lang::En);
        assert_eq!(Lang::resolve(LanguagePreference::Italian, None), Lang::It);
        assert_eq!(Lang::resolve(LanguagePreference::English, None), Lang::En);
    }

    #[test]
    fn table_and_format() {
        assert!(table().len() >= 470, "loc table has {} keys", table().len());
        set_language(LanguagePreference::English);
        assert_eq!(get("Nav.Mixer"), "Mixer");
        assert_eq!(fmt("Tray.KnobFunc", &["Mix"]), "Knob: Mix");
        set_language(LanguagePreference::Italian);
        assert_eq!(get("Tray.Exit"), "Esci");
        assert_eq!(format_with("a {0} b {1:X8} c {2:+0.0;-0.0}", &["x", "y", "z"]), "a x b y c z");
        set_language(LanguagePreference::English);
        assert_eq!(get("Missing.Key"), "Missing.Key");
    }
}
