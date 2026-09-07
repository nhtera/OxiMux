//! Every string a person reads about a listening port.
//!
//! One module because two surfaces read this data — the panel and the status
//! bar — and two surfaces wording the same fact differently is how "3 ports"
//! ends up next to a list of four. Pure: no `Context`, no store, no clock.

use std::path::Path;

use super::scan::Attribution;

/// `"1 port"` / `"N ports"`. The status bar's metric, plural-aware to match
/// the `N agents` / `N panes` segments beside it.
pub(crate) fn port_metric_label(count: usize) -> String {
    match count {
        1 => "1 port".to_string(),
        n => format!("{n} ports"),
    }
}

/// Where a detected port can actually be opened.
///
/// `localhost` rather than `127.0.0.1` deliberately: a dev server that issues
/// cookies or checks `Origin` treats the two as different sites, and the one
/// the framework prints — the one the user's session is already on — is
/// almost always `localhost`.
pub(crate) fn url_for(port: u16) -> String {
    format!("http://localhost:{port}")
}

/// What holds the socket: `"node · pid 21044"`.
///
/// The pid is here because the process name frequently is not enough — three
/// `node` rows on three ports are told apart by nothing else, and the pid is
/// what a person needs if they are about to go kill one.
pub(crate) fn origin_label(process: &str, pid: u32) -> String {
    if process.is_empty() {
        return format!("pid {pid}");
    }
    format!("{process} · pid {pid}")
}

/// Who can reach the port.
///
/// Worth saying out loud because the non-loopback case is usually an
/// accident: a dev server told to bind `0.0.0.0` so a phone on the same wifi
/// could reach it, left that way afterwards.
pub(crate) fn reach_label(loopback: bool) -> &'static str {
    if loopback {
        "local only"
    } else {
        "on your network"
    }
}

/// The whole second line of a row: what holds the port, and who can reach it.
///
/// One string rather than two elements because the two facts are read
/// together and a row that wraps between them reads as two rows.
pub(crate) fn detail_label(process: &str, pid: u32, loopback: bool) -> String {
    format!("{} · {}", origin_label(process, pid), reach_label(loopback))
}

/// Why the panel filed a port under a project — the tooltip on an owned row.
///
/// Shown rather than kept internal because "why is this listed under my other
/// worktree" is a real question with a real answer, and the answer is short.
pub(crate) fn attribution_tooltip(how: Attribution) -> &'static str {
    match how {
        Attribution::Terminal => "Started in a terminal in this window",
        Attribution::Cwd => "Running in this project's directory",
        Attribution::Command => "This project's path is in the command line",
    }
}

/// Heading for the section holding everything no project claimed.
pub(crate) fn external_section_label() -> &'static str {
    "EXTERNAL"
}

/// Shown above the external section when the machine is serving things but
/// none of them are yours.
///
/// Distinct from the fully-empty state on purpose: "nothing at all is
/// listening" and "plenty is listening, none of it yours" look identical if
/// they share copy, and only one of them means a dev server failed to start.
pub(crate) fn no_owned_hint() -> &'static str {
    "Nothing listening in your projects"
}

/// Heading for a project's section: the directory's own name.
///
/// Falls back to the full path for a root or a path that ends in `..` — rare,
/// but an empty heading is worse than a long one.
pub(crate) fn project_label(project: &Path) -> String {
    project
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| project.display().to_string())
}

/// The row's headline: the user's label when they set one, the process name
/// when they have not, and the port itself when even that is unknown.
///
/// Falling back to the process name rather than to the port means the common
/// case needs no labelling at all — `vite` on 5173 already reads correctly.
pub(crate) fn row_title(label: Option<&str>, process: &str, port: u16) -> String {
    if let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) {
        return label.to_string();
    }
    if !process.is_empty() {
        return process.to_string();
    }
    format!("port {port}")
}

/// Headline for the panel when *nothing on the machine* is listening.
///
/// Genuinely rare now that the scan is machine-wide — a desktop with no
/// listening TCP socket at all is a quiet one — which is why there is a single
/// state rather than the pair this had while the scan was scoped to open
/// terminals.
pub(crate) fn empty_headline() -> &'static str {
    "Nothing listening"
}

/// Second line under [`empty_headline`].
pub(crate) fn empty_detail() -> &'static str {
    "No process on this machine is accepting connections."
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_metric_is_plural_aware() {
        assert_eq!(port_metric_label(0), "0 ports");
        assert_eq!(port_metric_label(1), "1 port");
        assert_eq!(port_metric_label(4), "4 ports");
    }

    #[test]
    fn the_url_uses_the_host_the_framework_printed() {
        assert_eq!(url_for(5173), "http://localhost:5173");
    }

    #[test]
    fn an_unnamed_process_still_says_which_pid() {
        assert_eq!(origin_label("node", 21044), "node · pid 21044");
        assert_eq!(origin_label("", 21044), "pid 21044");
    }

    #[test]
    fn reach_names_the_surprising_case_plainly() {
        assert_eq!(reach_label(true), "local only");
        assert_eq!(reach_label(false), "on your network");
    }

    #[test]
    fn a_heading_is_the_directory_name() {
        assert_eq!(project_label(&PathBuf::from("/work/api")), "api");
        // `Path` splits on the platform's own separators, so a `\`-separated
        // path is one single component everywhere but Windows — asserting it
        // here on macOS would demand the wrong answer.
        #[cfg(windows)]
        assert_eq!(project_label(&PathBuf::from("D:\\Projects\\OxiMux")), "OxiMux");
    }

    #[test]
    fn a_heading_never_comes_back_empty() {
        // A filesystem root has no file name of its own.
        let root = PathBuf::from("/");
        assert!(!project_label(&root).is_empty());
    }

    #[test]
    fn a_row_prefers_the_users_words_then_the_kernels() {
        assert_eq!(row_title(Some("Storefront"), "node", 3000), "Storefront");
        assert_eq!(row_title(None, "node", 3000), "node");
        assert_eq!(row_title(None, "", 3000), "port 3000");
    }

    #[test]
    fn a_label_of_only_spaces_is_not_a_label() {
        // Otherwise clearing a label by selecting-all and hitting space
        // leaves a row with a blank headline and no way to tell why.
        assert_eq!(row_title(Some("   "), "node", 3000), "node");
        assert_eq!(row_title(Some("  api  "), "node", 3000), "api");
    }

    #[test]
    fn the_empty_state_never_promises_a_terminal_is_needed() {
        // The scan is machine-wide: telling a user to open a terminal would be
        // instructing them to do something that is not what finds a port.
        assert!(!empty_headline().to_lowercase().contains("terminal"));
        assert!(!empty_detail().to_lowercase().contains("terminal"));
    }

    #[test]
    fn nothing_of_yours_is_not_the_same_as_nothing_at_all() {
        assert_ne!(no_owned_hint(), empty_headline());
    }

    #[test]
    fn a_detail_line_carries_both_facts() {
        assert_eq!(
            detail_label("node", 21044, true),
            "node · pid 21044 · local only"
        );
        assert_eq!(detail_label("", 7, false), "pid 7 · on your network");
    }

    #[test]
    fn every_attribution_explains_itself() {
        for how in [Attribution::Terminal, Attribution::Cwd, Attribution::Command] {
            assert!(!attribution_tooltip(how).is_empty());
        }
        // Three distinct reasons must read as three distinct sentences.
        assert_ne!(
            attribution_tooltip(Attribution::Cwd),
            attribution_tooltip(Attribution::Command)
        );
        assert_ne!(
            attribution_tooltip(Attribution::Terminal),
            attribution_tooltip(Attribution::Cwd)
        );
    }
}
