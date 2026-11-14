//! Tiny, dependency-light message catalog.
//!
//! Catalogs live in `/i18n/<lang>.toml` (flat `key = "text"` tables with
//! `{placeholder}` arguments) and are compiled into the binaries, so no
//! runtime files are needed inside PAM stacks or early boot.
//!
//! Language selection follows GNU gettext precedence:
//! `LANGUAGE` (colon list) > `LC_ALL` > `LC_MESSAGES` > `LANG`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// (language code, native name, catalog source)
pub const LANGUAGES: &[(&str, &str, &str)] = &[
    ("en", "English", include_str!("../../../i18n/en.toml")),
    ("de", "Deutsch", include_str!("../../../i18n/de.toml")),
    ("fr", "Français", include_str!("../../../i18n/fr.toml")),
    ("es", "Español", include_str!("../../../i18n/es.toml")),
    ("pt", "Português", include_str!("../../../i18n/pt.toml")),
    ("ja", "日本語", include_str!("../../../i18n/ja.toml")),
    ("zh", "中文", include_str!("../../../i18n/zh.toml")),
    ("ko", "한국어", include_str!("../../../i18n/ko.toml")),
    ("hi", "हिन्दी", include_str!("../../../i18n/hi.toml")),
    ("sw", "Kiswahili", include_str!("../../../i18n/sw.toml")),
    ("ru", "Русский", include_str!("../../../i18n/ru.toml")),
    ("ar", "العربية", include_str!("../../../i18n/ar.toml")),
    ("id", "Bahasa Indonesia", include_str!("../../../i18n/id.toml")),
    ("ms", "Bahasa Melayu", include_str!("../../../i18n/ms.toml")),
    ("rw", "Ikinyarwanda", include_str!("../../../i18n/rw.toml")),
    ("nyn", "Runyankore", include_str!("../../../i18n/nyn.toml")),
];

type Catalog = HashMap<String, String>;

fn catalogs() -> &'static HashMap<&'static str, Catalog> {
    static CATALOGS: OnceLock<HashMap<&'static str, Catalog>> = OnceLock::new();
    CATALOGS.get_or_init(|| {
        LANGUAGES
            .iter()
            .map(|(code, _, src)| {
                let table: toml::Table =
                    toml::from_str(src).unwrap_or_else(|e| panic!("invalid catalog {code}: {e}"));
                let cat = table
                    .into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_owned())))
                    .collect();
                (*code, cat)
            })
            .collect()
    })
}

/// Normalise a POSIX locale (`de_DE.UTF-8@euro`, `pt-BR`, `zh_CN`) to one of
/// our catalog codes, or `None` if unsupported.
pub fn normalize(locale: &str) -> Option<&'static str> {
    let base = locale
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .split(['_', '-'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let base = match base.as_str() {
        // Indonesian and Malay share a lot but keep separate catalogs.
        "in" => "id".to_string(),
        "zsm" => "ms".to_string(),
        "swa" => "sw".to_string(),
        "kin" => "rw".to_string(),
        _ => base,
    };
    LANGUAGES.iter().map(|(c, _, _)| *c).find(|c| *c == base)
}

/// Pick the language from a set of environment-like lookups.
pub fn detect_with(get: impl Fn(&str) -> Option<String>) -> &'static str {
    if let Some(list) = get("LANGUAGE").filter(|s| !s.is_empty()) {
        for l in list.split(':') {
            if let Some(c) = normalize(l) {
                return c;
            }
        }
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = get(var).filter(|s| !s.is_empty()) {
            if v == "C" || v == "POSIX" || v.starts_with("C.") {
                return "en";
            }
            return normalize(&v).unwrap_or("en");
        }
    }
    "en"
}

/// Language of the current process environment.
pub fn detect() -> &'static str {
    detect_with(|k| std::env::var(k).ok())
}

/// Translate `key` into `lang` and substitute `{name}` placeholders.
/// Falls back to English, then to the key itself.
pub fn tr(lang: &str, key: &str, args: &[(&str, &str)]) -> String {
    let cats = catalogs();
    let lang = normalize(lang).unwrap_or("en");
    let text = cats
        .get(lang)
        .and_then(|c| c.get(key))
        .or_else(|| cats.get("en").and_then(|c| c.get(key)))
        .map(String::as_str)
        .unwrap_or(key);
    let mut out = text.to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Convenience macro: `t!(lang, "key")` or `t!(lang, "key", user = name)`.
#[macro_export]
macro_rules! t {
    ($lang:expr, $key:expr) => { $crate::tr($lang, $key, &[]) };
    ($lang:expr, $key:expr, $($name:ident = $val:expr),+ $(,)?) => {
        $crate::tr($lang, $key, &[$((stringify!($name), &$val.to_string())),+])
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(s: &str) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        let mut rest = s;
        while let Some(i) = rest.find('{') {
            if let Some(j) = rest[i..].find('}') {
                set.insert(rest[i + 1..i + j].to_string());
                rest = &rest[i + j + 1..];
            } else {
                break;
            }
        }
        set
    }

    #[test]
    fn all_catalogs_complete_and_consistent() {
        let cats = catalogs();
        let en = &cats["en"];
        assert!(en.len() > 20);
        for (code, _, _) in LANGUAGES {
            let c = &cats[code];
            for (k, v) in en {
                let tv = c.get(k).unwrap_or_else(|| panic!("{code}: missing key {k}"));
                assert!(!tv.trim().is_empty(), "{code}: empty {k}");
                assert_eq!(
                    placeholders(v),
                    placeholders(tv),
                    "{code}: placeholder mismatch in {k}"
                );
            }
            for k in c.keys() {
                assert!(en.contains_key(k), "{code}: unknown key {k}");
            }
        }
    }

    #[test]
    fn locale_normalisation() {
        assert_eq!(normalize("de_DE.UTF-8"), Some("de"));
        assert_eq!(normalize("pt_BR.UTF-8"), Some("pt"));
        assert_eq!(normalize("zh_CN.UTF-8"), Some("zh"));
        assert_eq!(normalize("sw_KE"), Some("sw"));
        assert_eq!(normalize("ms_MY.UTF-8"), Some("ms"));
        assert_eq!(normalize("rw_RW.UTF-8"), Some("rw"));
        assert_eq!(normalize("kin"), Some("rw"));
        // Runyankore has no glibc locale; LANGUAGE=nyn selects it.
        assert_eq!(normalize("nyn"), Some("nyn"));
        assert_eq!(normalize("nl_NL.UTF-8"), None);
    }

    #[test]
    fn detection_precedence() {
        let env = |k: &str| match k {
            "LANGUAGE" => Some("nl:fr".to_string()),
            "LANG" => Some("de_DE.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(detect_with(env), "fr");
        let env2 = |k: &str| (k == "LANG").then(|| "ja_JP.UTF-8".to_string());
        assert_eq!(detect_with(env2), "ja");
        assert_eq!(detect_with(|_| None), "en");
    }

    #[test]
    fn substitution_and_fallback() {
        assert_eq!(
            t!("en", "enroll.done", count = 7),
            tr("en", "enroll.done", &[("count", "7")])
        );
        assert!(t!("de", "enroll.done", count = 7).contains('7'));
        assert_eq!(tr("xx", "no.such.key", &[]), "no.such.key");
    }
}
