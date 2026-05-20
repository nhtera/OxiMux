//! Pure scope-tab data. `All` shows the changed-files list + diff + commit
//! graph; `Uncommitted` hides the graph (Phase 05) so the user can focus on
//! the staging area.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceControlScope {
    #[default]
    All,
    Uncommitted,
}

impl SourceControlScope {
    /// Whether the commit-graph section is visible in this scope.
    pub fn shows_graph(self) -> bool {
        matches!(self, SourceControlScope::All)
    }

    /// Display label for the tab strip.
    pub fn label(self) -> &'static str {
        match self {
            SourceControlScope::All => "All",
            SourceControlScope::Uncommitted => "Uncommitted",
        }
    }
}
