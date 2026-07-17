//! The Whisper transcription-language table for dictation settings.
//!
//! Whisper accepts a fixed set of ~99 two-letter language codes (plus `yue` for
//! Cantonese in newer builds); passing an off-list code makes sherpa error, so
//! the picker and `DictationSettings::sanitized` both validate against this list.
//! `auto` (empty language upstream) means whisper auto-detects.
//!
//! Entries are ordered for display: `auto` first, then a small cluster of the
//! most common languages (Vietnamese first — this app is Vietnamese-first), then
//! the rest alphabetically by English name. The UI iterates the slice directly.
//!
//! Non-whisper model families (Parakeet, Zipformer, SenseVoice) ignore the
//! language entirely — the engine only threads it into the whisper recognizer —
//! so the Voice pane shows this picker only for whisper models.

/// The auto-detect selector: whisper receives an empty language and picks one.
pub const AUTO: &str = "auto";

/// `(code, English display name)`, in display order. `auto` leads; then the
/// common cluster; then alphabetical by name. Codes match OpenAI Whisper's
/// tokenizer language set.
pub const WHISPER_LANGUAGES: &[(&str, &str)] = &[
    (AUTO, "Auto-detect"),
    // Common cluster (Vietnamese-first), most-reached languages up top.
    ("vi", "Vietnamese"),
    ("en", "English"),
    ("zh", "Chinese"),
    ("yue", "Cantonese"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("fr", "French"),
    ("de", "German"),
    ("es", "Spanish"),
    ("ru", "Russian"),
    ("pt", "Portuguese"),
    ("th", "Thai"),
    // The rest, alphabetical by English name.
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
    ("ca", "Catalan"),
    ("hr", "Croatian"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("nl", "Dutch"),
    ("et", "Estonian"),
    ("fo", "Faroese"),
    ("fi", "Finnish"),
    ("gl", "Galician"),
    ("ka", "Georgian"),
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
    ("jw", "Javanese"),
    ("kn", "Kannada"),
    ("kk", "Kazakh"),
    ("km", "Khmer"),
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
    ("mi", "Maori"),
    ("mr", "Marathi"),
    ("mn", "Mongolian"),
    ("ne", "Nepali"),
    ("no", "Norwegian"),
    ("nn", "Nynorsk"),
    ("oc", "Occitan"),
    ("ps", "Pashto"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pa", "Punjabi"),
    ("ro", "Romanian"),
    ("sa", "Sanskrit"),
    ("sr", "Serbian"),
    ("sn", "Shona"),
    ("sd", "Sindhi"),
    ("si", "Sinhala"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("so", "Somali"),
    ("su", "Sundanese"),
    ("sw", "Swahili"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("tg", "Tajik"),
    ("ta", "Tamil"),
    ("tt", "Tatar"),
    ("te", "Telugu"),
    ("bo", "Tibetan"),
    ("tr", "Turkish"),
    ("tk", "Turkmen"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("uz", "Uzbek"),
    ("cy", "Welsh"),
    ("yi", "Yiddish"),
    ("yo", "Yoruba"),
];

/// True when `code` is a language the whisper recognizer accepts (including
/// `auto`). Case-sensitive on the lowercase code — callers lowercase first.
pub fn is_supported(code: &str) -> bool {
    WHISPER_LANGUAGES.iter().any(|(c, _)| *c == code)
}

/// The English display name for a language code, or the code itself if unknown
/// (so a hand-edited TOML code still renders something rather than blank).
pub fn display_name(code: &str) -> &str {
    WHISPER_LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_leads_and_is_supported() {
        assert_eq!(WHISPER_LANGUAGES[0].0, AUTO);
        assert!(is_supported("auto"));
    }

    #[test]
    fn vietnamese_and_english_present() {
        assert!(is_supported("vi"));
        assert!(is_supported("en"));
        assert_eq!(display_name("vi"), "Vietnamese");
    }

    #[test]
    fn covers_the_broad_whisper_set() {
        // A handful of less-common codes must resolve, proving the list is the
        // full set and not the legacy 3.
        for code in ["ja", "th", "fa", "haw", "yue", "cy", "sw"] {
            assert!(is_supported(code), "{code} should be supported");
        }
        assert!(WHISPER_LANGUAGES.len() > 90, "expected the full whisper set");
    }

    #[test]
    fn unknown_code_is_unsupported_and_echoes_itself() {
        assert!(!is_supported("xx"));
        assert_eq!(display_name("xx"), "xx");
    }

    #[test]
    fn codes_are_unique() {
        let mut codes: Vec<&str> = WHISPER_LANGUAGES.iter().map(|(c, _)| *c).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "duplicate language code in table");
    }
}
