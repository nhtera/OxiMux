//! The scope-name → [`HighlightKind`] mapping, and the vocabulary itself.
//!
//! **The rule this file exists to enforce: a capture name never reaches a
//! theme.** Grammars disagree wildly about naming — one calls a Rust `fn`
//! `storage.type.function.rust`, another calls the same idea
//! `keyword.declaration` — and a theme written against those strings is a theme
//! written against one grammar set. Everything collapses to the small closed set
//! below, and the theme only ever sees that.
//!
//! Adding a kind is fine when a theme genuinely wants a distinction it cannot
//! express. Leaking a scope name is not.

/// What a span *is*, independent of how anything chooses to draw it.
///
/// Small on purpose. Every extra variant is a decision every theme then has to
/// make, and a theme that has to name thirty token classes is a theme nobody
/// finishes writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    Function,
    Type,
    String,
    /// An escape inside a string (`\n`, `\u{1f600}`). Separate from
    /// [`Self::String`] because it is the one distinction that reliably tells a
    /// reader where a string's structure is, and every serious theme colors it.
    Escape,
    Number,
    Comment,
    Constant,
    Operator,
    Punctuation,
    Variable,
    /// An annotation or decorator — `#[derive(..)]`, `@Override`, `[Serializable]`.
    Attribute,
    /// A module, package or namespace path segment.
    Namespace,
    /// A markup tag name (HTML/XML/JSX elements).
    Tag,
}

/// Map a TextMate scope stack to a kind, or `None` for "not a token".
///
/// Walks from the **innermost** scope outward and takes the first match, which
/// is what makes specificity work: `constant.character.escape` must beat the
/// enclosing `string.quoted.double`, and it only does so by being looked at
/// first.
///
/// `None` is the common and correct answer for structural scopes — `source.rust`
/// and every `meta.*` describe regions, not tokens, and coloring them would tint
/// whole blocks of code.
#[cfg(test)]
fn kind_for_scopes<'a>(
    scopes: impl DoubleEndedIterator<Item = &'a str>,
) -> Option<HighlightKind> {
    scopes.rev().find_map(kind_for_scope)
}

/// Map one scope name, matching on the longest meaningful prefix.
///
/// Prefix matching rather than exact: scope names are dotted paths that get more
/// specific to the right and always end in the language name
/// (`keyword.operator.assignment.rust`), so matching a prefix is how one arm
/// covers every language at once.
pub(crate) fn kind_for_scope(scope: &str) -> Option<HighlightKind> {
    use HighlightKind::*;

    // Ordered most-specific-first within each family; the first `starts_with`
    // that hits wins.
    const TABLE: &[(&str, HighlightKind)] = &[
        // Escapes before strings — an escape sits *inside* a string scope, and
        // the whole point is that it wins.
        ("constant.character.escape", Escape),
        ("constant.other.escape", Escape),
        ("constant.numeric", Number),
        ("constant.language", Constant),
        ("constant.character", Constant),
        ("constant", Constant),
        ("comment", Comment),
        ("string", String),
        // `keyword.operator` before `keyword`, or every operator reads as a
        // keyword and the two can never be themed apart.
        ("keyword.operator", Operator),
        ("keyword", Keyword),
        // `storage` is where most grammars put `fn`, `let`, `const`, `static`,
        // `public` — declaration keywords by another name.
        ("storage", Keyword),
        ("entity.name.function", Function),
        ("entity.name.type", Type),
        ("entity.name.class", Type),
        ("entity.name.struct", Type),
        ("entity.name.enum", Type),
        ("entity.name.interface", Type),
        ("entity.name.trait", Type),
        ("entity.name.namespace", Namespace),
        ("entity.name.module", Namespace),
        ("entity.name.package", Namespace),
        ("entity.name.tag", Tag),
        ("entity.other.attribute-name", Attribute),
        ("entity.other.inherited-class", Type),
        ("entity.name", Type),
        ("support.function", Function),
        ("support.macro", Function),
        ("support.class", Type),
        ("support.type", Type),
        ("support.constant", Constant),
        ("support.variable", Variable),
        ("variable.function", Function),
        ("variable.annotation", Attribute),
        ("variable", Variable),
        ("punctuation.definition.comment", Comment),
        ("punctuation.definition.string", String),
        ("punctuation.definition.annotation", Attribute),
        ("punctuation", Punctuation),
        ("meta.annotation", Attribute),
        ("meta.decorator", Attribute),
        ("markup.bold", Keyword),
        ("markup.italic", Keyword),
        ("markup.heading", Function),
        ("markup.raw", String),
        ("markup.underline.link", Constant),
    ];

    TABLE.iter().find(|(prefix, _)| scope.starts_with(prefix)).map(|(_, kind)| *kind)
}

