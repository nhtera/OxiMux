//! Quoting a path so a shell reads it back as exactly one word, and refusing
//! the paths that cannot be typed safely at all.
//!
//! A file name may legally contain control bytes; typed at a prompt, `^C` or a
//! carriage return would act instead of appear — a name could run a command.
//! Such paths are never typed ([`has_control`]). Everything else is quoted
//! only when it needs to be, so the common path reads as the user would type
//! it.

/// Bytes a shell word can hold unquoted without any meaning to the shell.
fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '~' | '@' | '%' | '+' | '=' | ':' | ',' | '-')
}

/// `true` when `s` holds a C0 control, DEL or a C1 control — anything a
/// terminal would act on rather than show.
pub fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// Quote `path` for a POSIX shell (sh, bash, zsh, fish read single quotes the
/// same way for this). Unquoted when every character is plain; otherwise
/// wrapped in single quotes with each `'` written as `'\''`. A leading `-`
/// (an option) or `~` (another user's home, `~foo`) gets `./`; only a `~/`
/// the caller wrote for the home directory itself stays outside the quotes so
/// the shell still expands it.
pub fn posix_quote(path: &str) -> String {
    let path = if path.starts_with('-') || (path.starts_with('~') && !path.starts_with("~/")) {
        format!("./{path}")
    } else {
        path.to_owned()
    };
    // A leading `=` is zsh's `=cmd` expansion: never bare.
    if !path.is_empty() && !path.starts_with('=') && path.chars().all(is_plain) {
        return path;
    }
    let (home, rest) = match path.strip_prefix("~/") {
        Some(rest) => ("~/", rest),
        None => ("", path.as_str()),
    };
    format!("{home}'{}'", rest.replace('\'', r"'\''"))
}

/// How the shell in a pane reads a typed word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDialect {
    /// sh, bash (Git Bash too), zsh, fish.
    Posix,
    PowerShell,
    /// `cmd.exe`.
    Cmd,
}

impl ShellDialect {
    /// The dialect of the shell `program` (a path or a bare name).
    pub fn of_program(program: &str) -> Self {
        let name = program.rsplit(['/', '\\']).next().unwrap_or(program).to_ascii_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "cmd" => Self::Cmd,
            "powershell" | "pwsh" => Self::PowerShell,
            _ => Self::Posix,
        }
    }
}

/// Quote `path` for `dialect`, or `None` when it cannot be typed there
/// safely (a control byte anywhere; for `cmd`, see [`cmd_quote`]).
pub fn quote_for(path: &str, dialect: ShellDialect) -> Option<String> {
    if has_control(path) {
        return None;
    }
    match dialect {
        ShellDialect::Posix => Some(posix_quote(path)),
        ShellDialect::PowerShell => Some(powershell_quote(path)),
        ShellDialect::Cmd => cmd_quote(path),
    }
}

/// Quote `path` for `cmd.exe`: bare when it holds only letters, digits and
/// path punctuation, else in double quotes. `cmd` has no way to quote `"`,
/// and expands `%VAR%` (and `!VAR!`) even inside quotes, so a path holding
/// any of those is refused rather than typed.
pub fn cmd_quote(path: &str) -> Option<String> {
    if path.contains(['"', '%', '!']) {
        return None;
    }
    let bare = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '\\' | '/' | ':');
    if !path.is_empty() && path.chars().all(bare) {
        return Some(path.to_owned());
    }
    // A program reading its command line takes `\"` as an escaped quote:
    // a trailing backslash is doubled so the closing quote still closes.
    let tail = if path.ends_with('\\') { "\\" } else { "" };
    Some(format!("\"{path}{tail}\""))
}

/// The characters PowerShell reads as a single quote: the ASCII one and the
/// typographic ones (U+2018–U+201B), any of which ends a single-quoted string.
const PS_SINGLE_QUOTES: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];

/// Escape a value for a PowerShell single-quoted literal: nothing interpolates
/// inside one, so only the quotes need doubling — every character PowerShell
/// takes as one, or a file named `a’;calc;’b` would end the string early and
/// run `calc`.
pub fn ps_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if PS_SINGLE_QUOTES.contains(&c) {
            out.push(c);
        }
        out.push(c);
    }
    out
}

