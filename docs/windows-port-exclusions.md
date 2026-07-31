# Windows port — what is left behind, and what that costs

OxiMux ships on macOS. The Windows port keeps the whole product except the
features listed here, each of which is macOS-shaped down to the OS API. This
file is the record of *why* each one is out and *what is lost by leaving it
out* — the second half matters more, because a feature that only removes
capability is cheap and a feature that also removes a **guarantee** is not.

Companion to `plans/260731-0325-windows-cross-platform-port/`. Update this file
when an exclusion is lifted, not the plan.

## Excluded from Windows v1

| Feature | Why | What Windows loses |
|---|---|---|
| Computer use (screen control) | Drives macOS Accessibility APIs through a macOS-only CUA driver; no Windows equivalent exists to port to | The whole feature, plus a safety guarantee — see below |
| Browser cookie import | Chromium app-bound encryption on Windows is a different scheme from the macOS Keychain path | Signed-in sessions do not carry into the embedded browser tab |
| Escape-tap kill switch | A global CGEventTap needing Accessibility + Input Monitoring grants | Nothing — it exists only to interrupt computer use |
| Secure-input probe | Reads macOS's IORegistry `IOConsoleUsers` | Nothing — same, it only reports why computer use went deaf |
| Microphone permission (TCC) flow | macOS `AVCaptureDevice` authorization UX | Nothing structural; Windows grants mic access through its own settings, so dictation asks differently |

## The one exclusion that removes a guarantee

Dropping computer use also drops the **Bash policing hook**. On macOS, every
Claude chat gets a `PreToolUse` hook installed that inspects Bash commands for
attempts to reach screen control by the side door — driving the machine through
`osascript`, or granting the driver's Accessibility permission from a shell.
That hook is wider than the computer-use tools it protects: it watches *every*
chat, not just chats that opted in.

On Windows there is no CUA driver to reach, so there is nothing for the side
door to open, and the hook has nothing to police. The exclusion is therefore
consistent rather than a hole. It is recorded here anyway because the reasoning
is conditional: **the moment any screen-driving capability lands on Windows, the
policing hook has to land with it, in the same change.** A Windows build that
gains screen control without the hook would be strictly less safe than the macOS
build, and nothing in the compiler would say so.

Revisit in v2.

## What still compiles everywhere

Two pieces that live near computer use are deliberately **not** excluded:

- **Transcript screenshot scrubbing** (`oximux-agent-core::redact`) — captures
  are produced only on macOS, but a transcript containing them can be *read*
  anywhere: agent CLIs keep their session stores under the user's home
  directory, and those get synced between machines. A Windows host serving a
  paired phone must still scrub. Moved out of `oximux-computer-use` for exactly
  this reason.
- **The screen-tool naming contract** (`oximux-agent-core::screen_tools`) — what
  the scrubber matches on.

## Enumerated `oximux_computer_use` call sites

All references live behind `#[cfg(target_os = "macos")]` or inside macOS-only
modules; the crate is absent from the Windows dependency graph
(`cargo tree --target x86_64-pc-windows-msvc` shows no edge to it). Verify with:

```
grep -rn "oximux_computer_use" apps/ crates/ --include="*.rs"
```

| File | Role |
|---|---|
| `apps/desktop/src/shell/agent_chat/computer_use.rs` | Turn-level driver glue |
| `apps/desktop/src/shell/agent_chat/screen_card.rs` | Tool-call rendering |
| `apps/desktop/src/shell/agent_chat/screen_consent.rs` | Consent prompts |
| `apps/desktop/src/shell/agent_chat/mod.rs` | Tool-name dispatch |
| `apps/desktop/src/shell/agent_chat/remote_turn.rs` | Remote-driven turns |
| `apps/desktop/src/shell/settings_modal/pane_computer_use.rs` | Settings pane |
| `apps/desktop/src/shell/settings_modal/mod.rs` | Pane registration |
| `apps/desktop/src/shell/driver_install.rs` | Driver installer |
| `apps/desktop/src/platform/screen_control_indicator.rs` | Menu-bar status item |
| `apps/desktop/src/agent_glue/screen_control_watch.rs` | Session watch |

The two crate-level consumers that once reached for it — `oximux-agents`
(`session_registry`) and `oximux-remote-host` (`dispatcher/handlers`) — now
depend on `oximux-agent-core::redact` instead and no longer name it at all.
