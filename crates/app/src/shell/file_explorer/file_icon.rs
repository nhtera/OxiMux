//! Filename → icon-asset mapping for the file explorer.
//!
//! Pure module — no GPUI imports. `icon_for_name` looks up a file's display
//! glyph by exact basename first, then by extension. Returns `None` for
//! unknown names so the caller can fall back to the upstream
//! `IconName::File` default rather than embedding a duplicate generic glyph.

/// Exact-basename matches, lowercase. Lock files, manifests, well-known
/// configs land here so the icon survives even when the file has no
/// extension (`Dockerfile`, `Makefile`, `LICENSE`, ...).
const NAME_TABLE: &[(&str, &str)] = &[
    ("cargo.lock", "icons/file-box.svg"),
    ("cargo.toml", "icons/file-box.svg"),
    ("package.json", "icons/file-box.svg"),
    ("package-lock.json", "icons/file-box.svg"),
    ("pnpm-lock.yaml", "icons/file-box.svg"),
    ("pnpm-workspace.yaml", "icons/file-box.svg"),
    ("yarn.lock", "icons/file-box.svg"),
    ("bun.lock", "icons/file-box.svg"),
    ("bun.lockb", "icons/file-box.svg"),
    ("composer.json", "icons/file-box.svg"),
    ("composer.lock", "icons/file-box.svg"),
    ("go.mod", "icons/file-box.svg"),
    ("go.sum", "icons/file-box.svg"),
    ("gemfile", "icons/file-box.svg"),
    ("gemfile.lock", "icons/file-box.svg"),
    ("pipfile", "icons/file-box.svg"),
    ("pipfile.lock", "icons/file-box.svg"),
    ("poetry.lock", "icons/file-box.svg"),
    ("pyproject.toml", "icons/file-box.svg"),
    ("requirements.txt", "icons/file-box.svg"),
    ("dockerfile", "icons/file-cog.svg"),
    ("makefile", "icons/file-cog.svg"),
    ("cmakelists.txt", "icons/file-cog.svg"),
    ("rust-toolchain.toml", "icons/file-text.svg"),
    ("readme", "icons/file-text.svg"),
    ("readme.md", "icons/file-text.svg"),
    ("changelog", "icons/file-text.svg"),
    ("changelog.md", "icons/file-text.svg"),
    ("license", "icons/file-text.svg"),
    ("notice", "icons/file-text.svg"),
    ("authors", "icons/file-text.svg"),
    ("contributing.md", "icons/file-text.svg"),
    (".editorconfig", "icons/file-text.svg"),
    (".gitignore", "icons/file-text.svg"),
    (".gitattributes", "icons/file-text.svg"),
    (".dockerignore", "icons/file-text.svg"),
    (".npmrc", "icons/file-text.svg"),
    (".prettierrc", "icons/file-text.svg"),
    (".eslintrc", "icons/file-text.svg"),
];

/// Extension matches, lowercase (no leading dot).
const EXTENSION_TABLE: &[(&str, &str)] = &[
    ("rs", "icons/file-code.svg"),
    ("ts", "icons/file-code.svg"),
    ("tsx", "icons/file-code.svg"),
    ("js", "icons/file-code.svg"),
    ("jsx", "icons/file-code.svg"),
    ("mjs", "icons/file-code.svg"),
    ("cjs", "icons/file-code.svg"),
    ("py", "icons/file-code.svg"),
    ("go", "icons/file-code.svg"),
    ("rb", "icons/file-code.svg"),
    ("php", "icons/file-code.svg"),
    ("java", "icons/file-code.svg"),
    ("kt", "icons/file-code.svg"),
    ("swift", "icons/file-code.svg"),
    ("c", "icons/file-code.svg"),
    ("cc", "icons/file-code.svg"),
    ("cpp", "icons/file-code.svg"),
    ("h", "icons/file-code.svg"),
    ("hpp", "icons/file-code.svg"),
    ("cs", "icons/file-code.svg"),
    ("sh", "icons/file-code.svg"),
    ("zsh", "icons/file-code.svg"),
    ("bash", "icons/file-code.svg"),
    ("html", "icons/file-code.svg"),
    ("css", "icons/file-code.svg"),
    ("scss", "icons/file-code.svg"),
    ("md", "icons/file-text.svg"),
    ("markdown", "icons/file-text.svg"),
    ("txt", "icons/file-text.svg"),
    ("rst", "icons/file-text.svg"),
    ("xml", "icons/file-cog.svg"),
    ("yml", "icons/file-cog.svg"),
    ("yaml", "icons/file-cog.svg"),
    ("toml", "icons/file-text.svg"),
    ("json", "icons/file-cog.svg"),
    ("ini", "icons/file-text.svg"),
    ("conf", "icons/file-text.svg"),
    ("plist", "icons/file-cog.svg"),
];

/// Returns the asset path for the icon that should represent this file.
/// `None` means use the default (`IconName::File`).
pub fn icon_for_name(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if let Some((_, path)) = NAME_TABLE.iter().find(|(k, _)| *k == lower) {
        return Some(*path);
    }
    let ext = lower.rsplit_once('.').map(|(_, e)| e)?;
    EXTENSION_TABLE
        .iter()
        .find(|(k, _)| *k == ext)
        .map(|(_, p)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cargo_lock_by_name_lowercase() {
        assert_eq!(icon_for_name("Cargo.lock"), Some("icons/file-box.svg"));
    }

    #[test]
    fn matches_rust_source_by_extension() {
        assert_eq!(icon_for_name("main.rs"), Some("icons/file-code.svg"));
    }

    #[test]
    fn matches_xml_extension_to_cog() {
        assert_eq!(
            icon_for_name("repomix-output.xml"),
            Some("icons/file-cog.svg")
        );
    }

    #[test]
    fn matches_dotfile_by_exact_name() {
        assert_eq!(icon_for_name(".gitignore"), Some("icons/file-text.svg"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(icon_for_name("unknown.xyz"), None);
        assert_eq!(icon_for_name("noextension"), None);
    }

    #[test]
    fn ds_store_no_match() {
        // Hidden macOS file — no name/ext entry, so caller uses default glyph.
        assert_eq!(icon_for_name(".DS_Store"), None);
    }

    #[test]
    fn toml_uses_text_unless_overridden() {
        // Generic toml → text, but pyproject.toml → box via name table.
        assert_eq!(icon_for_name("config.toml"), Some("icons/file-text.svg"));
        assert_eq!(icon_for_name("pyproject.toml"), Some("icons/file-box.svg"));
    }
}
