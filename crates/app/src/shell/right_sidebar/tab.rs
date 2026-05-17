//! Tab identity and visibility rules for the right activity bar.

/// Tabs available in the right sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightTab {
    Explorer,
    Search,
    SourceControl,
}

/// Inputs that determine which tabs are visible.
pub struct TabVisibility {
    /// Whether a git repository is open in the current workspace.
    pub has_repo: bool,
}

/// Returns the ordered list of tabs that should be visible given `v`.
///
/// Source Control is hidden when there is no repository — avoids showing a
/// broken panel before Phase 04 adds graceful no-repo handling.
pub fn visible_tabs(v: TabVisibility) -> Vec<RightTab> {
    if v.has_repo {
        vec![
            RightTab::Explorer,
            RightTab::Search,
            RightTab::SourceControl,
        ]
    } else {
        vec![RightTab::Explorer, RightTab::Search]
    }
}

impl RightTab {
    /// Asset path for the tab's SVG icon. Resolved by `CompositeAssets`:
    /// `file.svg` and `search.svg` come from gpui-component's bundle;
    /// `git-branch.svg` is shipped locally in `crates/app/assets/icons/`.
    pub fn icon_path(self) -> &'static str {
        match self {
            RightTab::Explorer => "icons/file.svg",
            RightTab::Search => "icons/search.svg",
            RightTab::SourceControl => "icons/git-branch.svg",
        }
    }

    /// Single-letter glyph — kept as a textual fallback / accessibility hint.
    pub fn label(self) -> &'static str {
        match self {
            RightTab::Explorer => "E",
            RightTab::Search => "S",
            RightTab::SourceControl => "G",
        }
    }

    /// Human-readable name shown in tooltips (reserved for future use).
    pub fn title(self) -> &'static str {
        match self {
            RightTab::Explorer => "Explorer",
            RightTab::Search => "Search",
            RightTab::SourceControl => "Source Control",
        }
    }
}
