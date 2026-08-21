//! Every string a person reads about a listening port.
//!
//! One module because two surfaces read this data — the panel and the status
//! bar — and two surfaces wording the same fact differently is how "3 ports"
//! ends up next to a list of four. Pure: no `Context`, no store, no clock.

use std::path::Path;

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

/// Headline for the panel when nothing is listening.
///
/// The two cases are genuinely different and must not share copy: no
/// terminals open at all is a "do this first"; terminals open with nothing
/// serving is the ordinary resting state and needs no instruction.
pub(crate) fn empty_headline(has_terminals: bool) -> &'static str {
    if has_terminals {
        "Nothing listening"
    } else {
        "No terminals open"
    }
}

/// Second line under [`empty_headline`].
pub(crate) fn empty_detail(has_terminals: bool) -> &'static str {
    if has_terminals {
        "Start a dev server in a terminal and its port appears here."
    } else {
        "Ports are found by looking inside the terminals you have open."
    }
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
    fn the_two_empty_states_do_not_share_copy() {
        assert_ne!(empty_headline(true), empty_headline(false));
        assert_ne!(empty_detail(true), empty_detail(false));
    }
}
