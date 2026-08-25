//! The status extension OxiMux writes into Pi's extensions directory.
//!
//! Pi has no hooks file. Its extension point is an in-process TypeScript API —
//! `pi.on("agent_end", …)` — so the only way to learn what a Pi agent said is
//! to hand Pi a program that tells us. That program is this one: OxiMux renders
//! it at install time with the absolute path of its own binary baked in, writes
//! it into the directory Pi discovers extensions from, and Pi loads it on every
//! subsequent start.
//!
//! Everything downstream is unchanged. The extension shells out to the same
//! `oximux agent-status` CLI every hooks-file agent runs and hands it JSON on
//! stdin, so from the relay onward a Pi row is indistinguishable from a Claude
//! one. It composes that JSON in Claude's key names for the same reason — one
//! reader, not a Pi-shaped special case.
//!
//! The event sequence is measured, not assumed. A run of
//! `pi -p 'reply with only the word DONE'` produced, in order:
//! `session_start`, `message_start`, `agent_start`, `message_end` (role
//! `user`), `message_start`, `message_end` (role `assistant`), `agent_end`,
//! `session_shutdown` — with the reply itself under
//! `message.content[].text`, an array of parts exactly like Claude's.
//!
//! Two consequences of that ordering shape the code below:
//!
//! * The reply arrives BEFORE the turn ends, on its own event, so it is held
//!   and attached to the `agent_end` report. Reporting it as it arrives would
//!   put a row into idle while the agent is still working.
//! * Pi dispatches BOTH `before_agent_start` and `agent_start` for one turn,
//!   and both `agent_end` and `session_shutdown` to end it. Subscribing to
//!   only one of each pair would be a bet on a version; subscribing to both
//!   and dropping the repeat is not.

use std::path::Path;

/// Where the extension goes, relative to Pi's configuration directory.
///
/// Everything in `extensions/` is discovered and loaded on start, so writing
/// the file IS the install. The name is distinctive enough that
/// [`crate::agent_hook_dialects::Install::Extension`] can delete it outright
/// on uninstall without inspecting the contents: no other tool writes it.
pub(crate) const EXTENSION_FILE: &str = "extensions/oximux-agent-status.ts";

/// Where the omp extension goes, relative to omp's configuration directory.
///
/// A DIFFERENT basename than Pi's on purpose: omp kept Pi's
/// `PI_CODING_AGENT_DIR` env override, so with that variable set both agents
/// resolve the SAME directory — two dialects writing one path would leave
/// install order deciding which survives, and prune would delete the winner.
/// Distinct names keep each dialect's install/uninstall its own even when the
/// homes collide (each runtime then loads both files; the format slug keeps
/// the reports apart).
pub(crate) const OMP_EXTENSION_FILE: &str = "extensions/oximux-agent-status-omp.ts";

/// Render the Pi extension, calling back into the `oximux` binary at
/// `binary_path`.
///
/// The path is embedded in a single-quoted shell argument, so an embedded
/// quote is escaped the same way the hooks-file commands escape theirs — a
/// home directory like `/Users/O'X` must not break out into shell injection.
pub(crate) fn source(binary_path: &Path) -> String {
    source_for(binary_path, "Pi", "pi")
}

/// Render the omp extension. Same program, same measured event dialect
/// (re-verified live against omp 18.0.4), different `--format` slug so the
/// reader attributes the report to the right agent.
pub(crate) fn omp_source(binary_path: &Path) -> String {
    source_for(binary_path, "omp", "omp")
}

