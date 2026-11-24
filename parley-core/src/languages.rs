//! The languages Whisper can transcribe, for the language pickers in
//! Settings, onboarding and the import dialog.
//!
//! Codes are the ones whisper.cpp accepts (`whisper_lang_id`), so a stored
//! choice passes straight through `utils::language_preference_from_setting`.

/// Code for automatic language detection.
pub const AUTO: &str = "auto";

/// `(code, English name)`: auto-detect first, then every Whisper language
/// sorted by name.
pub const WHISPER_LANGUAGES: &[(&str, &str)] = &[
    (AUTO, "Auto-detect"),
    ("af", "Afrikaans"),
    ("sq", "Albanian"),
    ("am", "Amharic"),
    ("ar", "Arabic"),
    ("hy", "Armenian"),
    ("as", "Assamese"),
    ("az", "Azerbaijani"),
    ("ba", "Bashkir"),
    ("eu", "Basque"),
    ("be", "Belarusian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("br", "Breton"),
    ("bg", "Bulgarian"),
    ("my", "Burmese"),
    ("yue", "Cantonese"),
    ("ca", "Catalan"),
    ("zh", "Chinese"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("en", "English"),
    ("et", "Estonian"),
    ("fo", "Faroese"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("gl", "Galician"),
    ("ka", "Georgian"),
    ("de", "German"),
    ("el", "Greek"),
    ("gu", "Gujarati"),
    ("ht", "Haitian Creole"),
    ("ha", "Hausa"),
    ("haw", "Hawaiian"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hu", "Hungarian"),
    ("is", "Icelandic"),
    ("id", "Indonesian"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("jw", "Javanese"),
    ("kn", "Kannada"),
    ("kk", "Kazakh"),
    ("km", "Khmer"),
    ("ko", "Korean"),
    ("lo", "Lao"),
    ("la", "Latin"),
    ("lv", "Latvian"),
    ("ln", "Lingala"),
    ("lt", "Lithuanian"),
    ("lb", "Luxembourgish"),
    ("mk", "Macedonian"),
    ("mg", "Malagasy"),
    ("ms", "Malay"),
    ("ml", "Malayalam"),
    ("mt", "Maltese"),
    ("mi", "Māori"),
    ("mr", "Marathi"),
    ("mn", "Mongolian"),
    ("ne", "Nepali"),
    ("no", "Norwegian"),
    ("nn", "Norwegian Nynorsk"),
    ("oc", "Occitan"),
    ("ps", "Pashto"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("pa", "Punjabi"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sa", "Sanskrit"),
    ("sr", "Serbian"),
    ("sn", "Shona"),
    ("sd", "Sindhi"),
    ("si", "Sinhala"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("so", "Somali"),
    ("es", "Spanish"),
    ("su", "Sundanese"),
    ("sw", "Swahili"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("tg", "Tajik"),
    ("ta", "Tamil"),
    ("tt", "Tatar"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("bo", "Tibetan"),
    ("tr", "Turkish"),
    ("tk", "Turkmen"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("uz", "Uzbek"),
    ("vi", "Vietnamese"),
    ("cy", "Welsh"),
    ("yi", "Yiddish"),
    ("yo", "Yoruba"),
];

/// The display name for `code`, or the code itself if it isn't a Whisper
/// language. Blank means auto-detect (the old free-text field's default).
pub fn label(code: &str) -> String {
    let code = normalize(code);
    WHISPER_LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| name.to_string())
        .unwrap_or(code)
}

/// A stored setting as a picker value: trimmed, lower-cased, blank → auto.
pub fn normalize(code: &str) -> String {
    let code = code.trim().to_ascii_lowercase();
    if code.is_empty() { AUTO.to_string() } else { code }
}

/// The Whisper language for a POSIX locale such as `en_GB.UTF-8` or `pt_BR`,
/// if Whisper supports it.
pub fn from_locale(locale: &str) -> Option<&'static str> {
    let lang = locale.split(['_', '.', '@', '-']).next()?.to_ascii_lowercase();
    // Norwegian Bokmål's locale code; Whisper calls it "no".
    let lang = if lang == "nb" { "no".to_string() } else { lang };
    WHISPER_LANGUAGES
        .iter()
        .find(|(c, _)| *c != AUTO && *c == lang)
        .map(|(c, _)| *c)
}

/// The language to preselect on first launch: the desktop locale's
/// language (`LC_ALL`, then `LC_MESSAGES`, then `LANG`), or auto-detect.
pub fn system_default() -> &'static str {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .and_then(|v| from_locale(&v))
        .unwrap_or(AUTO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales_map_to_whisper_codes() {
        assert_eq!(from_locale("en_GB.UTF-8"), Some("en"));
        assert_eq!(from_locale("pt_BR"), Some("pt"));
        assert_eq!(from_locale("de"), Some("de"));
        assert_eq!(from_locale("nb_NO.UTF-8"), Some("no"));
        assert_eq!(from_locale("C"), None);
        assert_eq!(from_locale("xx_YY"), None);
    }

    #[test]
    fn labels_handle_blank_and_unknown_codes() {
        assert_eq!(label("en"), "English");
        assert_eq!(label(" FR "), "French");
        assert_eq!(label(""), "Auto-detect");
        assert_eq!(label("zz"), "zz");
    }

    #[test]
    fn every_code_is_accepted_by_the_language_setting() {
        for (code, _) in WHISPER_LANGUAGES {
            assert_eq!(crate::utils::language_preference_from_setting(Some(code)), *code);
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut codes: Vec<_> = WHISPER_LANGUAGES.iter().map(|(c, _)| *c).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), WHISPER_LANGUAGES.len());
    }
}
