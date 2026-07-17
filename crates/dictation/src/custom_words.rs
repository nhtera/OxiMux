//! Fuzzy custom-word correction for transcripts.
//!
//! Speech models mangle proper nouns, brands, repo and command names — "oxy
//! mux" for "OxiMux", "charge bee" for "ChargeBee", "chat GPT" for "ChatGPT". A
//! user-supplied dictionary lets the transcript snap those back to the intended
//! spelling. The match is fuzzy (normalized edit distance) over 1-, 2- and
//! 3-word windows so multi-token mishearings collapse to a single dictionary
//! entry, with an exact-key shortcut for the common clean-collapse case.
//!
//! Self-contained: Levenshtein is implemented here rather than pulling a crate,
//! keeping the dictation crate dependency-light. Over-correction of ordinary
//! words is the real hazard, so acceptance is gated purely on a conservative
//! normalized edit distance (plus a length prefilter) — deliberately NOT on a
//! phonetic/soundex bonus, which coarsely collides unrelated words (e.g.
//! "later"↔"Ladder") and would rewrite common words.

/// Default acceptance threshold: a normalized edit-distance below this replaces
/// the window. Lower = stricter. 0.18 is conservative — roughly a one-character
/// slip in a six-character word — chosen to catch mishearings without rewriting
/// unrelated words. Tune via settings.
pub const DEFAULT_THRESHOLD: f64 = 0.18;

/// Largest word-window considered (so "chat g p t" → "ChatGPT" collapses 4? no —
/// 3 tokens max: "chat g p" / "g p t"). Three balances multi-token captures
/// against runtime.
const MAX_WINDOW: usize = 3;

/// Apply the dictionary to `transcript`. Returns the corrected text. `words` is
/// the user's dictionary (case as they typed it — that case is authoritative on
/// replacement). A `threshold` at or below 0 disables correction.
pub fn apply(transcript: &str, words: &[String], threshold: f64) -> String {
    if words.is_empty() || threshold <= 0.0 || transcript.trim().is_empty() {
        return transcript.to_string();
    }

    // Precompute each dictionary word's match key once.
    let dict: Vec<DictWord> = words
        .iter()
        .filter_map(|w| {
            let key = clean_key(w);
            if key.is_empty() {
                None
            } else {
                Some(DictWord {
                    display: w.clone(),
                    key,
                })
            }
        })
        .collect();
    if dict.is_empty() {
        return transcript.to_string();
    }

    let tokens: Vec<&str> = transcript.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let mut matched = false;
        // Longest window first so a 3-token mishearing wins over a 1-token one.
        let max_n = MAX_WINDOW.min(tokens.len() - i);
        for n in (1..=max_n).rev() {
            let window = &tokens[i..i + n];
            let candidate = window.iter().map(|t| clean_key(t)).collect::<String>();
            if candidate.len() < 2 {
                continue;
            }
            if let Some(best) = best_match(&candidate, &dict, threshold) {
                // Preserve leading punctuation of the first token and trailing
                // punctuation of the last so "…ChargeBee." keeps its comma/period.
                let lead = leading_punct(window[0]);
                let trail = trailing_punct(window[n - 1]);
                out.push(format!("{lead}{}{trail}", best.display));
                i += n;
                matched = true;
                break;
            }
        }
        if !matched {
            out.push(tokens[i].to_string());
            i += 1;
        }
    }

    out.join(" ")
}

struct DictWord {
    /// The user's spelling, used verbatim on replacement.
    display: String,
    /// Lowercased alnum-only key for distance scoring.
    key: String,
}

