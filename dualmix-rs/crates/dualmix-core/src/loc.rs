//! Localization (English default; automatic Italian on Italian Windows UI language).

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
}
