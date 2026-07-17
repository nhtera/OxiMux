//! Transcript cleanup: filler words, stutters, and whisper hallucinations.
//!
//! Speech models emit disfluencies ("um", "uh"), stutter repeats ("wh wh wh"),
//! and — on near-silent audio — phantom captions ("(sad music)", "Thank you.").
//! This pass removes them. It is deliberately conservative: filler lists are
//! per-language (an unknown language removes no fillers, only the
//! language-agnostic cleanups run), stutter collapse needs three repeats, and a
//! phantom phrase is only dropped when it is the *entire* output.
//!
//! Pure + unit-tested; over-removal of real words is the hazard, so the ambiguous
//! ones ("ah", "like", Portuguese "um") are excluded from the lists.

/// Rewrite an ALL-CAPS transcript to sentence case.
///
/// Some models can only emit uppercase — their BPE vocabulary contains no
/// lowercase tokens at all (the dedicated Vietnamese zipformers are trained on
/// uppercase-normalized text, the usual icefall convention). Inserting their raw
/// output types SHOUTING TEXT at the cursor, so it is normalized here.
///
/// Only call this for models flagged [`crate::ModelSpec::uppercase_output`]:
/// applying it to a mixed-case model (whisper) would destroy real capitalization.
/// Original casing is unrecoverable — the model never encoded it — so proper
/// nouns come back lowercase; the custom-words dictionary is the way to restore
/// specific ones, which is why this must run *before* that pass.
pub fn sentence_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Capitalize the first letter, and any letter starting a new sentence (these
    // models emit no punctuation today, but a mid-string `.` must not stay lower
    // if one ever does).
    let mut at_sentence_start = true;
    for c in text.to_lowercase().chars() {
        if at_sentence_start && c.is_alphabetic() {
            out.extend(c.to_uppercase());
            at_sentence_start = false;
        } else {
            if matches!(c, '.' | '!' | '?') {
                at_sentence_start = true;
            }
            out.push(c);
        }
    }
    out
}

