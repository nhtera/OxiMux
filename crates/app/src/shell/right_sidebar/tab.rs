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
    /// Single-letter glyph used as the icon in the activity bar.
    /// Using letters avoids asset path uncertainty in v1; Phase 02+ can swap
    /// to gpui-component IconName paths once the asset registry is confirmed.
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
