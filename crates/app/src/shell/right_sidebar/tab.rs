//! Tab identity and visibility rules for the right activity bar.

/// Tabs available in the right sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightTab {
    Files,
    Explorer,
    Search,
    SourceControl,
    /// Past agent sessions for the project — browse + reopen as a chat tab.
    History,
}

/// Inputs that determine which tabs are visible.
pub struct TabVisibility {
    /// Whether a git repository is open in the current workspace.
    pub has_repo: bool,
}

/// Returns the ordered list of tabs that should be visible given `v`.
///
/// `Files` is intentionally **omitted** here even though the variant still
/// exists. The `oximux-editor::FileTree` model (live filesystem watcher +
/// background walker + future LSP-aware affordances) is wired through
/// `FileTreeView`, but exposing it as a second file-tab alongside
/// `Explorer` confuses users (two folder icons, no clear semantic split).
/// We hide the surface until LSP affordances justify reintroducing it
/// under a non-"Files" label. The view code stays — re-add the variant
/// to this list to expose it again. See gap report
/// `plans/reports/uiux-gap-260525-2258-right-sidebar.md` §1 + §5(X1)
/// and `plans/260525-2258-right-sidebar-uiux-parity/phase-01-…`.
///
/// Source Control is hidden when there is no repository — avoids showing a
/// broken panel before Phase 04 adds graceful no-repo handling.
pub fn visible_tabs(v: TabVisibility) -> Vec<RightTab> {
    // History is repo-independent — past sessions exist whether or not the
    // current workspace is a git repo — so it shows in both cases.
    if v.has_repo {
        vec![
            RightTab::Explorer,
            RightTab::Search,
            RightTab::SourceControl,
            RightTab::History,
        ]
    } else {
        vec![RightTab::Explorer, RightTab::Search, RightTab::History]
    }
}

impl RightTab {
    /// Asset path for the tab's SVG icon. Resolved by `CompositeAssets`:
    /// gpui-component's bundle ships `file.svg` / `search.svg` / `folder.svg`;
    /// `git-branch.svg` is shipped locally in `crates/app/assets/icons/`.
    pub fn icon_path(self) -> &'static str {
        match self {
            RightTab::Files => "icons/folder.svg",
            RightTab::Explorer => "icons/file.svg",
            RightTab::Search => "icons/search.svg",
            RightTab::SourceControl => "icons/git-branch.svg",
            RightTab::History => "icons/history.svg",
        }
    }

    /// Single-letter glyph — kept as a textual fallback / accessibility hint.
    pub fn label(self) -> &'static str {
        match self {
            RightTab::Files => "F",
            RightTab::Explorer => "E",
            RightTab::Search => "S",
            RightTab::SourceControl => "G",
            RightTab::History => "H",
        }
    }

    /// Human-readable name shown in tooltips (reserved for future use).
    pub fn title(self) -> &'static str {
        match self {
            RightTab::Files => "Files",
            RightTab::Explorer => "Explorer",
            RightTab::Search => "Search",
            RightTab::SourceControl => "Source Control",
            RightTab::History => "Session History",
        }
    }
}
