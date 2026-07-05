//! `ChatThread` — the in-memory conversation state machine.
//!
//! Folds a stream of `ThreadEvent`s into an ordered `Vec<ThreadEntry>` (the
//! message history OxiMux lacks today). Pure and gpui-free: the app crate
//! owns a GPUI entity that holds a `ChatThread`, calls `apply` on each event,
//! and repaints.
//!
//! Reconciliation: streaming deltas build the current assistant block; the
//! FIRST finalized block of that kind replaces the streamed text (so it isn't
//! doubled), and any FURTHER finalized block of the same kind in the same
//! message is appended (a message may legitimately carry more than one text
//! block). A tool call ends the current assistant block, so later text starts
//! a fresh assistant message.

use super::background_task::{BackgroundTask, TaskStatus};
use super::entry::{AssistantMessage, ChatImage, CheckpointState, ThreadEntry};
use super::event::{ThreadEvent, TurnUsage};
use super::question::QuestionRequest;
use super::tool_call::{PermissionRequest, ToolCall, ToolCallStatus};

#[derive(Debug, Default)]
pub struct ChatThread {
    pub entries: Vec<ThreadEntry>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Command names advertised at session init (from `SessionInit`), for the
    /// composer's slash-command palette. Empty until init arrives or when the
    /// backend advertises none. Rides the restore snapshot so a resumed session
    /// keeps its palette without waiting for a fresh init.
    pub slash_commands: Vec<String>,
    /// Latest one-line turn summary (`post_turn_summary`), for a status chip.
    pub last_summary: Option<String>,
    /// Latest transport/protocol error, surfaced non-fatally.
    pub last_error: Option<String>,
    /// Whether a turn is currently in flight (between a user send and
    /// `TurnEnded`). Drives the composer's send/stop affordance.
    pub turn_active: bool,
    /// Latest per-turn token/cost usage (from the most recent `TurnEnded`), for
    /// the usage footer. `None` until a turn reports usage.
    pub usage: Option<TurnUsage>,
    /// Background tasks (subagents + background bash) folded from `task_*` events,
    /// in start order, for the Background Tasks panel. In-memory only (not
    /// persisted): a task's output files are ephemeral, so a restored chat starts
    /// with none rather than dangling references.
    pub background_tasks: Vec<BackgroundTask>,
    /// Index of the assistant entry currently being streamed, if any.
    current_assistant: Option<usize>,
    /// True once a text delta has streamed into the current assistant block
    /// but its finalized text hasn't reconciled it yet.
    text_streaming: bool,
    /// Same, for the thinking block.
    thinking_streaming: bool,
}

impl ChatThread {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a thread from a persisted transcript on session restore. Seeds
    /// `entries` + the `session_id`/`model` needed to `--resume`, and leaves the
    /// streaming/turn flags at rest (no turn is in flight after a relaunch).
    ///
    /// **Fail-closed:** any tool call left `WaitingForConfirmation` when the app
    /// last quit is downgraded to `Rejected`. The process that asked is dead, so
    /// the request can never be answered — restoring it as pending would strand a
    /// permanently-unanswerable prompt in the UI (and, worse, imply the edit is
    /// still gated). Denying is the safe default, matching the live-disconnect
    /// fail-closed rule in the view.
    pub fn rehydrated(
        session_id: Option<String>,
        model: Option<String>,
        mut entries: Vec<ThreadEntry>,
        slash_commands: Vec<String>,
    ) -> Self {
        for entry in &mut entries {
            if let ThreadEntry::ToolCall(tc) = entry
                && matches!(
                    tc.status,
                    ToolCallStatus::WaitingForConfirmation(_) | ToolCallStatus::AwaitingAnswer(_)
                )
            {
                tc.status = ToolCallStatus::Rejected;
            }
        }
        Self {
            entries,
            session_id,
            model,
            slash_commands,
            ..Self::default()
        }
    }

