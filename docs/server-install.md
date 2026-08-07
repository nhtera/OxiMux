# Running `oximux serve` on a server

`oximux serve` turns the same binary the CLI ships into a headless OxiMux
host: agent sessions, transcripts, terminals (via the relay daemon), the local
CLI socket, and the paired-device endpoint — everything the desktop app hosts,
minus every window. A phone or laptop pairs with it exactly as it pairs with
the desktop.

## Installing and updating

```bash
curl -sSL https://raw.githubusercontent.com/nhtera/OxiMux/main/scripts/install-cli.sh | sh
```

The installer resolves the platform, verifies the release manifest's minisign
signature before trusting any checksum, and lands `oximux` + `oximux-relay`
together (`--dir <DIR>` overrides the destination; PowerShell:
`scripts/install-cli.ps1`). macOS/Linuxbrew alternatively:
`brew install nhtera/tap/oximux`.

Update in place with `oximux update` (`--check` to only ask). It verifies the
same signed manifest with a key built into the binary, refuses anything not
newer, swaps both binaries together, and **never restarts a running serve** —
restart it yourself to pick the new version up. A Homebrew-managed install is
refused by the updater; use `brew upgrade oximux` there.

## Contract

- **stdout carries exactly one line**, the readiness JSON, and nothing else —
  ever. A journal that captures stdout can never capture a secret:

  ```json
  {"type":"oximux_serve_ready","schemaVersion":1,"protocolVersion":19,"dataDir":"/var/lib/oximux","endpointId":"<64 hex>"}
  ```

  `dataDir` is the directory, not the socket — pass it straight back as
  `oximux --dir`. The socket itself is `<dataDir>/control-v1.sock`. Shown with a
  real path on purpose: this example used to elide the value as `"…"`, and while
  it did, the field was called `localSocket` and nothing here could contradict it.

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

The supported headless path is the **SCM service**. From an elevated prompt:

```powershell
oximux serve --install-service --data-dir C:\OxiMux\data --project C:\work\repo-a
sc start OxiMuxServe
```

`--data-dir` is required at install (not defaulted): the service runs as
LocalSystem, whose per-user default data directory is not yours. Repoint the
service at a user account afterwards via `services.msc` or `sc config` when
agents need that user's toolchains and credentials.

Stopping (`sc stop OxiMuxServe`, or system shutdown) maps
`SERVICE_CONTROL_STOP` onto the same drain the unix SIGTERM path takes: the
service reports `STOP_PENDING` with a 45-second wait hint — above serve's
20-second drain deadline plus its transcript flush — lets in-flight agent
turns finish, marks stragglers interrupted, and reports `STOPPED`. At start
the service adopts itself into a kill-on-close job object, so even a hard
kill (`taskkill /f`, a crash) takes every agent child and grandchild with the
process — nothing orphans. Remove with `oximux serve --uninstall-service`.

A **Scheduled Task** still works where installing a service is not an option
(serve treats console-close and OS-shutdown notifications as its drain
signal), but a hard task kill has no job-object guarantee there:

```powershell
$action  = New-ScheduledTaskAction -Execute "C:\Program Files\OxiMux\oximux.exe" `
           -Argument "serve --project C:\work\repo-a"
$trigger = New-ScheduledTaskTrigger -AtLogOn
Register-ScheduledTask -TaskName "OxiMux Serve" -Action $action -Trigger $trigger
```

## Verifying an install

```bash
oximux version  # this CLI's own build + protocol versions — offline, no host needed
oximux status   # reaches the booted host over the local socket; versions + counts
oximux ls       # the host answers with its session list (empty is fine; no reply is not)
```

The readiness line's `endpointId` matches what every pairing ticket names, so
`pair-new`'s output can be cross-checked against the journal's readiness line
without ever logging the ticket itself.
