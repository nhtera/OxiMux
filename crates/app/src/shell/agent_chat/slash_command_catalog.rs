//! Enrichment for the slash-command palette: the stream-json `init` message
//! advertises command *names* only, so descriptions + grouping are recovered by
//! scanning the on-disk command/skill definitions (`~/.claude/skills/*/SKILL.md`,
//! plugin commands/skills under `~/.claude/plugins/cache`, project
//! `.claude/commands/*.md`) plus a small built-in map. The scan is pure I/O run
//! off the main thread; the palette looks up each advertised name in the result.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Which section a command sorts under in the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandGroup {
    /// Native CLI commands and plugin commands.
    BuiltIn,
    /// Skill-catalog entries (a `SKILL.md`), user or plugin.
    Skill,
}

impl CommandGroup {
    pub fn label(self) -> &'static str {
        match self {
            CommandGroup::BuiltIn => "Built-in",
            CommandGroup::Skill => "Skills",
        }
    }
}

/// Enrichment for one advertised command name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMeta {
    pub description: Option<String>,
    /// Usage hint from the command's `argument-hint` frontmatter (e.g.
    /// `cm|cp|pr|merge [args]`). Surfaced once the command is accepted so the
    /// user knows what arguments to type next; `None` when the command takes no
    /// arguments or advertises no hint.
    pub argument_hint: Option<String>,
    pub group: CommandGroup,
    /// Provider tag shown muted on the right (e.g. a plugin name); `None` for
    /// native/user entries.
    pub source_label: Option<String>,
}

/// Command name → enrichment. Advertised names absent from this map render as a
/// bare name under Built-in.
pub type CommandCatalog = HashMap<String, CommandMeta>;

/// Build the catalog for a chat rooted at `cwd`. Best-effort: any unreadable
/// source is skipped, never panics. Blocking I/O — call from a background task.
pub fn discover_catalog(cwd: &Path) -> CommandCatalog {
    let mut cat = built_in_catalog();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        scan_user_skills(&home.join(".claude/skills"), &mut cat);
        scan_plugin_cache(&home.join(".claude/plugins/cache"), &mut cat);
    }
    scan_project_commands(&cwd.join(".claude/commands"), &mut cat);
    cat
}

/// Descriptions for the common native CLI commands (which have no on-disk file).
fn built_in_catalog() -> CommandCatalog {
    const BUILT_INS: &[(&str, &str)] = &[
        ("clear", "Clear the conversation and free up context"),
        ("compact", "Summarize the conversation to reclaim context"),
        ("context", "Show current context-window usage"),
        ("config", "Open the configuration panel"),
        ("usage", "Show plan usage and rate limits"),
        ("usage-credits", "Show remaining usage credits"),
        ("extra-usage", "Buy additional usage"),
        ("agents", "Manage subagents"),
        ("review", "Review a pull request"),
        ("security-review", "Run a security review of the changes"),
        ("recap", "Recap what happened while you were away"),
        ("init", "Bootstrap a CLAUDE.md for this project"),
        ("reload-skills", "Reload skills from disk"),
        ("heapdump", "Write a heap snapshot for debugging"),
        ("insights", "Show usage insights"),
        ("goal", "Set the session goal"),
    ];
    BUILT_INS
        .iter()
        .map(|(name, desc)| {
            (
                (*name).to_string(),
                CommandMeta {
                    description: Some((*desc).to_string()),
                    argument_hint: None,
                    group: CommandGroup::BuiltIn,
                    source_label: None,
                },
            )
        })
        .collect()
}

/// `~/.claude/skills/<name>/SKILL.md` → command `/<name>`, group Skills.
fn scan_user_skills(skills_dir: &Path, cat: &mut CommandCatalog) {
    let Ok(entries) = fs::read_dir(skills_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let fm = read_frontmatter(&entry.path().join("SKILL.md"));
        cat.insert(
            name,
            CommandMeta {
                description: fm.description,
                argument_hint: fm.argument_hint,
                group: CommandGroup::Skill,
                source_label: None,
            },
        );
    }
}

/// `~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/{commands,skills}`.
/// Commands → `/<plugin>:<file>` (Built-in); skills → `/<plugin>:<skill>` (Skills).
/// Both tagged with the plugin name on the right.
fn scan_plugin_cache(cache_dir: &Path, cat: &mut CommandCatalog) {
    let Ok(markets) = fs::read_dir(cache_dir) else { return };
    for market in markets.flatten() {
        let Ok(plugins) = fs::read_dir(market.path()) else { continue };
        for plugin in plugins.flatten() {
            let plugin_name = plugin.file_name().to_string_lossy().into_owned();
            let Ok(versions) = fs::read_dir(plugin.path()) else { continue };
            for version in versions.flatten() {
                scan_plugin_commands(&version.path().join("commands"), &plugin_name, cat);
                scan_plugin_skills(&version.path().join("skills"), &plugin_name, cat);
            }
        }
    }
}

fn scan_plugin_commands(dir: &Path, plugin: &str, cat: &mut CommandCatalog) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let fm = read_frontmatter(&path);
        cat.insert(
            format!("{plugin}:{stem}"),
            CommandMeta {
                description: fm.description,
                argument_hint: fm.argument_hint,
                group: CommandGroup::BuiltIn,
                source_label: Some(plugin.to_string()),
            },
        );
    }
}