/// Clean `text` for the given whisper language code (`"auto"`, `"vi"`, `"en"`,
/// …). `enabled == false` returns the text unchanged. Language-agnostic steps
/// (bracketed non-speech, stutters, whole-output phantom phrases) always run
/// when enabled; filler removal only runs for languages with a known list.
pub fn filter(text: &str, language: &str, enabled: bool) -> String {
    if !enabled || text.trim().is_empty() {
        return text.to_string();
    }

    // 1. Strip bracketed / musical non-speech annotations anywhere — these are
    //    never intended dictation.
    let stripped = strip_bracketed_nonspeech(text);

    // 2. If what remains IS a known phantom caption (whole-output only), drop it.
    if is_hallucination_phrase(&stripped) {
        return String::new();
    }

    // 3. Remove filler words (language-gated) then collapse stutters.
    let no_fillers = remove_fillers(&stripped, language);
    let collapsed = collapse_stutters(&no_fillers);

    // 4. Normalize whitespace.
    collapsed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove `(...)`, `[...]`, and musical-note runs (`♪ ... ♪`, `♫`). Non-greedy
/// bracket matching so "a (x) b (y) c" loses both parentheticals, not the middle.
fn strip_bracketed_nonspeech(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth_paren = 0u32;
    let mut depth_brack = 0u32;
    for c in text.chars() {
        match c {
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '[' => depth_brack += 1,
            ']' => depth_brack = depth_brack.saturating_sub(1),
            '♪' | '♫' => {}
            _ if depth_paren == 0 && depth_brack == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Filler words for a language code. `en` and `vi` have curated lists; `auto`
/// uses their union (the app's common case). Every other language returns an
/// empty set so a pinned language we have no list for never loses real words.
fn fillers_for(language: &str) -> &'static [&'static str] {
    // Near-universal English disfluencies; the ambiguous "ah"/"like"/"so" are
    // deliberately excluded.
    const EN: &[&str] = &["um", "umm", "uhm", "uh", "uhh", "uhhh", "er", "erm", "hmm", "mm", "mhm"];
    // Vietnamese hesitation sounds only; real words like "à"/"thì" are excluded.
    const VI: &[&str] = &["ừ", "ừm", "ờ", "ờm", "ưm"];
    const AUTO: &[&str] = &[
        "um", "umm", "uhm", "uh", "uhh", "uhhh", "er", "erm", "hmm", "mm", "mhm", "ừ", "ừm", "ờ",
        "ờm", "ưm",
    ];
    match language {
        "en" => EN,
        "vi" => VI,
        "auto" | "" => AUTO,
        _ => &[],
    }
}

/// Drop filler tokens (word-boundary, case-insensitive on the cleaned form).
/// Keeps the token's non-filler neighbors and their spacing.
fn remove_fillers(text: &str, language: &str) -> String {
    let fillers = fillers_for(language);
    if fillers.is_empty() {
        return text.to_string();
    }
    text.split_whitespace()
        .filter(|tok| {
            let core: String = tok
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();
            core.is_empty() || !fillers.contains(&core.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collapse three-or-more consecutive case-insensitive repeats of an alphabetic
/// word to a single instance ("wh wh wh wh" → "wh"). Two repeats are preserved
/// (real doubles like "no no", "very very").
fn collapse_stutters(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let word = tokens[i];
        let key: String = word.to_lowercase();
        let is_alpha = word.chars().all(|c| c.is_alphabetic()) && !word.is_empty();
        let mut run = 1;
        while i + run < tokens.len() && tokens[i + run].to_lowercase() == key {
            run += 1;
        }
        // 3+ repeats of an alphabetic word collapse to one; otherwise keep all.
        let keep = if is_alpha && run >= 3 { 1 } else { run };
        for _ in 0..keep {
            out.push(word);
        }
        i += run;
    }
    out.join(" ")
}

/// Whether the whole (trimmed, punctuation-normalized) text is a known whisper
/// phantom caption. Only used as a whole-output guard so a real utterance that
/// merely contains "thank you" is untouched.
fn is_hallucination_phrase(text: &str) -> bool {
    // Notorious whisper hallucinations on silence / non-speech. Kept short and
    // specific; matched only against the entire output.
    const PHRASES: &[&str] = &[
        "thank you",
        "thank you.",
        "thanks for watching",
        "thanks for watching!",
        "please subscribe",
        "subscribe to my channel",
        "you",
        "bye",
        "bye.",
        "sad music",
        "music",
        "music playing",
        "silence",
        "applause",
        "subtitles by the amara.org community",
    ];
    let norm: String = text
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '.')
        .flat_map(|c| c.to_lowercase())
        .collect();
    let norm = norm.split_whitespace().collect::<Vec<_>>().join(" ");
    PHRASES.contains(&norm.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_case_fixes_shouting_vietnamese() {
        // The exact shape the vi zipformers emit: all caps, no punctuation.
        assert_eq!(
            sentence_case("TẠI SAO LẠI BIẾT HOA VẬY MÌNH CÓ BIẾT HOA ĐÂU"),
            "Tại sao lại biết hoa vậy mình có biết hoa đâu"
        );
    }

    #[test]
    fn sentence_case_preserves_vietnamese_diacritics() {
        // Unicode-aware casing: Ế→ế, Ạ→ạ must survive intact.
        assert_eq!(sentence_case("XIN CHÀO THẾ GIỚI"), "Xin chào thế giới");
        assert_eq!(sentence_case("ĐƯỢC"), "Được");
    }

    #[test]
    fn sentence_case_capitalizes_after_terminal_punctuation() {
        assert_eq!(sentence_case("MỘT. HAI! BA?"), "Một. Hai! Ba?");
    }

    #[test]
    fn sentence_case_handles_empty_and_non_letter_starts() {
        assert_eq!(sentence_case(""), "");
        assert_eq!(sentence_case("   "), "   ");
        // Leading non-letters must not consume the capitalization.
        assert_eq!(sentence_case("  XIN CHÀO"), "  Xin chào");
        assert_eq!(sentence_case("123 MỘT"), "123 Một");
    }

    #[test]
    fn disabled_is_identity() {
        assert_eq!(filter("um hello", "en", false), "um hello");
    }

    #[test]
    fn removes_english_fillers() {
        assert_eq!(filter("um, so uh the thing", "en", true), "so the thing");
    }

    #[test]
    fn keeps_real_doubles_collapses_triples() {
        assert_eq!(filter("no no thanks", "en", true), "no no thanks");
        assert_eq!(filter("wh wh wh what", "en", true), "wh what");
    }

    #[test]
    fn strips_bracketed_and_music() {
        assert_eq!(filter("hello (sad music) there", "en", true), "hello there");
        // Two "la" (kept as a real double) isolates the music-note strip from
        // stutter collapse (three-plus repeats would collapse to one).
        assert_eq!(filter("♪ la la ♪ ok", "en", true), "la la ok");
    }

    #[test]
    fn whole_output_hallucination_dropped() {
        assert_eq!(filter("Thank you.", "en", true), "");
        assert_eq!(filter("(sad music)", "en", true), "");
    }

    #[test]
    fn hallucination_phrase_inside_real_text_is_kept() {
        // "thank you" mid-sentence must survive — only whole-output matches drop.
        assert_eq!(
            filter("thank you for the code review", "en", true),
            "thank you for the code review"
        );
    }

    #[test]
    fn unknown_language_removes_no_fillers() {
        // Portuguese "um" (= "a/an") must NOT be stripped for a pinned pt.
        assert_eq!(filter("um livro", "pt", true), "um livro");
    }

    #[test]
    fn vietnamese_fillers_removed_for_vi_and_auto() {
        assert_eq!(filter("ừ mình nghĩ vậy", "vi", true), "mình nghĩ vậy");
        assert_eq!(filter("ừ mình nghĩ vậy", "auto", true), "mình nghĩ vậy");
    }

    #[test]
    fn auto_removes_english_fillers_too() {
        assert_eq!(filter("um hello", "auto", true), "hello");
    }
}
