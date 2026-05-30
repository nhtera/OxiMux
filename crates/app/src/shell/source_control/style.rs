//! Source Control panel style tokens.
//!
//! The Source Control surface intentionally runs a touch looser than the
//! global cockpit density: 12px horizontal padding instead of 8, 12px body
//! text instead of 11. Keeping these locally scoped (instead of bumping
//! `Density` / `Typography`) leaves the rest of the cockpit chrome — top
//! bar, file explorer, terminal — at the tight default.

/// Outer horizontal padding for tabs, toolbar, filter row, commit area, and
/// section headers. Matches `px-3` (12px) in the reference layout.
pub const PAD_H: f32 = 12.0;

/// Vertical padding for the filter row (tight). Matches `py-1.5` (6px).
pub const PAD_V_TIGHT: f32 = 6.0;

/// Vertical padding for the branch-compare toolbar. Matches `py-2` (8px).
pub const PAD_V: f32 = 8.0;

/// Fixed row height for the scope-tabs strip. Definite height keeps the row
/// from being compressed by flex pressure when the file list expands below.
/// Combined with `items_end`, the active tab's 2px underline lands directly
/// on the row's 1px bottom border so the two lines unify visually.
pub const TAB_H: f32 = 32.0;

/// Primary text size used for tabs, toolbar copy, filter input, file rows,
/// commit subject placeholder, and graph subject lines. Matches `text-xs`
/// (12px) in the reference.
pub const TEXT: f32 = 12.0;

/// Secondary text size for parent paths and graph author/date meta lines.
pub const META_TEXT: f32 = 11.0;

/// Uppercase section-header text size (e.g. "STAGED CHANGES", "GRAPH").
pub const CAPS_TEXT: f32 = 11.0;

/// Inline icon size for the toolbar / filter / split-button chevron. The
/// reference uses Lucide `size-3.5` which is 14px.
pub const ICON: f32 = 14.0;

/// Commit message textarea height. A compact ~2-line composer that scrolls
/// for longer bodies rather than ballooning the panel — keeps the file list
/// and graph above the fold on a typical 13" display. Matches the reference
/// 2-row default: text-xs line-height × 2 + py-1.5 padding + border.
pub const COMMIT_H: f32 = 46.0;

/// Row height for the branch-compare toolbar.
pub const TOOLBAR_H: f32 = 32.0;

/// Primary commit button height. Reference button is noticeably chunkier
/// than the cockpit default ghost button.
pub const COMMIT_BTN_H: f32 = 32.0;

/// Minimum row height for one commit in the graph list. Lets rows with a
/// ref-badge sub-row (e.g. "(main)") grow taller, while non-ref rows stay
/// compact. The timeline column distributes connector lines across this
/// height; without a definite parent height the lines collapse to 2-3 px.
pub const COMMIT_ROW_H: f32 = 34.0;

/// Horizontal gap between the `+A` (green) and `-B` (red) line-count
/// fragments rendered to the right of a file name. Tight enough that the
/// pair reads as one decoration, loose enough that the minus sign doesn't
/// blur into the digits of the previous number.
pub const LINE_COUNT_GAP: f32 = 4.0;

/// Font size for the conflict-kind sub-label that hangs under a file name
/// on unmerged rows (e.g. "both modified"). One point smaller than the
/// secondary META_TEXT so it reads as a parenthetical to the row name.
pub const SUB_LABEL_TEXT: f32 = 10.0;