#[cfg(test)]
mod tests {
    use super::HighlightKind::*;
    use super::*;

    fn kind(scopes: &[&str]) -> Option<HighlightKind> {
        kind_for_scopes(scopes.iter().copied())
    }

    /// Structural scopes are regions, not tokens. Coloring them would tint
    /// whole blocks rather than words.
    #[test]
    fn structural_scopes_are_not_tokens() {
        assert_eq!(kind(&["source.rust"]), None);
        assert_eq!(kind(&["source.rust", "meta.function.rust"]), None);
        assert_eq!(kind(&["text.html", "meta.block"]), None);
    }

    /// Specificity: the innermost scope wins, which is the whole reason the
    /// walk is inside-out.
    #[test]
    fn the_innermost_scope_wins() {
        assert_eq!(
            kind(&["source.rust", "meta.function.rust", "entity.name.function.rust"]),
            Some(Function),
        );
        // An escape inside a string must beat the string, or `\n` is invisible.
        assert_eq!(
            kind(&["source.rust", "string.quoted.double.rust", "constant.character.escape.rust"]),
            Some(Escape),
        );
    }

    /// Operators must not collapse into keywords — a theme that cannot tell
    /// `=` from `let` is a theme missing its most-used distinction.
    #[test]
    fn operators_are_not_keywords() {
        assert_eq!(kind(&["keyword.operator.assignment.rust"]), Some(Operator));
        assert_eq!(kind(&["keyword.control.rust"]), Some(Keyword));
    }

    /// Real scope stacks observed from the bundled grammars, so the table is
    /// checked against what actually arrives rather than what it assumes.
    #[test]
    fn observed_rust_scopes_map_as_expected() {
        for (scopes, expected) in [
            (vec!["source.rust", "meta.function.rust", "storage.type.function.rust"], Keyword),
            (vec!["source.rust", "meta.function.rust", "entity.name.function.rust"], Function),
            (vec!["source.rust", "meta.block.rust", "storage.type.rust"], Keyword),
            (vec!["source.rust", "meta.block.rust", "keyword.operator.assignment.rust"], Operator),
            (vec!["source.rust", "meta.block.rust", "string.quoted.double.rust"], String),
            (vec!["source.rust", "meta.block.rust", "comment.line.double-slash.rust"], Comment),
            (
                vec![
                    "source.rust",
                    "meta.block.rust",
                    "comment.line.double-slash.rust",
                    "punctuation.definition.comment.rust",
                ],
                Comment,
            ),
            (
                vec![
                    "source.rust",
                    "meta.function.parameters.rust",
                    "punctuation.section.parameters.begin.rust",
                ],
                Punctuation,
            ),
        ] {
            assert_eq!(kind(&scopes), Some(expected), "scopes: {scopes:?}");
        }
    }

    /// A string's own delimiters read as string, not as loose punctuation —
    /// otherwise the quotes flicker a different color than what they enclose.
    #[test]
    fn string_delimiters_stay_string() {
        assert_eq!(
            kind(&["string.quoted.double.rust", "punctuation.definition.string.begin.rust"]),
            Some(String),
        );
    }

    /// An unknown scope is plain, never an error and never a guess.
    #[test]
    fn unknown_scopes_are_plain() {
        assert_eq!(kind(&["something.nobody.has.seen"]), None);
        assert_eq!(kind(&[]), None);
    }
}
