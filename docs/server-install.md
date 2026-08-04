# Running `oximux serve` on a server

`oximux serve` turns the same binary the CLI ships into a headless OxiMux
host: agent sessions, transcripts, terminals (via the relay daemon), the local
CLI socket, and the paired-device endpoint — everything the desktop app hosts,
minus every window. A phone or laptop pairs with it exactly as it pairs with
the desktop.

## Contract

- **stdout carries exactly one line**, the readiness JSON, and nothing else —
  ever. A journal that captures stdout can never capture a secret:

  ```json
  {"type":"oximux_serve_ready","schemaVersion":1,"protocolVersion":16,"localSocket":"…","endpointId":"<64 hex>"}
  ```

- **Logs go to stderr** (`RUST_LOG` filters them; default `info`).
- **Exit codes**: 0 after a clean drain, 1 on a boot failure.
- **Shutdown drains**: on SIGTERM/SIGINT (Windows: Ctrl+C, console close, OS
  shutdown) serve stops accepting work, waits up to 20 s for in-flight agent
  turns, marks any stragglers *interrupted* in the transcript (never silently
  truncated), flushes, and exits. Terminals survive regardless — the relay
  daemon is a detached process that outlives serve on purpose.

## Data directory

By default serve uses the same data directory as the desktop app
(`dev.nhtera.oximux` under the platform's local-data root), so sessions,
transcripts, projects, and **pairings** are one set: a phone paired with the
desktop reaches serve without re-pairing, and vice versa. Only one host can
hold the local socket at a time — if the desktop app is running, serve refuses
to bind and says so.

`--data-dir <DIR>` isolates a serve instance deliberately (its own database,
identity, and pairings). The directory is restricted to the owning account at
every boot, database sidecars included.

## Requirements on the box

- The agent CLIs you intend to run (`claude`, `codex`, `pi`) must be on the
  **service's** `PATH` — a systemd unit's default PATH is minimal, and a
  missing agent binary surfaces as "the session could not be started", not at
  boot. Set `Environment=PATH=…` explicitly (see the unit below).
- `oximux-relay` must sit next to the `oximux` binary (the installer does
  this) or be named via `OXIMUX_RELAY_BINARY`. Without it serve still runs;
  terminals are simply not served.
- Serve scrubs inherited Claude Code session markers itself, so starting it
  from inside an agent session cannot break transcript saving.

## Pairing

Pairing is a **runtime command, never a boot flag** — a flag would reprint
the bearer ticket into the journal on every restart:

```bash
oximux pair-new              # full-write enrollment (the default)
oximux pair-new --read-only  # opt the enrollment down to watching
oximux pair-ls               # every enrollment, tier included
oximux pair-rm <pubkey>      # erase one (it may pair again with a new ticket)
```

`pair-new` prints a QR + ticket to an **interactive terminal only** and
refuses a pipe (override consciously with `--force-non-tty`). Tickets are
one-time and expire after ~2 minutes; those three properties carry the
write-by-default tier — do not script around them casually. Only the local
operator may run the pair verbs: no paired device can mint further
enrollments, whatever its tier.

## systemd (Linux)

`/etc/systemd/system/oximux.service`, running as the user who owns the
repositories (never root):

```ini
[Unit]
Description=OxiMux headless host
After=network-online.target
Wants=network-online.target

[Service]
User=dev
ExecStart=/usr/local/bin/oximux serve --project /home/dev/work/repo-a --project /home/dev/work/repo-b
# The agent CLIs live on the user's PATH, which a unit does not inherit.
Environment=PATH=/home/dev/.local/bin:/usr/local/bin:/usr/bin:/bin
Environment=RUST_LOG=info
# Drain semantics. `mixed` sends SIGTERM to the MAIN process only, so serve
# runs its drain while the agent children keep working until it finishes;
# `control-group` would SIGTERM every agent mid-turn. TimeoutStopSec exceeds
# serve's 20 s drain deadline so systemd never SIGKILLs a drain in progress.
KillMode=mixed
TimeoutStopSec=30
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now oximux
journalctl -u oximux -f        # logs (stderr); the one stdout line is the readiness JSON
```

Project roots can also live in `<data-dir>/projects.toml`:

```toml
projects = ["/home/dev/work/repo-a", "/home/dev/work/repo-b"]
```

## Windows

Serve handles the console-close and OS-shutdown notifications as its drain
signal, so the supported headless path is a **Scheduled Task** (runs at logon
or at startup with stored credentials):

```powershell
$action  = New-ScheduledTaskAction -Execute "C:\Program Files\OxiMux\oximux.exe" `
           -Argument "serve --project C:\work\repo-a"
$trigger = New-ScheduledTaskTrigger -AtLogOn
Register-ScheduledTask -TaskName "OxiMux Serve" -Action $action -Trigger $trigger
```

Stopping the task delivers the close notification and serve drains exactly as
under systemd. Agent children are console children of serve; the job-object
teardown that guarantees no orphaned tree on a hard kill ships with the SCM
service wrapper, which is tracked as a follow-up — until then a hard task
kill can leave an agent process behind (a drain-stop does not).

## Verifying an install

```bash
oximux status            # reaches the local socket, prints versions + counts
oximux ls                # sessions persisted on this host (dormant included)
oximux run --bg "hello"  # spawns a real agent session (agent CLI required)
```

The readiness line's `endpointId` matches what every pairing ticket names, so
`pair-new`'s output can be cross-checked against the journal's readiness line
without ever logging the ticket itself.