fn scan_plugin_skills(dir: &Path, plugin: &str, cat: &mut CommandCatalog) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let skill = entry.file_name().to_string_lossy().into_owned();
        let fm = read_frontmatter(&entry.path().join("SKILL.md"));
        cat.insert(
            format!("{plugin}:{skill}"),
            CommandMeta {
                description: fm.description,
                argument_hint: fm.argument_hint,
                group: CommandGroup::Skill,
                source_label: Some(plugin.to_string()),
            },
        );
    }
}

/// `<cwd>/.claude/commands/<name>.md` → command `/<name>`, group Built-in.
fn scan_project_commands(dir: &Path, cat: &mut CommandCatalog) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) {
            cat.entry(stem).or_insert_with(|| {
                let fm = read_frontmatter(&path);
                CommandMeta {
                    description: fm.description,
                    argument_hint: fm.argument_hint,
                    group: CommandGroup::BuiltIn,
                    source_label: None,
                }
            });
        }
    }
}

/// The frontmatter fields the palette cares about, read from one file pass.
#[derive(Default)]
struct FrontMeta {
    description: Option<String>,
    argument_hint: Option<String>,
}

/// Read a command/skill definition's frontmatter once, pulling both the
/// description and the `argument-hint`. Best-effort: an unreadable file yields
/// empty fields rather than an error.
fn read_frontmatter(file: &Path) -> FrontMeta {
    let Ok(content) = fs::read_to_string(file) else {
        return FrontMeta::default();
    };
    FrontMeta {
        description: frontmatter_field(&content, "description"),
        argument_hint: frontmatter_field(&content, "argument-hint"),
    }
}

/// Extract a scalar frontmatter field from the leading `--- … ---` block.
/// Handles quoted (`"x"`/`'x'`), plain, and block-scalar (`|`/`>`) values —
/// taking the first non-empty line of a block. Truncates over-long values.
fn frontmatter_field(content: &str, field: &str) -> Option<String> {
    let body = content.strip_prefix("---")?;
    let end = body.find("\n---")?;
    let front = &body[..end];
    let mut lines = front.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(field)
            && let Some(val) = rest.strip_prefix(':')
        {
            let val = val.trim();
            if val.is_empty() || matches!(val, "|" | ">" | "|-" | ">-" | "|+" | ">+") {
                // Block scalar: use the first non-empty continuation line.
                let cont = lines.by_ref().map(str::trim).find(|l| !l.is_empty());
                return cont.map(truncate_desc);
            }
            return Some(truncate_desc(unquote(val)));
        }
    }
    None
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

fn truncate_desc(s: impl AsRef<str>) -> String {
    // Generous cap: the palette row visually clips to one line, but the full text
    // is shown in a hover tooltip, so keep enough to be useful there (while still
    // bounding a pathologically long value).
    const MAX: usize = 400;
    let s = s.as_ref().trim();
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_plain_and_block_descriptions() {
        let quoted = "---\nname: r\ndescription: \"Research things\"\n---\nbody";
        assert_eq!(frontmatter_field(quoted, "description").as_deref(), Some("Research things"));

        let plain = "---\ndescription: Automate browsers and apps\nother: x\n---\n";
        assert_eq!(frontmatter_field(plain, "description").as_deref(), Some("Automate browsers and apps"));

        let block = "---\ndescription: |\n  First line of block\n  second line\n---\n";
        assert_eq!(frontmatter_field(block, "description").as_deref(), Some("First line of block"));
    }

    #[test]
    fn parses_argument_hint_field() {
        // The `argument-hint` field (with a hyphen) is read the same way as
        // `description` — it drives the usage hint shown after a command accepts.
        let fm = "---\nname: g\nargument-hint: \"cm|cp|pr|merge [args]\"\n---\nbody";
        assert_eq!(
            frontmatter_field(fm, "argument-hint").as_deref(),
            Some("cm|cp|pr|merge [args]")
        );
        // Absent field → None (a command that takes no arguments).
        let none = "---\nname: g\ndescription: x\n---\n";
        assert_eq!(frontmatter_field(none, "argument-hint"), None);
    }

    #[test]
    fn field_prefix_does_not_false_match() {
        // `description-hint:` must not satisfy a lookup for `description`.
        let fm = "---\ndescription-hint: nope\nname: x\n---\n";
        assert_eq!(frontmatter_field(fm, "description"), None);
    }

    #[test]
    fn missing_frontmatter_or_field_is_none() {
        assert_eq!(frontmatter_field("no frontmatter here", "description"), None);
        assert_eq!(frontmatter_field("---\nname: x\n---\n", "description"), None);
    }

    #[test]
    fn over_long_description_is_truncated() {
        let long = "x".repeat(600);
        let fm = format!("---\ndescription: {long}\n---\n");
        let got = frontmatter_field(&fm, "description").unwrap();
        assert!(got.chars().count() <= 401, "got {} chars", got.chars().count());
        assert!(got.ends_with('…'));
    }

    #[test]
    fn built_in_catalog_has_known_natives() {
        let cat = built_in_catalog();
        assert_eq!(cat["compact"].group, CommandGroup::BuiltIn);
        assert!(cat["compact"].description.is_some());
        assert!(cat.contains_key("context"));
    }
}
