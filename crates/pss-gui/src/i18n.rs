/// Language codes with their own-language display names (same codes as `locales/*.toml`).
pub const LANGUAGES: [(&str, &str); 2] = [("fr", "Français"), ("en", "English")];

pub const DEFAULT_LANG: &str = "fr";

/// A saved code that no longer matches a locale file falls back to the default instead of
/// leaving every text showing its raw key.
pub fn normalize(code: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(DEFAULT_LANG, |(c, _)| c)
}

pub fn set(code: &str) {
    rust_i18n::set_locale(code);
}

/// Translates a key and returns an owned `String` (egui widgets take `String`/`&str`, not the
/// `Cow` that `t!` returns). Same syntax as `rust_i18n::t!`.
#[macro_export]
macro_rules! tr {
    ($($args:tt)*) => {
        rust_i18n::t!($($args)*).to_string()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `tr!("key"...)` literal in the GUI sources, including calls split over lines.
    fn keys_used_in_sources() -> Vec<String> {
        let mut keys = Vec::new();
        for src in [include_str!("main.rs"), include_str!("crew.rs")] {
            let mut rest = src;
            while let Some(i) = rest.find("tr!(") {
                // skip longer macro names ending in "tr!(" (none today, but cheap to guard)
                let preceded_by_ident = rest[..i]
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                rest = &rest[i + 4..];
                if preceded_by_ident {
                    continue;
                }
                let trimmed = rest.trim_start();
                if let Some(after_quote) = trimmed.strip_prefix('"') {
                    if let Some(end) = after_quote.find('"') {
                        keys.push(after_quote[..end].to_string());
                    }
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    #[test]
    fn every_key_used_in_code_exists_in_every_language() {
        let keys = keys_used_in_sources();
        assert!(
            keys.len() > 50,
            "scan found suspiciously few keys: {}",
            keys.len()
        );
        let mut missing = Vec::new();
        for (code, _) in LANGUAGES {
            for key in &keys {
                if crate::_rust_i18n_try_translate(code, key).is_none() {
                    missing.push(format!("{code}: {key}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "keys without a translation: {missing:#?}"
        );
    }

    #[test]
    fn values_are_interpolated_and_languages_differ() {
        set("fr");
        assert_eq!(
            tr!("plan.training_used", used = 12, cap = 70),
            "12 / 70 utilisés"
        );
        set("en");
        assert_eq!(
            tr!("plan.training_used", used = 12, cap = 70),
            "12 / 70 used"
        );
        set(DEFAULT_LANG);
    }

    #[test]
    fn unknown_saved_language_falls_back_to_default() {
        assert_eq!(normalize("en"), "en");
        assert_eq!(normalize("de"), DEFAULT_LANG);
        assert_eq!(normalize(""), DEFAULT_LANG);
    }
}