/// The shared renderer: one template, parameterized over the runtime's display
/// name (comments) and status slug (the `--format` argument). Pi and omp share
/// an extension API and event taxonomy by lineage — omp is a Pi fork — so the
/// program itself is identical; only the identity strings differ.
fn source_for(binary_path: &Path, name: &str, slug: &str) -> String {
    let quoted = binary_path.display().to_string().replace('\'', "'\\''");
    format!(
        r#"// Managed by OxiMux. Written on start; edits are overwritten.
//
// Reports this {name} agent's lifecycle to the OxiMux pane it is running in, so
// the pane's row can show what the agent is doing and what it last said.
// Outside an OxiMux pane this file does nothing at all.

import {{ spawn }} from "node:child_process";

const OXIMUX_BINARY = '{quoted}';

export default function (pi: any) {{
  // The pane id the relay injects into every PTY it spawns. Without it there
  // is nothing to report to, and this extension is a complete no-op — which is
  // the case for every `{slug}` the user runs outside OxiMux.
  if (!process.env.OXIMUX_PTY_ID) return;

  // The agent's most recent reply, held from the message that carried it until
  // the turn actually ends. {name} emits the two separately and in that order.
  let lastMessage: string | undefined;
  // The last thing reported, so a repeat can be dropped. {name} fires two events
  // for each end of a turn, and reporting both would spend a process and a
  // relay round-trip restating what the row already says. A report that
  // carries something NEW — a prompt, then a tool — differs here and goes
  // through.
  let lastReport: string | undefined;

  const report = (state: string, payload: Record<string, unknown> = {{}}) => {{
    const signature = state + JSON.stringify(payload);
    if (signature === lastReport) return;
    lastReport = signature;
    try {{
      const child = spawn(OXIMUX_BINARY, ["agent-status", "--state", state, "--format", "{slug}"], {{
        stdio: ["pipe", "ignore", "ignore"],
        detached: true,
      }});
      // Never let a reporting failure reach the agent: a missing binary, a
      // relay that is not listening, or a pipe closed early are all silent.
      child.on("error", () => {{}});
      child.stdin?.on("error", () => {{}});
      child.stdin?.end(JSON.stringify(payload));
      child.unref();
    }} catch {{
      // spawn() can throw synchronously (EACCES, ENOENT). Stay silent.
    }}
  }};

  // The text parts of a message, joined — the same shape Claude uses, so the
  // reader on the other side is the same one.
  const textOf = (message: any): string | undefined => {{
    const parts = message?.content;
    if (!Array.isArray(parts)) return undefined;
    const text = parts
      .filter((p: any) => p?.type === "text" && typeof p.text === "string")
      .map((p: any) => p.text)
      .join(" ")
      .trim();
    return text.length > 0 ? text : undefined;
  }};

  const on = (event: string, handler: (e: any) => void) => {{
    // A {name} version that does not know an event must not take the rest down
    // with it, so each subscription stands alone.
    try {{
      pi.on(event, handler);
    }} catch {{}}
  }};

  // Turn start. Both spellings are subscribed because which one a given {name}
  // dispatches has moved between versions.
  for (const event of ["before_agent_start", "agent_start"]) {{
    on(event, () => {{
      lastMessage = undefined;
      report("working");
    }});
  }}

  on("tool_execution_start", (event: any) => {{
    report("working", {{ tool_name: event?.tool_name ?? event?.toolName }});
  }});

  on("message_end", (event: any) => {{
    const message = event?.message;
    const text = textOf(message);
    if (!text) return;
    // The user's own message is the row's title; the agent's is what it said.
    if (message?.role === "user") {{
      report("working", {{ prompt: text }});
    }} else if (message?.role === "assistant") {{
      lastMessage = text;
    }}
  }});

  // Turn end, and the point the held reply is finally reported.
  // `session_shutdown` covers Ctrl+C, /quit and a reload, any of which would
  // otherwise leave the row stuck on working forever.
  for (const event of ["agent_end", "session_shutdown"]) {{
    on(event, () => {{
      report("idle", lastMessage ? {{ last_assistant_message: lastMessage }} : {{}});
    }});
  }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered() -> String {
        source(Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"))
    }

    #[test]
    fn the_extension_calls_back_into_our_binary_with_the_pi_reader() {
        let src = rendered();
        assert!(src.contains("/Applications/OxiMux.app/Contents/MacOS/oximux"));
        assert!(src.contains(r#""agent-status", "--state", state, "--format", "pi""#));
    }

    #[test]
    fn the_omp_variant_is_the_same_program_under_its_own_identity() {
        // One template, two identities: the omp render must select the omp
        // reader and must not carry Pi's name anywhere — a leaked "Pi" would
        // mean an identity substitution was missed and the next divergence
        // between the two would edit one render thinking it edited both.
        let src = omp_source(Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"));
        assert!(src.contains(r#""agent-status", "--state", state, "--format", "omp""#));
        assert!(!src.contains("Pi"), "Pi identity leaked into the omp render");
        // The inertness guard is identity-independent and must survive.
        assert!(src.contains("if (!process.env.OXIMUX_PTY_ID) return;"));
        // And the Pi render stays free of omp's identity in return. (A bare
        // "omp" substring probe would trip on the word "complete", so the
        // reader selection — the one identity a wrong render would act on —
        // is what is asserted.)
        assert!(
            !rendered().contains(r#""--format", "omp""#),
            "omp identity leaked into the Pi render"
        );
    }

    #[test]
    fn a_path_with_a_quote_cannot_break_out_of_the_source() {
        // The path is embedded in a single-quoted JS string literal, so an
        // unescaped quote would end the literal and everything after it would
        // be parsed as code.
        let src = source(Path::new("/Users/O'X/oximux"));
        assert!(src.contains(r"/Users/O'\''X/oximux"), "unescaped quote in {src:?}");
    }

    #[test]
    fn every_state_the_cli_accepts_is_spelled_exactly() {
        // A typo here installs cleanly and reports nothing: the CLI rejects an
        // unknown state, and its stderr goes to a pipe nobody reads.
        let src = rendered();
        assert!(src.contains(r#"report("working""#));
        assert!(src.contains(r#"report("idle""#));
    }

    #[test]
    fn a_repeated_report_is_dropped() {
        // Pi fires two events at each end of a turn. Both are subscribed so
        // that no Pi version loses one, which makes suppressing the repeat the
        // extension's job rather than the relay's.
        let src = rendered();
        assert!(src.contains("if (signature === lastReport) return;"));
        // …but a report that carries something new must still go through, or
        // the tool that follows a prompt would never be reported.
        assert!(src.contains("const signature = state + JSON.stringify(payload);"));
    }

    #[test]
    fn the_reply_is_held_until_the_turn_ends() {
        // Pi emits the assistant's message BEFORE the turn-end event. Reporting
        // it on arrival would put the row into idle mid-turn; assigning it and
        // reporting later is what keeps the two honest.
        let src = rendered();
        let assign = src.find("lastMessage = text").expect("the reply is held");
        let report = src.find("last_assistant_message: lastMessage").expect("and later reported");
        assert!(assign < report, "the reply must be held before it is reported");
    }

    #[test]
    fn the_extension_is_inert_outside_an_oximux_pane() {
        // A user's own `pi` must not spawn anything. The guard is the first
        // statement in the extension body, before any subscription.
        let src = rendered();
        let guard = src.find("if (!process.env.OXIMUX_PTY_ID) return;").expect("the guard");
        let first_subscription = src.find("pi.on(").expect("a subscription");
        assert!(guard < first_subscription, "the guard must precede every subscription");
    }

    #[test]
    fn a_headless_run_still_reports() {
        // Pi flags print mode and RPC mode by setting `hasUI` false, and it is
        // tempting to skip on that — the shape another cockpit uses to keep a
        // subagent off its parent's row. Pi has no subagent tool (`read`,
        // `bash`, `edit`, `write`), so that gate would suppress every
        // `pi -p` in a pane to prevent nothing.
        assert!(
            !rendered().contains("hasUI"),
            "a headless Pi in a pane is the user's work and must still report"
        );
    }
}

