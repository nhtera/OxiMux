//! Keep radius and type sizes routed through the design tokens.
//!
//! `Density` and `Typography` exist so the cockpit has one radius scale and one
//! type scale. A measurement of the tree found colour 96% tokenised and
//! typography 97% — but radius only 62%: 148 token uses against 89 hardcoded
//! `.rounded(px(N))` spread over 15 distinct values, where the scale has five.
//! The literals are not a style preference, they are why the chrome's corners
//! do not agree with each other.
//!
//! They are also a hard blocker on UI zoom. Zoom multiplies every dimension by
//! deriving `Density` and `Typography` from a scale factor; a literal silently
//! refuses to scale, and the failure mode is nasty — chrome that looks almost
//! right at 125% but has mismatched corners and clipped descenders. So this
//! lint is not cosmetic housekeeping, it is the thing that makes the zoom
//! feature possible to finish.
//!
//! A unit test cannot catch this: the property is about how the source is
//! written, not what it computes. Hence a lint, on the same ratchet the
//! file-size gate uses — an allowlisted file may shed literals freely but fails
//! the moment it gains one, so the debt only goes down.

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};

/// The call sites that must take a token. Both are `f32`-valued builders whose
/// argument belongs to a scale: corner radius, and type size.
///
/// Deliberately only two spellings, because only two are used — gpui exposes
/// per-corner variants (`rounded_t`, `rounded_l`, …) but this tree never calls
/// them with a literal. A new spelling shows up as a lint that passes while the
/// corners drift, so re-check this list if the pattern count moves.
const WATCHED: &[&str] = &[".rounded(px(", ".text_size(px("];

const ALLOW_FILE: &str = "xtask/literal-allow.txt";

/// Mirrors `main.rs::SOURCE_ROOTS` — see the note there on why `apps/` is not
/// walked wholesale.
const SOURCE_ROOTS: &[&str] = &["crates", "apps/desktop"];

/// One hardcoded literal, located for the error message.
pub struct Hit {
    pub line: usize,
    pub text: String,
}

/// The zoom escape hatch for dimensions that are not on a token scale.
///
/// Legitimate for a glyph box or a gradient width; never for the two call
/// sites this lint watches. Type sizes and corner radii *have* scales, so
/// routing one through `scale()` would make it follow the zoom while still
/// disagreeing with every other size and corner in the tree — and it would
/// slip off this ratchet on the way, since the argument no longer starts
/// with a digit. Counted as a hit so the ratchet cannot be gamed by it.
const SCALE_HATCH: &str = ".scale(";

/// Count literal-valued calls in one file's text.
///
/// A call is "literal-valued" when the character after `px(` starts a number.
/// `px(density.r_card)` and `px(self.size)` begin with an identifier and pass;
/// `px(8.0)` and `px(12.)` do not. Leading `-` is not considered — a negative
/// radius or type size is not a thing, and treating `-` as numeric would only
/// misclassify arithmetic on a token.
pub fn scan(text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for pat in WATCHED {
            let mut from = 0usize;
            while let Some(at) = line[from..].find(pat) {
                let open = from + at + pat.len();
                let rest = &line[open..];
                let literal = rest.chars().next().is_some_and(|c| c.is_ascii_digit());
                // The argument up to its first `)`, which is where a
                // `density.scale(N)` call would close.
                let arg = &rest[..rest.find(')').map_or(rest.len(), |i| i + 1)];
                if literal || arg.contains(SCALE_HATCH) {
                    hits.push(Hit {
                        line: n + 1,
                        text: line.trim().to_string(),
                    });
                }
                from = open;
            }
        }
    }
    hits
}