/// The best dictionary match for `candidate` whose (bonus-adjusted) score is
/// under `threshold`, or `None`.
fn best_match<'a>(candidate: &str, dict: &'a [DictWord], threshold: f64) -> Option<&'a DictWord> {
    let mut best: Option<(&DictWord, f64)> = None;
    for word in dict {
        // Length prefilter: skip pairs too different in length to plausibly match
        // (min 2-char slack so short words aren't over-filtered).
        let (a, b) = (candidate.len(), word.key.len());
        let diff = a.abs_diff(b);
        let longer = a.max(b);
        if longer == 0 || (diff > 2 && diff as f64 / longer as f64 > 0.25) {
            continue;
        }
        // Exact key match is a definite hit.
        if candidate == word.key {
            return Some(word);
        }
        let dist = levenshtein(candidate, &word.key);
        let score = dist as f64 / longer as f64;
        if score < threshold && best.map(|(_, s)| score < s).unwrap_or(true) {
            best = Some((word, score));
        }
    }
    best.map(|(w, _)| w)
}

/// Lowercase, keep only alphanumerics (drops spaces + punctuation). "Chat-G.P.T"
/// → "chatgpt".
fn clean_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Leading non-alphanumeric run of a token (opening quotes/brackets).
fn leading_punct(tok: &str) -> &str {
    let end = tok
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, _)| i)
        .unwrap_or(0);
    &tok[..end]
}

/// Trailing non-alphanumeric run of a token (commas, periods, closing brackets).
fn trailing_punct(tok: &str) -> &str {
    let start = tok
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(tok.len());
    &tok[start..]
}

/// Classic Levenshtein edit distance over Unicode scalar values.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_dictionary_is_identity() {
        assert_eq!(apply("hello world", &[], DEFAULT_THRESHOLD), "hello world");
    }

    #[test]
    fn corrects_single_word_homophone() {
        let out = apply("i use oximax daily", &dict(&["OxiMux"]), DEFAULT_THRESHOLD);
        assert_eq!(out, "i use OxiMux daily");
    }

    #[test]
    fn collapses_multi_token_mishearing() {
        let out = apply("open charge bee please", &dict(&["ChargeBee"]), DEFAULT_THRESHOLD);
        assert_eq!(out, "open ChargeBee please");
    }

    #[test]
    fn preserves_trailing_punctuation() {
        let out = apply("thanks oximax.", &dict(&["OxiMux"]), DEFAULT_THRESHOLD);
        assert_eq!(out, "thanks OxiMux.");
    }

    #[test]
    fn does_not_touch_unrelated_words() {
        // "banana" must not be dragged to "OxiMux".
        let out = apply("i ate a banana", &dict(&["OxiMux"]), DEFAULT_THRESHOLD);
        assert_eq!(out, "i ate a banana");
    }

    #[test]
    fn exact_match_is_left_as_the_dictionary_spelling() {
        let out = apply("run oximux now", &dict(&["OxiMux"]), DEFAULT_THRESHOLD);
        assert_eq!(out, "run OxiMux now");
    }

    #[test]
    fn threshold_zero_disables() {
        assert_eq!(apply("oximax", &dict(&["OxiMux"]), 0.0), "oximax");
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
    }

    #[test]
    fn common_word_not_over_corrected_by_length_prefilter() {
        // "cat" vs a long brand should be filtered on length, never replaced.
        let out = apply("the cat sat", &dict(&["ChargeBee"]), DEFAULT_THRESHOLD);
        assert_eq!(out, "the cat sat");
    }

    #[test]
    fn common_words_not_over_corrected_by_phonetic_collision() {
        // Regression: a phonetic (soundex) bonus used to widen the match budget so
        // that unrelated common words sharing a soundex code were rewritten. These
        // are edit-distance ~0.33+ from the dictionary word and MUST NOT match.
        assert_eq!(
            apply("i got there later", &dict(&["Ladder"]), DEFAULT_THRESHOLD),
            "i got there later"
        );
        assert_eq!(
            apply("cold winter day", &dict(&["Wonder"]), DEFAULT_THRESHOLD),
            "cold winter day"
        );
        assert_eq!(
            apply("send a letter", &dict(&["Ladder"]), DEFAULT_THRESHOLD),
            "send a letter"
        );
    }
}
