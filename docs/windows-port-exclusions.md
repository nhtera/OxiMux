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
| ~~Computer use (screen control)~~ | **No longer excluded.** Shipping on Windows: the driver is declared, the policing hook rides with it, and the trust anchor is the user's own approval | — (two narrower gaps remain; see below) |
| Browser cookie import | Chromium app-bound encryption on Windows is a different scheme from the macOS Keychain path | Signed-in sessions do not carry into the embedded browser tab |
| ~~Escape-tap kill switch~~ | **No longer excluded.** Ported to a `WH_KEYBOARD_LL` hook, which consumes the key rather than observing it | — |
| Secure-input probe | Reads macOS's IORegistry `IOConsoleUsers`; Windows exposes no equivalent to read | The ability to *confirm* the kill switch is live — see below |
| Microphone permission (TCC) flow | macOS `AVCaptureDevice` authorization UX | Nothing structural; Windows grants mic access through its own settings, so dictation asks differently |

### Computer use: what actually changed

The original reason — "no Windows equivalent exists to port to" — was out of
date. `cua-driver` lists Windows at its Supported tier (Win32, UI Automation,
native input), and has shipped Windows binaries in every release since v0.1.3.

Measuring it turned up a different blocker. **The published Windows binaries are
unsigned**: `cua-driver.exe`, `cua-driver-uia.exe`, and `cua_driver_sdk.dll` all
report `NotSigned`, there is no GitHub build provenance to fall back on, and
`install.ps1` verifies neither a checksum nor a signature. `crates/computer-use`'s
whole trust model is Apple code signing — an identifier pin, a team-ID pin, and a
stapled notarization ticket — and there is no Authenticode subject to pin in
their place.

Detail in `plans/260801-0157-windows-computer-use/findings.md`.

### And what replaced it: the user as the trust anchor

That policy question has been answered. `oximux_computer_use::trust` pins the
SHA-256 of the binary **the user approved** and refuses to hand the driver to an
agent if those bytes ever change. Trust-on-first-use, anchored on a person
rather than a certificate authority.

Be exact about the halves, because overselling this is how it becomes worse than
nothing:

- **No identity.** Nothing here distinguishes an authentic `cua-driver.exe` from
  a hostile one the user was talked into installing. Approve a bad binary and it
  pins the bad binary. Any UI must say *unverified publisher* and must never say
  *verified*.
- **Continuity, yes.** Once approved the bytes cannot change without the user
  being asked again — which is the realistic threat against a long-lived install
  of an unsigned, self-updating tool.

The pin is on the bytes, not the path, so moving the driver keeps it trusted and
rewriting it does not. Upstream ships roughly six releases a week and the driver
rewrites itself in place, so re-approval is the routine path rather than an edge
case. That cost is inherent: with no publisher signature, a new binary genuinely
is a new trust decision. The UI may make re-approval one click; it may not skip
it.

### The approval screen

`settings_modal::pane_driver_trust` is the Windows Computer use pane. It shows
the path, the size, and the **full 64-character digest** — ungrouped and
untruncated, so it can be compared against upstream's published `checksums.txt`;
an abbreviated hash cannot be checked against anything — plus the words
*unverified publisher*, and an Approve button.

There is no separate confirmation dialog. Everything the decision rests on is on
the pane before the click, because a dialog that appears afterwards would be the
second thing read rather than the first.

Two details that are policy rather than styling:

- An approved driver is **not** painted with the success colour and its summary
  does not use the word "verified". Green would claim a publisher check that did
  not happen.
- "Not approved yet" and "changed since you approved it" are separate states
  with separate wording. The first is the ordinary first-run state; the second is
  the only thing here that should alarm anyone.

### The feature is on

`windows-screen-control` is enabled for the desktop app, so the driver is
declared to agents, the consent cards and turn glue run, and the policing hook
rides alongside — which is what the rule below always demanded.

Two things Windows still does not get, both narrower than the feature and
recorded here rather than hidden:

**No in-app installer.** `install` downloads and stages a notarized `.app`; none
of that has a Windows meaning. The user installs the driver themselves, which is
also what makes the trust anchor coherent — they choose the route, OxiMux
enforces the choice.

**A weaker "an agent is driving" indicator** (verified present on a real desktop, 2026-08-01). macOS gets a menu-bar status item,
visible the moment it is created. Windows gets a notification-area icon carrying
the same wording in its tooltip — but Windows 10 and 11 hide newly-registered
tray icons in the overflow flyout by default, so it can be one click away until
the user pins it. That is a real reduction in a *safety* signal, not a cosmetic
difference, and it is the one remaining place where the Windows build is weaker
than the macOS build at the same job.