    /// Record a user prompt (called when we send one to the agent).
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.push_user_message_with_images(text, Vec::new());
    }

    /// Record a user prompt that carries attached images.
    pub fn push_user_message_with_images(
        &mut self,
        text: impl Into<String>,
        images: Vec<ChatImage>,
    ) {
        self.entries.push(ThreadEntry::User { text: text.into(), images, checkpoint: None });
        self.end_assistant_window();
        // A new turn begins: the prior turn's usage/summary no longer apply.
        // Clear them so the footer never implies stale numbers belong to this
        // turn (e.g. if this turn ends in error before reporting any usage).
        self.usage = None;
        self.last_summary = None;
        self.turn_active = true;
    }

    /// Fold one decoded event into the transcript.
    pub fn apply(&mut self, event: &ThreadEvent) {
        match event {
            ThreadEvent::SessionInit { session_id, model, permission_mode, slash_commands } => {
                self.session_id = Some(session_id.clone());
                self.model = Some(model.clone());
                self.permission_mode = Some(permission_mode.clone());
                // Keep the last non-empty list: a resumed session may seed the
                // palette before init, and a later init should never blank it.
                if !slash_commands.is_empty() {
                    self.slash_commands = slash_commands.clone();
                }
            }
            ThreadEvent::AssistantTextDelta(t) => {
                self.assistant_mut().text.push_str(t);
                self.text_streaming = true;
            }
            ThreadEvent::ThinkingDelta(t) => {
                self.assistant_mut().thinking.push_str(t);
                self.thinking_streaming = true;
            }
            // First finalized block reconciles (replaces) the streamed value;
            // a further finalized block of the same kind appends.
            ThreadEvent::AssistantText(t) => {
                if self.text_streaming {
                    self.assistant_mut().text = t.clone();
                    self.text_streaming = false;
                } else {
                    let m = self.assistant_mut();
                    if !m.text.is_empty() {
                        m.text.push('\n');
                    }
                    m.text.push_str(t);
                }
            }
            ThreadEvent::AssistantThinking(t) => {
                if self.thinking_streaming {
                    self.assistant_mut().thinking = t.clone();
                    self.thinking_streaming = false;
                } else {
                    let m = self.assistant_mut();
                    if !m.thinking.is_empty() {
                        m.thinking.push('\n');
                    }
                    m.thinking.push_str(t);
                }
            }
            ThreadEvent::ToolCallStarted { id, name, input } => {
                self.entries.push(ThreadEntry::ToolCall(ToolCall::new(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                )));
                // A tool call ends the current assistant block; later text
                // starts a fresh assistant message.
                self.end_assistant_window();
            }
            ThreadEvent::ToolResult { tool_use_id, content, is_error } => {
                // A result for an unknown id is silently dropped (no matching
                // tool call to attach it to) rather than mutating the wrong
                // entry — see the `tool_result_for_unknown_id_is_ignored` test.
                if let Some(tc) = self.tool_call_mut(tool_use_id) {
                    tc.result = Some(content.clone());
                    tc.status = if *is_error {
                        ToolCallStatus::Failed(content.clone())
                    } else {
                        ToolCallStatus::Completed
                    };
                }
            }
            ThreadEvent::PermissionRequested {
                request_id, tool_use_id, tool_name, input, description, suggestions,
            } => {
                let req = PermissionRequest {
                    request_id: request_id.clone(),
                    description: description.clone(),
                    suggestions: suggestions.clone(),
                };
                let matched = tool_use_id
                    .as_deref()
                    .and_then(|id| self.tool_call_mut(id));
                match matched {
                    Some(tc) => tc.status = ToolCallStatus::WaitingForConfirmation(req),
                    None => {
                        // Permission arrived without a matching tool_use block
                        // (unexpected ordering, or a null tool_use_id) —
                        // synthesize the card anyway so the user can still act.
                        let mut tc = ToolCall::new(
                            tool_use_id.clone().unwrap_or_else(|| request_id.clone()),
                            tool_name.clone(),
                            input.clone(),
                        );
                        tc.status = ToolCallStatus::WaitingForConfirmation(req);
                        self.entries.push(ThreadEntry::ToolCall(tc));
                    }
                }
            }
            ThreadEvent::QuestionAsked { request_id, tool_use_id, questions } => {
                // Ignore a malformed/empty question set rather than entering an
                // unanswerable `AwaitingAnswer` — an empty-indexed card render
                // would panic, and there is nothing for the user to answer. The
                // CLI validates 1-4 questions, so this only guards garbled input.
                if !questions.is_empty() {
                    let req = QuestionRequest {
                        request_id: request_id.clone(),
                        questions: questions.clone(),
                    };
                    let matched = tool_use_id.as_deref().and_then(|id| self.tool_call_mut(id));
                    match matched {
                        Some(tc) => tc.status = ToolCallStatus::AwaitingAnswer(req),
                        None => {
                            // No matching tool_use block (unexpected ordering /
                            // null id) — synthesize the card anyway so the user
                            // can still answer, keyed on the tool_use_id (fallback
                            // request_id).
                            let mut tc = ToolCall::new(
                                tool_use_id.clone().unwrap_or_else(|| request_id.clone()),
                                "AskUserQuestion",
                                serde_json::Value::Null,
                            );
                            tc.status = ToolCallStatus::AwaitingAnswer(req);
                            self.entries.push(ThreadEntry::ToolCall(tc));
                        }
                    }
                }
            }
            ThreadEvent::TurnSummary { detail, .. } => {
                self.last_summary = Some(detail.clone());
                self.end_assistant_window();
            }
            ThreadEvent::TurnEnded { is_error, result, usage } => {
                self.turn_active = false;
                self.end_assistant_window();
                if let Some(u) = usage {
                    self.usage = Some(u.clone());
                }
                if *is_error {
                    self.last_error = result.clone().or(Some("turn ended with error".into()));
                }
            }
            ThreadEvent::BackgroundTaskStarted { task_id, tool_use_id, kind, description } => {
                // Idempotent: a duplicate start (defensive against a re-emit) just
                // refreshes the existing record rather than adding a second row.
                if let Some(t) = self.background_task_mut(task_id) {
                    t.tool_use_id = tool_use_id.clone();
                    t.kind = kind.clone();
                    t.description = description.clone();
                } else {
                    self.background_tasks.push(BackgroundTask::started(
                        task_id.clone(),
                        tool_use_id.clone(),
                        kind.clone(),
                        description.clone(),
                    ));
                }
            }
            ThreadEvent::BackgroundTaskProgress { task_id, last_tool } => {
                if let Some(t) = self.background_task_mut(task_id) {
                    t.tool_uses = t.tool_uses.saturating_add(1);
                    if last_tool.is_some() {
                        t.last_tool = last_tool.clone();
                    }
                }
            }
            ThreadEvent::BackgroundTaskFinished {
                task_id,
                failed,
                ended_at_ms,
                summary,
                output_file,
            } => {
                if let Some(t) = self.background_task_mut(task_id) {
                    // Terminal status wins; two completion signals (task_updated +
                    // task_notification) may both arrive — merge, don't overwrite
                    // populated fields with a later `None`.
                    t.status = if *failed { TaskStatus::Failed } else { TaskStatus::Completed };
                    if ended_at_ms.is_some() {
                        t.ended_at_ms = *ended_at_ms;
                    }
                    if summary.is_some() {
                        t.summary = summary.clone();
                    }
                    if output_file.is_some() {
                        t.output_file = output_file.clone();
                    }
                }
            }
            ThreadEvent::Error(msg) => {
                self.last_error = Some(msg.clone());
                self.turn_active = false;
            }
        }
    }

    /// Mutable lookup of a background task by its `task_id`.
    fn background_task_mut(&mut self, task_id: &str) -> Option<&mut BackgroundTask> {
        self.background_tasks.iter_mut().find(|t| t.task_id == task_id)
    }

    /// Count of background tasks still running — the header badge value.
    pub fn running_task_count(&self) -> usize {
        self.background_tasks.iter().filter(|t| t.is_running()).count()
    }

    /// Interrupt the current turn (the user pressed Stop). Clears the turn flag
    /// (drops the spinner) but deliberately leaves the current assistant block
    /// OPEN: after SIGINT the agent still flushes a few trailing events (more
    /// deltas, then a finalized `assistant` block) before its stdout closes, and
    /// that finalized block must reconcile into the in-progress block rather than
    /// starting a second one — closing the window here would duplicate the reply.
    /// The turn's own `TurnEnded`/EOF closes the window normally afterward.
    /// Fail-closed: any tool call still `WaitingForConfirmation` is downgraded to
    /// `Rejected` — the process is being killed, so the request can never be
    /// answered, exactly like a live disconnect.
    pub fn interrupt(&mut self) {
        self.turn_active = false;
        for entry in &mut self.entries {
            if let ThreadEntry::ToolCall(tc) = entry
                && matches!(
                    tc.status,
                    ToolCallStatus::WaitingForConfirmation(_) | ToolCallStatus::AwaitingAnswer(_)
                )
            {
                tc.status = ToolCallStatus::Rejected;
            }
        }
    }

    /// Ordinal (0-based, counting only user entries) → entry index.
    pub fn user_entry_index(&self, ordinal: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, ThreadEntry::User { .. }))
            .nth(ordinal)
            .map(|(i, _)| i)
    }

    /// Attach the just-created snapshot to the LAST user entry, if it is still
    /// the one at `expected_index` (the thread may have moved on while the
    /// checkpoint was being created in the background).
    pub fn attach_checkpoint(&mut self, expected_index: usize, sha: String) {
        if let Some(ThreadEntry::User { checkpoint, .. }) = self.entries.get_mut(expected_index) {
            *checkpoint = Some(CheckpointState { sha, show: false });
        }
    }

    /// Flip the "restore files" affordance on the user entry at `index` once
    /// the turn is known to have changed repo state.
    pub fn set_checkpoint_show(&mut self, index: usize, show: bool) {
        if let Some(ThreadEntry::User { checkpoint: Some(cp), .. }) = self.entries.get_mut(index) {
            cp.show = show;
        }
    }

    /// Rewind: drop the user entry at `ordinal` (0-based among user entries)
    /// and everything after it, returning the removed user message's payload
    /// (for edit-and-resend prefill). Leaves the thread at rest — no turn in
    /// flight, streaming windows closed, stale footer cleared. Returns `None`
    /// (and mutates nothing) when the ordinal doesn't resolve.
    pub fn truncate_to_user(&mut self, ordinal: usize) -> Option<(String, Vec<ChatImage>)> {
        let idx = self.user_entry_index(ordinal)?;
        let removed = self.entries.drain(idx..).next();
        self.end_assistant_window();
        self.turn_active = false;
        self.usage = None;
        self.last_summary = None;
        match removed {
            Some(ThreadEntry::User { text, images, .. }) => Some((text, images)),
            _ => unreachable!("user_entry_index always indexes a User entry"),
        }
    }

    /// Transition a tool call's status directly (e.g. to `InProgress` when the
    /// user allows, or `Rejected` when they deny) — called by the connection
    /// layer alongside sending the control response.
    pub fn set_tool_status(&mut self, tool_id: &str, status: ToolCallStatus) {
        if let Some(tc) = self.tool_call_mut(tool_id) {
            tc.status = status;
        }
    }

    /// The pending permission request (if any) awaiting a decision.
    pub fn pending_permission(&self) -> Option<(&str, &PermissionRequest)> {
        self.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) => match &tc.status {
                ToolCallStatus::WaitingForConfirmation(req) => Some((tc.id.as_str(), req)),
                _ => None,
            },
            _ => None,
        })
    }

    /// The pending AskUserQuestion (if any) awaiting the user's answers.
    pub fn pending_question(&self) -> Option<(&str, &QuestionRequest)> {
        self.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) => match &tc.status {
                ToolCallStatus::AwaitingAnswer(req) => Some((tc.id.as_str(), req)),
                _ => None,
            },
            _ => None,
        })
    }

    fn tool_call_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.entries.iter_mut().rev().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(tc),
            _ => None,
        })
    }

    /// Close the current assistant message: subsequent text/thinking starts a
    /// fresh entry, and the streamed-vs-finalized reconciliation flags reset.
    fn end_assistant_window(&mut self) {
        self.current_assistant = None;
        self.text_streaming = false;
        self.thinking_streaming = false;
    }

    /// The assistant message currently being built, creating one if the last
    /// entry isn't an in-progress assistant message.
    fn assistant_mut(&mut self) -> &mut AssistantMessage {
        if self.current_assistant.is_none() {
            self.entries.push(ThreadEntry::Assistant(AssistantMessage::default()));
            self.current_assistant = Some(self.entries.len() - 1);
        }
        let idx = self.current_assistant.expect("just set");
        match &mut self.entries[idx] {
            ThreadEntry::Assistant(m) => m,
            _ => unreachable!("current_assistant always indexes an Assistant entry"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::background_task::BackgroundTaskKind;
    use serde_json::json;

    fn assistant_text(e: &ThreadEntry) -> &str {
        match e {
            ThreadEntry::Assistant(m) => &m.text,
            _ => panic!("not an assistant entry"),
        }
    }

    #[test]
    fn builds_a_simple_turn() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        t.apply(&ThreadEvent::AssistantTextDelta("Hel".into()));
        t.apply(&ThreadEvent::AssistantTextDelta("lo".into()));
        t.apply(&ThreadEvent::AssistantText("Hello!".into())); // finalize reconciles
        t.apply(&ThreadEvent::TurnEnded { result: Some("Hello!".into()), usage: None, is_error: false });

        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[0], ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None });
        assert_eq!(assistant_text(&t.entries[1]), "Hello!"); // not "Hellolo Hello!"
        assert!(!t.turn_active);
    }

    #[test]
    fn multiple_finalized_text_blocks_append_not_overwrite() {
        // A single assistant message carrying two top-level text blocks (no
        // tool_use between) must keep both — not silently drop the first.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantText("First.".into()));
        t.apply(&ThreadEvent::AssistantText("Second.".into()));
        let joined = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.clone()), _ => None }).collect::<Vec<_>>();
        assert_eq!(joined, vec!["First.\nSecond.".to_string()]);
    }

    #[test]
    fn streamed_then_multi_finalized_reconciles_then_appends() {
        // Deltas stream the first block; the first finalized replaces the
        // stream; a second finalized block appends.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantTextDelta("Fir".into()));
        t.apply(&ThreadEvent::AssistantTextDelta("st.".into()));
        t.apply(&ThreadEvent::AssistantText("First.".into())); // reconcile
        t.apply(&ThreadEvent::AssistantText("Second.".into())); // append
        assert_eq!(assistant_text(&t.entries[1]), "First.\nSecond.");
    }

    #[test]
    fn tool_call_flows_to_completed() {
        let mut t = ChatThread::new();
        t.push_user_message("read it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Read".into(), input: json!({"file_path":"a"}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_1".into(), content: "file body".into(), is_error: false });

        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.name, "Read");
                assert_eq!(tc.status, ToolCallStatus::Completed);
                assert_eq!(tc.result.as_deref(), Some("file body"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_for_unknown_id_is_ignored() {
        // A result whose id matches no tool call must not panic or corrupt the
        // existing tool call — it is simply dropped.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Read".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_UNKNOWN".into(), content: "stray".into(), is_error: false });
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::InProgress, "existing tool call untouched");
                assert!(tc.result.is_none());
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn permission_marks_matching_tool_call_and_is_findable() {
        let mut t = ChatThread::new();
        t.push_user_message("edit");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_9".into(), name: "Edit".into(), input: json!({}) });
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "rid-1".into(), tool_use_id: Some("toolu_9".into()),
            tool_name: "Edit".into(), input: json!({}), description: "notes.txt".into(),
            suggestions: vec![] });

        let (tool_id, req) = t.pending_permission().expect("a pending permission");
        assert_eq!(tool_id, "toolu_9");
        assert_eq!(req.request_id, "rid-1");
        assert_eq!(req.description, "notes.txt");
        // only one tool-call entry was created (permission matched, not duplicated)
        assert_eq!(t.entries.iter().filter(|e| matches!(e, ThreadEntry::ToolCall(_))).count(), 1);

        // deny → rejected, no longer pending
        t.set_tool_status("toolu_9", ToolCallStatus::Rejected);
        assert!(t.pending_permission().is_none());
    }

    #[test]
    fn question_asked_marks_tool_call_and_settles_on_result() {
        use crate::thread::question::parse_questions;
        let mut t = ChatThread::new();
        t.push_user_message("choose");
        let input = json!({"questions":[{"question":"Tabs or spaces?","header":"Indent",
            "options":[{"label":"Tabs","description":""},{"label":"Spaces","description":""}],
            "multiSelect":false}]});
        // The tool_use block lands first, then the AskUserQuestion control_request.
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_q".into(), name: "AskUserQuestion".into(), input: input.clone() });
        t.apply(&ThreadEvent::QuestionAsked {
            request_id: "rid-q".into(), tool_use_id: Some("toolu_q".into()),
            questions: parse_questions(&input) });

        let (tool_id, req) = t.pending_question().expect("a pending question");
        assert_eq!(tool_id, "toolu_q");
        assert_eq!(req.request_id, "rid-q");
        assert_eq!(req.questions.len(), 1);
        assert!(t.pending_permission().is_none(), "a question is not a permission");
        // folded onto the existing tool_use — not duplicated
        assert_eq!(t.entries.iter().filter(|e| matches!(e, ThreadEntry::ToolCall(_))).count(), 1);

        // the real tool_result (after we answer) settles it to Completed
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_q".into(),
            content: "Your questions have been answered: \"Tabs or spaces?\"=\"Tabs\".".into(),
            is_error: false });
        assert!(t.pending_question().is_none());
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Completed),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn rehydrate_and_interrupt_fail_close_a_pending_question() {
        use crate::thread::question::parse_questions;
        let input = json!({"questions":[{"question":"Q?","header":"H",
            "options":[{"label":"A","description":""}],"multiSelect":false}]});
        let mut tc = ToolCall::new("toolu_q", "AskUserQuestion", input.clone());
        tc.status = ToolCallStatus::AwaitingAnswer(QuestionRequest {
            request_id: "rid".into(), questions: parse_questions(&input) });

        // rehydrate fail-closes (the process that asked is gone)
        let t = ChatThread::rehydrated(None, None, vec![ThreadEntry::ToolCall(tc.clone())], vec![]);
        assert!(t.pending_question().is_none(), "no pending question survives restore");
        match &t.entries[0] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected Rejected, got {other:?}"),
        }

        // interrupt (Stop) also fail-closes
        let mut t2 = ChatThread::new();
        t2.entries.push(ThreadEntry::ToolCall(tc));
        t2.turn_active = true;
        t2.interrupt();
        assert!(t2.pending_question().is_none(), "interrupt fail-closes the question");
    }

    #[test]
    fn empty_question_set_is_ignored_not_stranded() {
        // A malformed/garbled AskUserQuestion with no questions must NOT become
        // an AwaitingAnswer (an empty-indexed card render would panic) — the
        // event is dropped and the tool_use is left untouched.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(),
            name: "AskUserQuestion".into(),
            input: json!({"questions": []}),
        });
        t.apply(&ThreadEvent::QuestionAsked {
            request_id: "rq".into(),
            tool_use_id: Some("t1".into()),
            questions: vec![],
        });
        assert!(t.pending_question().is_none(), "empty questions never becomes pending");
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::InProgress, "tool_use left untouched");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn orphan_permission_synthesizes_a_tool_call() {
        // Permission arrives with no matching tool_use (null id, or the
        // tool_use event was lost) — the card is synthesized so the user can
        // still allow/deny, keyed on the request_id.
        let mut t = ChatThread::new();
        t.push_user_message("do it");
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "rid-orphan".into(), tool_use_id: None,
            tool_name: "Bash".into(), input: json!({"command":"ls"}),
            description: "ls".into(), suggestions: vec![] });

        let (tool_id, req) = t.pending_permission().expect("synthesized pending permission");
        assert_eq!(tool_id, "rid-orphan", "falls back to request_id as the tool id");
        assert_eq!(req.request_id, "rid-orphan");
        match t.entries.iter().find(|e| matches!(e, ThreadEntry::ToolCall(_))) {
            Some(ThreadEntry::ToolCall(tc)) => assert_eq!(tc.name, "Bash"),
            _ => panic!("a synthesized ToolCall entry should exist"),
        }
    }

    #[test]
    fn rehydrated_seeds_entries_and_session_and_rests_flags() {
        let entries = vec![
            ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None },
            ThreadEntry::Assistant(AssistantMessage { text: "hello".into(), thinking: String::new() }),
        ];
        let t = ChatThread::rehydrated(Some("sid-9".into()), Some("opus".into()), entries, vec![]);
        assert_eq!(t.session_id.as_deref(), Some("sid-9"));
        assert_eq!(t.model.as_deref(), Some("opus"));
        assert_eq!(t.entries.len(), 2);
        assert!(!t.turn_active, "a restored thread has no turn in flight");
        assert!(t.pending_permission().is_none());
    }

    #[test]
    fn rehydrated_fail_closes_a_pending_permission() {
        // A tool call left WaitingForConfirmation at quit must restore as
        // Rejected — the process that asked is gone, so it can never be
        // answered; leaving it pending would strand an unanswerable prompt.
        let req = PermissionRequest {
            request_id: "rid".into(),
            description: "notes.txt".into(),
            suggestions: vec![],
        };
        let mut tc = ToolCall::new("toolu_1", "Edit", serde_json::json!({}));
        tc.status = ToolCallStatus::WaitingForConfirmation(req);
        let t = ChatThread::rehydrated(None, None, vec![ThreadEntry::ToolCall(tc)], vec![]);
        assert!(t.pending_permission().is_none(), "no pending permission survives restore");
        match &t.entries[0] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn turn_ended_stores_usage_for_the_footer() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        assert!(t.usage.is_none(), "no usage before a turn ends");
        t.apply(&ThreadEvent::TurnEnded {
            result: Some("done".into()),
            usage: Some(TurnUsage {
                input_tokens: 714, output_tokens: 7,
                cache_read_tokens: 16681, cache_creation_tokens: 5571,
                context_window: Some(200000), cost_usd: Some(0.33),
            }),
            is_error: false,
        });
        let u = t.usage.as_ref().expect("usage stored");
        assert_eq!(u.input_tokens, 714);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cost_usd, Some(0.33));
    }

    #[test]
    fn new_turn_clears_prior_usage_and_summary() {
        // The footer must not carry a prior turn's numbers into the next turn.
        let mut t = ChatThread::new();
        t.push_user_message("q1");
        t.apply(&ThreadEvent::TurnSummary { detail: "did a thing".into(), category: "x".into() });
        t.apply(&ThreadEvent::TurnEnded {
            result: Some("a".into()),
            usage: Some(TurnUsage { input_tokens: 100, ..Default::default() }),
            is_error: false,
        });
        assert!(t.usage.is_some() && t.last_summary.is_some());

        // Sending the next prompt wipes the stale footer/summary.
        t.push_user_message("q2");
        assert!(t.usage.is_none(), "prior usage cleared on a new turn");
        assert!(t.last_summary.is_none(), "prior summary cleared on a new turn");
    }

    #[test]
    fn interrupt_ends_turn_and_fail_closes_pending_permission() {
        // Stop mid-turn: the turn flag clears and a pending approval downgrades
        // to Rejected (the process is being killed and can never answer it).
        let mut t = ChatThread::new();
        t.push_user_message("edit it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(), name: "Edit".into(), input: json!({}) });
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "r1".into(), tool_use_id: Some("t1".into()),
            tool_name: "Edit".into(), input: json!({}), description: "x".into(),
            suggestions: vec![] });
        assert!(t.turn_active);
        assert!(t.pending_permission().is_some());

        t.interrupt();

        assert!(!t.turn_active, "interrupt ends the turn");
        assert!(t.pending_permission().is_none(), "pending approval is fail-closed");
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn interrupt_does_not_duplicate_the_trailing_finalized_block() {
        // After Stop, the agent flushes trailing deltas + a finalized `assistant`
        // block before EOF. That finalized text must reconcile INTO the streamed
        // block, not spawn a second identical one. (Interrupt must not close the
        // assistant window, or the reply doubles.)
        let mut t = ChatThread::new();
        t.push_user_message("essay");
        t.apply(&ThreadEvent::AssistantTextDelta("The History".into()));
        t.interrupt(); // user hits Stop mid-stream
        // trailing events that arrive after SIGINT, before stdout EOF:
        t.apply(&ThreadEvent::AssistantTextDelta(" and Design".into()));
        t.apply(&ThreadEvent::AssistantText("The History and Design of Rust".into()));
        t.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: true });

        let assistants: Vec<&str> = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.as_str()), _ => None }).collect();
        assert_eq!(
            assistants,
            vec!["The History and Design of Rust"],
            "exactly one assistant block, reconciled — not doubled"
        );
    }

    #[test]
    fn transcript_survives_a_serde_round_trip() {
        // The persisted-blob path serializes the entry list and reloads it.
        // Prove a representative transcript (user + assistant + completed tool)
        // round-trips byte-for-byte through serde_json.
        let mut src = ChatThread::new();
        src.push_user_message("read it");
        src.apply(&ThreadEvent::AssistantText("On it.".into()));
        src.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(), name: "Read".into(), input: json!({"file_path":"a"}) });
        src.apply(&ThreadEvent::ToolResult {
            tool_use_id: "t1".into(), content: "body".into(), is_error: false });

        let json = serde_json::to_string(&src.entries).expect("serialize entries");
        let back: Vec<ThreadEntry> = serde_json::from_str(&json).expect("deserialize entries");
        assert_eq!(back, src.entries);
    }

    #[test]
    fn checkpoint_attach_show_and_truncate() {
        let mut t = ChatThread::new();
        t.push_user_message("one");
        t.apply(&ThreadEvent::AssistantText("A1".into()));
        t.push_user_message("two");
        t.apply(&ThreadEvent::AssistantText("A2".into()));
        t.push_user_message("three");
        t.apply(&ThreadEvent::AssistantText("A3".into()));

        // Attach a checkpoint to user #1 ("two") and flip its show flag.
        let idx = t.user_entry_index(1).expect("second user entry");
        t.attach_checkpoint(idx, "sha-2".into());
        t.set_checkpoint_show(idx, true);
        match &t.entries[idx] {
            ThreadEntry::User { checkpoint: Some(cp), .. } => {
                assert_eq!(cp.sha, "sha-2");
                assert!(cp.show);
            }
            other => panic!("expected checkpointed user entry, got {other:?}"),
        }

        // Rewind to "two": it and everything after are dropped; payload returned.
        let (text, images) = t.truncate_to_user(1).expect("truncate succeeds");
        assert_eq!(text, "two");
        assert!(images.is_empty());
        assert_eq!(t.entries.len(), 2, "only 'one' + 'A1' remain");
        assert!(!t.turn_active);
        assert!(t.usage.is_none() && t.last_summary.is_none());

        // Out-of-range ordinal mutates nothing.
        assert!(t.truncate_to_user(5).is_none());
        assert_eq!(t.entries.len(), 2);
    }

    #[test]
    fn attach_checkpoint_skips_non_user_index() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        t.apply(&ThreadEvent::AssistantText("yo".into()));
        // Index 1 is the assistant entry — attach must be a no-op, not a panic.
        t.attach_checkpoint(1, "sha-x".into());
        assert!(matches!(&t.entries[1], ThreadEntry::Assistant(_)));
    }

    #[test]
    fn checkpointed_entry_survives_serde_and_old_blobs_default() {
        let mut t = ChatThread::new();
        t.push_user_message("snap");
        t.attach_checkpoint(0, "abc123".into());
        let json = serde_json::to_string(&t.entries).unwrap();
        let back: Vec<ThreadEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t.entries);

        // A pre-checkpoint blob (no field at all) still loads.
        let old = r#"[{"User":{"text":"legacy","images":[]}}]"#;
        let entries: Vec<ThreadEntry> = serde_json::from_str(old).unwrap();
        match &entries[0] {
            ThreadEntry::User { checkpoint, .. } => assert!(checkpoint.is_none()),
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn text_then_tool_then_text_makes_two_assistant_messages() {
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantText("Let me check.".into()));
        t.apply(&ThreadEvent::ToolCallStarted { id: "t1".into(), name: "Bash".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult { tool_use_id: "t1".into(), content: "ok".into(), is_error: false });
        t.apply(&ThreadEvent::AssistantText("Done.".into()));

        let assistants: Vec<&str> = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.as_str()), _ => None }).collect();
        assert_eq!(assistants, vec!["Let me check.", "Done."]);
        // order: User, Assistant, ToolCall, Assistant
        assert!(matches!(t.entries[0], ThreadEntry::User { .. }));
        assert!(matches!(t.entries[1], ThreadEntry::Assistant(_)));
        assert!(matches!(t.entries[2], ThreadEntry::ToolCall(_)));
        assert!(matches!(t.entries[3], ThreadEntry::Assistant(_)));
    }

    #[test]
    fn background_task_lifecycle_folds_into_state() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::BackgroundTaskStarted {
            task_id: "T1".into(),
            tool_use_id: "toolu_a".into(),
            kind: BackgroundTaskKind::Subagent { subagent_type: "general-purpose".into() },
            description: "read files".into(),
        });
        assert_eq!(t.running_task_count(), 1);
        t.apply(&ThreadEvent::BackgroundTaskProgress {
            task_id: "T1".into(),
            last_tool: Some("Read".into()),
        });
        t.apply(&ThreadEvent::BackgroundTaskProgress {
            task_id: "T1".into(),
            last_tool: Some("Read".into()),
        });
        let task = &t.background_tasks[0];
        assert_eq!(task.tool_uses, 2);
        assert_eq!(task.last_tool.as_deref(), Some("Read"));
        assert!(task.is_running());

        // task_updated (end_time) then task_notification (summary+output_file) —
        // both terminal signals merge, neither clobbers the other's fields.
        t.apply(&ThreadEvent::BackgroundTaskFinished {
            task_id: "T1".into(),
            failed: false,
            ended_at_ms: Some(1000),
            summary: None,
            output_file: None,
        });
        t.apply(&ThreadEvent::BackgroundTaskFinished {
            task_id: "T1".into(),
            failed: false,
            ended_at_ms: None,
            summary: Some("done".into()),
            output_file: Some("/tmp/x.jsonl".into()),
        });
        let task = &t.background_tasks[0];
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.ended_at_ms, Some(1000), "end_time not clobbered by the later None");
        assert_eq!(task.summary.as_deref(), Some("done"));
        assert!(task.has_transcript());
        assert_eq!(t.running_task_count(), 0);
    }

    #[test]
    fn background_task_finished_failed_marks_failed() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::BackgroundTaskStarted {
            task_id: "T1".into(),
            tool_use_id: "toolu_a".into(),
            kind: BackgroundTaskKind::BackgroundBash,
            description: "boom".into(),
        });
        t.apply(&ThreadEvent::BackgroundTaskFinished {
            task_id: "T1".into(),
            failed: true,
            ended_at_ms: None,
            summary: None,
            output_file: None,
        });
        assert_eq!(t.background_tasks[0].status, TaskStatus::Failed);
    }

    /// End-to-end: folding the captured spike stream tracks exactly the two tasks,
    /// both completed, without polluting the transcript with subagent chatter.
    #[test]
    fn spike_fixture_folds_to_two_completed_tasks() {
        let fixture = include_str!("testdata/stream_json_subagent.jsonl");
        let mut t = ChatThread::new();
        for line in fixture.lines() {
            for ev in super::super::stream_json::decode_line(line) {
                t.apply(&ev);
            }
        }
        assert_eq!(t.background_tasks.len(), 2, "one subagent + one bash");
        assert!(t.background_tasks.iter().all(|task| task.status == TaskStatus::Completed));
        assert_eq!(t.running_task_count(), 0);
        // Both carry an output_file for "View transcript".
        assert!(t.background_tasks.iter().all(|task| task.has_transcript()));
        // The subagent ran tools; its activity counter advanced.
        let subagent = t
            .background_tasks
            .iter()
            .find(|task| matches!(task.kind, BackgroundTaskKind::Subagent { .. }))
            .expect("a subagent task");
        assert!(subagent.tool_uses > 0, "subagent progress steps counted");
    }
}
