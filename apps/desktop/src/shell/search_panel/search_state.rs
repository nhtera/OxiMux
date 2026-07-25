//! Pure data model for the search panel header.
//!
//! Holds the live query string plus the boolean toggles and glob filters that
//! map onto `ripgrep` CLI flags in `rg_runner`. Kept free of GPUI types so it
//! can be cheaply cloned into background tasks.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub use_regex: bool,
    /// Comma- or whitespace-separated globs (`*.rs, src/**`). Empty = no filter.
    pub include_glob: String,
    /// Comma- or whitespace-separated globs to exclude (`!` is added by runner).
    pub exclude_glob: String,
}

impl SearchOptions {
    /// `true` when the query has at least one non-whitespace char. The runner
    /// is never invoked for empty queries — the UI clears results instead.
    pub fn has_query(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_have_empty_query() {
        let o = SearchOptions::default();
        assert!(!o.has_query());
        assert!(!o.case_sensitive);
        assert!(!o.whole_word);
        assert!(!o.use_regex);
    }

    #[test]
    fn whitespace_only_query_does_not_trigger_search() {
        let o = SearchOptions {
            query: "   \t\n".into(),
            ..Default::default()
        };
        assert!(!o.has_query());
    }

    #[test]
    fn non_empty_query_triggers_search() {
        let o = SearchOptions {
            query: "FileStatus".into(),
            ..Default::default()
        };
        assert!(o.has_query());
    }
}
