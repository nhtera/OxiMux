//! Transcript entries for the Agent Chat UI thread.
//!
//! A chat thread is an ordered `Vec<ThreadEntry>`. This is the message-history
//! model OxiMux lacks today (the OSC-9999 sideband keeps only single-slot
//! `Option<String>` values, overwritten per turn). Pure data — no gpui.

use serde::{Deserialize, Serialize};

use super::tool_call::ToolCall;
use super::turn_diff::TurnFileChange;

/// One rendered item in the conversation, in arrival order.
///
/// `ToolCall` is much larger than the other variants, so every entry costs its
/// size (~424 bytes). Boxing it would reclaim the difference on the
/// non-tool entries, at the price of an indirection on the enum this app matches
/// on more than any other, and edits at ~78 construction and match sites across
/// twelve files. The transcript is bounded by what one conversation holds — a few
/// thousand entries, so under a megabyte of slack — which does not buy that.
/// Revisit if transcripts ever grow unbounded.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThreadEntry {
    /// A user prompt: the typed text plus any attached images.
    User {
        text: String,
        /// Images attached to this prompt. `#[serde(default)]` keeps older
        /// persisted transcripts (which had no images field) loadable.
        #[serde(default)]
        images: Vec<ChatImage>,
        /// Repo snapshot taken when this prompt was sent (rewind anchor).
        /// `None` when the chat cwd isn't a git repo, the engine is
        /// unsupported, or the snapshot hasn't finished yet. `#[serde(default)]`
        /// keeps older persisted transcripts loadable.
        #[serde(default)]
        checkpoint: Option<CheckpointState>,
    },
    /// An assistant message: visible text plus any thinking block.
    Assistant(AssistantMessage),
    /// A tool invocation (with its own streaming status + result).
    ToolCall(ToolCall),
    /// A context-compaction / truncation / notice divider, rendered as a subtle
    /// centered rule rather than a bubble. Produced by session import (the CLI
    /// summarized earlier history to reclaim context, or the importer capped a
    /// very long transcript, "older messages not shown"), by a live
    /// `CompactBoundary`, and by the stale-`--resume` recovery notice
    /// ("Session no longer resumable — starting fresh").
    ContextCompaction { summary: String },
    /// What a turn changed on disk, closing that turn. Rendered as a
    /// "N files changed +A −D" card, so an editing turn ends with what it did
    /// rather than only what it said.
    ///
    /// Pushed at turn end and only when the turn changed something, so a
    /// conversational turn adds nothing.
    TurnDiff {
        /// Per-file stats for the card's rows. Always populated — this is what
        /// the card renders.
        files: Vec<TurnFileChange>,
        /// The turn's unified diff, when the backend reported one (Codex
        /// `turn/diff/updated`). `Review` needs real hunks, so it is offered only
        /// when this is present. Claude/ACP/Pi report no turn diff and their edit
        /// cards carry fragments rather than line-numbered hunks, so their stats
        /// are derived but this stays `None` rather than fabricating a diff whose
        /// line numbers would be invented.
        #[serde(default)]
        diff: Option<String>,
    },
}

/// A git checkpoint anchored to a user message. `sha` is the dangling commit
/// created by the checkpoint engine (a plain string here — this crate stays
/// git-free). `show` is set once the turn is known to have changed repo state,
/// gating the "restore files" affordance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointState {
    pub sha: String,
    #[serde(default)]
    pub show: bool,
}

/// An image attached to a user prompt. `data` is standard-base64 of the raw
/// encoded image bytes — persisted verbatim in the transcript blob and sent to
/// the agent as a base64 image content block. `media_type` is the MIME string
/// (`image/png`, `image/jpeg`, `image/gif`, `image/webp`); it's both the wire
/// `media_type` and the hint used to pick a decoder for the thumbnail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatImage {
    pub media_type: String,
    pub data: String,
}

/// An assistant message. `thinking` is the (optional) extended-thinking block
/// shown collapsed; `text` is the user-visible reply. Both accumulate across
/// streaming deltas within one turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub text: String,
    pub thinking: String,
}

impl AssistantMessage {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.thinking.is_empty()
    }
}