/// Quote `path` for PowerShell: bare when plain, else a single-quoted literal.
pub fn powershell_quote(path: &str) -> String {
    let path = if path.starts_with('-') { format!(".\\{path}") } else { path.to_owned() };
    // `,` builds an array and `@` splats in PowerShell: those get quoted too.
    if !path.is_empty() && path.chars().all(|c| (is_plain(c) && !matches!(c, ',' | '@')) || c == '\\') {
        return path;
    }
    format!("'{}'", ps_single_quote(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_stay_bare() {
        assert_eq!(posix_quote("src/main.rs"), "src/main.rs");
        assert_eq!(posix_quote("~/notes.md"), "~/notes.md");
        assert_eq!(posix_quote("a-b_c.d+e=f@g%h:i,j"), "a-b_c.d+e=f@g%h:i,j");
    }

    #[test]
    fn anything_else_is_single_quoted() {
        assert_eq!(posix_quote("my file.txt"), "'my file.txt'");
        assert_eq!(posix_quote("it's here"), r"'it'\''s here'");
        assert_eq!(posix_quote("$HOME/x"), "'$HOME/x'");
        assert_eq!(posix_quote("a;rm -rf b"), "'a;rm -rf b'");
        assert_eq!(posix_quote("café/naïve"), "'café/naïve'");
        assert_eq!(posix_quote("~/my docs"), "~/'my docs'");
        assert_eq!(posix_quote(""), "''");
    }

    #[test]
    fn shell_specific_characters_are_quoted() {
        assert_eq!(posix_quote("=ls"), "'=ls'");
        assert_eq!(posix_quote("a=b"), "a=b");
        assert_eq!(powershell_quote("a,b"), "'a,b'");
        assert_eq!(powershell_quote("@x"), "'@x'");
    }

    #[test]
    fn a_leading_tilde_that_is_not_home_is_a_file() {
        assert_eq!(posix_quote("~foo"), "./~foo");
        assert_eq!(posix_quote("~/x"), "~/x");
    }

    #[test]
    fn a_leading_dash_is_not_an_option() {
        assert_eq!(posix_quote("-rf"), "./-rf");
        assert_eq!(posix_quote("-my file"), "'./-my file'");
        assert_eq!(powershell_quote("-x"), ".\\-x");
    }

    #[test]
    fn control_bytes_are_refused() {
        for bad in ["a\x03b", "a\rb", "a\nb", "a\x1b[31m", "a\x7f", "a\u{9b}b", "\t"] {
            assert!(has_control(bad), "{bad:?}");
        }
        assert!(!has_control("plain/path with spaces é"));
    }

    #[test]
    fn powershell_quoting() {
        assert_eq!(powershell_quote(r"C:\Users\me\x.txt"), r"C:\Users\me\x.txt");
        assert_eq!(powershell_quote("my file"), "'my file'");
        assert_eq!(powershell_quote("it's"), "'it''s'");
        assert_eq!(ps_single_quote("$env:PATH"), "$env:PATH");
    }

    #[test]
    fn typographic_quotes_cannot_end_a_powershell_string() {
        assert_eq!(powershell_quote("a\u{2019};calc;\u{2019}b"), "'a\u{2019}\u{2019};calc;\u{2019}\u{2019}b'");
        for q in PS_SINGLE_QUOTES {
            let quoted = powershell_quote(&format!("x{q}y"));
            assert!(quoted.contains(&format!("{q}{q}")), "{q:?} doubled in {quoted}");
        }
    }

    #[test]
    fn cmd_quoting_and_refusals() {
        assert_eq!(cmd_quote(r"C:\Users\me\x.txt").as_deref(), Some(r"C:\Users\me\x.txt"));
        assert_eq!(cmd_quote("a&calc").as_deref(), Some("\"a&calc\""));
        assert_eq!(cmd_quote("my file").as_deref(), Some("\"my file\""));
        assert_eq!(cmd_quote(r"my dir\").as_deref(), Some(r#""my dir\\""#));
        // `,`, `;` and `=` split arguments in cmd: quoted, never bare.
        assert_eq!(cmd_quote("a,b").as_deref(), Some("\"a,b\""));
        for bad in ["a\"b", "%PATH%", "x!y"] {
            assert_eq!(cmd_quote(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_dialect_follows_the_shell_program() {
        assert_eq!(ShellDialect::of_program(r"C:\Windows\System32\cmd.exe"), ShellDialect::Cmd);
        assert_eq!(ShellDialect::of_program(r"C:\Program Files\PowerShell\7\pwsh.exe"), ShellDialect::PowerShell);
        assert_eq!(ShellDialect::of_program("powershell.exe"), ShellDialect::PowerShell);
        assert_eq!(ShellDialect::of_program(r"C:\Program Files\Git\bin\bash.exe"), ShellDialect::Posix);
        assert_eq!(ShellDialect::of_program("/bin/zsh"), ShellDialect::Posix);
        assert_eq!(quote_for("a\x03b", ShellDialect::Posix), None);
        assert_eq!(quote_for("it's", ShellDialect::Posix).as_deref(), Some(r"'it'\''s'"));
    }
}