pub fn run(root: &Path) -> Result<(), Box<dyn Error>> {
    let allow = load_allowlist(root)?;
    let mut files: Vec<PathBuf> = Vec::new();
    for r in SOURCE_ROOTS {
        collect_rs(&root.join(r), &mut files)?;
    }
    files.sort();

    let mut over: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut total = 0usize;

    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let hits = scan(&std::fs::read_to_string(file)?);
        total += hits.len();
        let budget = allow.get(&rel).copied().unwrap_or(0);

        if hits.len() > budget {
            let shown: Vec<String> = hits
                .iter()
                .take(3)
                .map(|h| format!("      {}:{}  {}", rel, h.line, h.text))
                .collect();
            over.push(format!(
                "  {rel}: {} literal(s), budget {budget}\n{}",
                hits.len(),
                shown.join("\n")
            ));
        } else if budget > 0 && hits.len() < budget {
            stale.push(format!(
                "  STALE {rel}: {} literal(s), budget {budget} — lower it",
                hits.len()
            ));
        }
    }

    if !stale.is_empty() {
        println!("literal-lint: budgets that can ratchet down:");
        for s in &stale {
            println!("{s}");
        }
    }

    if over.is_empty() {
        println!("literal-lint: ok ({total} allowlisted literal(s) remaining)");
        return Ok(());
    }

    Err(format!(
        "literal-lint: {} file(s) carry un-allowlisted radius/type literals:\n{}\n\n\
         Use a `Density` or `Typography` token instead of a raw px value. If the \n\
         value is genuinely one-off and intentional, add a row to {ALLOW_FILE} \n\
         with a comment saying why — but two sites sharing a number means it \n\
         wants to be a token.",
        over.len(),
        over.join("\n")
    )
    .into())
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            // `target/` can appear under a crate dir on a dirty tree.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_rs(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn load_allowlist(root: &Path) -> Result<HashMap<String, usize>, Box<dyn Error>> {
    let path = root.join(ALLOW_FILE);
    let mut map = HashMap::new();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(e) => return Err(e.into()),
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let file = parts
            .next()
            .ok_or_else(|| format!("{ALLOW_FILE}:{}: missing path", n + 1))?;
        let budget: usize = parts
            .next()
            .ok_or_else(|| format!("{ALLOW_FILE}:{}: missing count for '{file}'", n + 1))?
            .parse()
            .map_err(|_| format!("{ALLOW_FILE}:{}: count for '{file}' is not a number", n + 1))?;
        map.insert(file.replace('\\', "/"), budget);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_a_literal_radius() {
        assert_eq!(scan(".rounded(px(8.0))").len(), 1);
    }

    #[test]
    fn flags_a_literal_type_size() {
        assert_eq!(scan(".text_size(px(11.0))").len(), 1);
    }

    /// The whole point: a token-valued call is what we want people to write.
    #[test]
    fn allows_a_token_radius() {
        assert_eq!(scan(".rounded(px(density.r_card))").len(), 0);
        assert_eq!(scan(".text_size(px(typo.t_body_md))").len(), 0);
    }

    /// Trailing-dot floats are valid Rust and appear in this tree.
    #[test]
    fn flags_trailing_dot_float() {
        assert_eq!(scan(".rounded(px(6.))").len(), 1);
    }

    /// Two on one line must both count, or a chained builder hides one.
    #[test]
    fn counts_every_hit_on_a_line() {
        assert_eq!(scan(".rounded(px(4.0)).text_size(px(9.0))").len(), 2);
    }

    #[test]
    fn reports_one_based_line_numbers() {
        let hits = scan("fn a() {}\n.rounded(px(3.0))\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
    }

    /// Arithmetic on a token is still a token.
    #[test]
    fn allows_arithmetic_on_a_token() {
        assert_eq!(scan(".rounded(px(density.r_card * 2.0))").len(), 0);
    }

    /// The zoom hatch does not launder a literal off this ratchet.
    ///
    /// `density.scale(11.0)` makes a size follow the zoom, which is right for
    /// a glyph box and wrong here: the result still disagrees with every
    /// other type size in the tree, and the argument no longer starts with a
    /// digit, so without this the literal would simply vanish from the count.
    #[test]
    fn flags_a_literal_routed_through_the_zoom_hatch() {
        assert_eq!(scan(".text_size(px(density.scale(11.0)))").len(), 1);
        assert_eq!(scan(".rounded(px(self.density.scale(6.0)))").len(), 1);
    }

    /// …but a real token in the same shape is still fine, and so is the hatch
    /// anywhere this lint does not watch.
    #[test]
    fn allows_the_hatch_away_from_type_and_radius() {
        assert_eq!(scan(".w(px(density.scale(28.0)))").len(), 0);
        assert_eq!(scan(".text_size(px(typography.t_body_sm))").len(), 0);
    }
}