**The allowlist is keyed differently, and more weakly.** macOS keys a persisted
"always allow" on the code-signing bundle id, which survives updates and moves
and cannot be forged by renaming a file. Windows has no such identity, so the key
is the executable's full path: a *different* binary later written to an approved
path inherits the approval. Full path rather than file name, at least, so
approving one `chrome.exe` is not approving every file with that name.

### The secure-input row now costs something

It used to cost nothing, because there was no kill switch to report on. Now
there is. A `WH_KEYBOARD_LL` hook can be starved of the key it exists to catch —
by the secure desktop (UAC, Ctrl+Alt+Del, the lock screen) and by UIPI when the
foreground window outranks OxiMux — and Windows offers no way to ask whether
that is happening.

The macOS build can therefore say "Escape will stop an agent" and mean it. A
Windows build can only say it *should*. That is a smaller loss than the feature
itself, and it is a real one, so any UI making that promise has to be worded for
the platform it is running on.

### The policing hook is no longer excluded

Windows chats now get the `PreToolUse` hook, with no driver and no tools behind
it. The reasoning below said the exclusion was "consistent rather than a hole"
because there would be no CUA driver to reach — and that is still true of the
*driver*. It was never true of the screen.

Synthesizing input on Windows requires no permission: `SendInput`, window
messages, and UI Automation are available to any unelevated process in the
interactive session. So an agent's shell can drive the screen on a machine where
computer use was never installed and cannot be. The side door was already open;
what was missing was anything watching it.

**Packaging requirement — met.** The hook resolves `oximux-screen-gate.exe`
beside the app executable. `scripts/bundle-windows.ps1` copies it there and
fails the build if it is missing, because a packaged Windows build that omits
the step runs every chat unenforced, logging one warning and otherwise looking
normal. See `docs/windows-packaging.md`.

## ~~The one exclusion that removes a guarantee~~ (superseded)

Kept because the conditional in its last paragraph is the reason the section
above exists — it named the trigger correctly and then mis-read when the trigger
had fired.

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

The error was in the first sentence, not the last. "Screen-driving capability"
was read as something OxiMux would have to *land*, so the trigger looked like it
was in the future. On Windows it is ambient: the capability was on the machine
before OxiMux was installed, and the hook was owed from the first Windows build.
The conditional held; what it was conditional on had already happened.

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

The crate **is** in the Windows dependency graph now — one edge, for the hook:

```
cargo tree --target x86_64-pc-windows-msvc -p oximux-app -i oximux-computer-use
```

All but two of the call sites below now compile on Windows as well as macOS.
The exceptions are `driver_install.rs` (there is no Windows installer) and the
macOS half of `pane_computer_use.rs`'s driver row, which is replaced by
`pane_driver_trust.rs` — same slot in the same pane, a different question.

`screen_control_absent.rs` is now the *Linux* stand-in only. Verify the split
with:

```
grep -rn "oximux_computer_use" apps/ crates/ --include="*.rs"
```

The Windows CI job does not exclude the crate. Its Windows-specific halves — the
executable blocklist, the GUI-scripting brake, `CommandLineToArgvW` quoting, the
user-pinned trust anchor — are run by `cargo test (computer-use, hook half)`.

| File | Role |
|---|---|
| `apps/desktop/src/shell/agent_chat/computer_use.rs` | Turn-level driver glue |
| `apps/desktop/src/shell/agent_chat/screen_card.rs` | Tool-call rendering |
| `apps/desktop/src/shell/agent_chat/screen_consent.rs` | Consent prompts |
| `apps/desktop/src/shell/agent_chat/mod.rs` | Tool-name dispatch |
| `apps/desktop/src/shell/agent_chat/remote_turn.rs` | Remote-driven turns |
| `apps/desktop/src/shell/settings_modal/pane_computer_use.rs` | Settings pane |
| `apps/desktop/src/shell/settings_modal/mod.rs` | Pane registration |
| `apps/desktop/src/shell/driver_install.rs` | Driver installer — **macOS only** |
| `apps/desktop/src/platform/screen_control_indicator.rs` | Menu-bar item / tray icon |
| `apps/desktop/src/agent_glue/screen_control_watch.rs` | Session watch |

The two crate-level consumers that once reached for it — `oximux-agents`
(`session_registry`) and `oximux-remote-host` (`dispatcher/handlers`) — now
depend on `oximux-agent-core::redact` instead and no longer name it at all.
