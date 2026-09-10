# Worktree provisioning

A git worktree gives you every **tracked** file and none of the untracked ones.
That is correct for git and useless in practice: the `.env`, the dev certs, and
the `node_modules` your project needs are all things a fresh worktree does not
have. Provisioning is the step that closes that gap, so a worktree OxiMux
reports as created is one you can actually work in.

Two things happen, in this order, between `git worktree add` and the workspace
appearing in the sidebar:

1. **Copy** the untracked local files named in `.oximuxinclude`.
2. **Run** the project's `setup` script.

The order is fixed, because setup scripts read the files the copy brings.

**A setup failure fails creation.** The worktree, the branch, and the database
row are all rolled back — there is no half-provisioned worktree left behind.
This is the opposite of the `cleanup` script, which is best-effort by design: a
teardown script must never trap you behind a delete that will not complete,
while a setup script that failed means the worktree is not ready.

## Opting in

Provisioning is **off by default**. A project that has never configured it
behaves exactly as it did before this existed.

```toml
# .oximux/scripts.toml — committed, shared by the team
auto_setup = true
setup   = "pnpm install --frozen-lockfile"
run     = "pnpm dev"
cleanup = "docker compose down"

# Terminal tabs to open in every new worktree.
default_tabs = ["server", "logs"]
```

`auto_setup` is the project's answer. The create-workspace dialog carries a
**Setup script** dropdown that overrides it for one worktree:

| Choice | Meaning |
|---|---|
| Project default | Use `auto_setup`. The default, and almost always right. |
| Run setup | Run it for this worktree even if `auto_setup` is off. |
| Skip setup | Skip it for this worktree — a throwaway branch that does not need a ten-minute install. |

The manual **Run setup** row in the rail's context menu is unchanged and is
still how you re-run setup in an existing worktree.

### An adopted branch is never deleted for you

A worktree can check out a branch that already exists (`--branch <NAME>`, or
**Existing branch** in the create dialog) instead of cutting a new one. When it
does, OxiMux records that it did not create that branch — and every path that
later removes the worktree leaves the branch alone, including **Force Delete**.

Removing a worktree is not permission to delete the branch it was sitting on.
For a worktree whose branch OxiMux minted (`<prefix>/<slug>`, the ordinary
case), deletion still removes the branch as it always has: that branch has no
life outside the worktree.

Worktrees created before this shipped are all treated as minted, which is what
they are — adopting a branch did not exist yet.

### Setup is skipped on a base you have not reviewed

Provisioning runs the worktree's **own committed** `scripts.toml`, so the
script that runs is the one on the branch you are checking out — not the one on
your default branch. That is safe while a worktree is cut from your own
checkout, and it stops being safe the moment you base one on somebody else's
branch: `--from origin/pr-4711` would otherwise run a contributor's script on
your machine, unattended, on any project with `auto_setup = true`.

So when the base is **not already part of your default branch**, setup is
skipped and the reason is shown before you commit in the dialog, and written to
the provisioning transcript afterwards:

```
Setup skipped: `pr-4711` is not based on `main`. Review the branch, then Run setup.
```

Read the script, then use **Run setup** from the row menu — or choose **Run
setup** in the dialog, which overrides the guard deliberately. The skip is a
default, not a prohibition.

A base that *is* an ancestor of the default branch provisions exactly as
before, and so does an ordinary create that names no base at all.

> Do not put secrets in `scripts.toml`. It is meant to be committed, the same
> trust boundary as `commands.toml`.

## `.oximuxinclude`

A file at the project root naming the untracked files a worktree cannot work
without. It is committed, so it names *paths*, never contents.

```gitignore
# Local config a worktree needs but git does not carry
.env
.env.local
certs/
config/*.local.json
!config/scratch.local.json
```

Syntax is gitignore's: one pattern per line, `#` comments, `!` negation,
directories copied recursively.

**Patterns are anchored at their literal prefix.** `certs/*.pem` scans `certs/`,
not the whole repository. Only a pattern that *starts* with a wildcard
(`*.pem`, `**/x`) scans from the root, and that scan is capped — if it hits the
cap it is reported as a skip telling you to anchor the pattern. This departs
from gitignore on purpose: git matches paths it has already walked, whereas here
the walk is the expensive part, and an unanchored scan of a repo containing
`node_modules` is the difference between instant and minutes.

### What the copy will and will not do

Nothing in this list fails creation. Every skip is reported in the transcript,
and the setup script is what decides whether a missing file actually mattered.

| Situation | Behavior |
|---|---|
| Path already exists in the worktree | **The tracked file wins.** Never overwritten; the skip is reported. |
| Source is a symlink | Skipped, never dereferenced. |
| Destination path crosses a symlink | Refused — a copy can never be redirected outside the worktree. |
| Pattern matches nothing | Reported; the remaining patterns still copy. |
| Source unreadable, or the write fails | Reported; the remaining patterns still copy. |
| Pattern leaves the project root (`..`) | Refused. |

The two symlink rules are a security boundary, not a nicety: a copy that
dereferenced a source symlink, or wrote through one in the destination, would
read or write outside the worktree. They are covered by
`worktree.include-copy-cannot-write-outside-the-worktree` in
[the reliability gates ledger](./reliability-gates.md), which also records that
the assertions are `#[cfg(unix)]` and therefore prove nothing on Windows yet.

### Put dependencies in the setup script, not here

`.oximuxinclude` copies files one at a time. Naming `node_modules/` will work
and will be slow. Installing dependencies is what `setup` is for.

## The transcript

Provisioning writes a live transcript to:

```
<data-dir>/projects/<project-id>/provisioning/<slug>-<millis>.log
```

It is written **outside** the worktree deliberately, because the case that most
needs reading is the one where the worktree no longer exists — a failed setup
rolls it back, and a transcript inside it would go with it. Lines are flushed as
they arrive, so a long install can be followed with `tail -f`.

On failure the transcript opens automatically in an editor tab, because the
useful information is the script's own output — the compiler error or the
missing binary — not a generic "setup failed".

A fresh file per attempt, with the newest three per slug retained. Overwriting
one file in place would look tidier and be wrong: the editor activates an
already-open tab without re-reading it, so a second failed create would show you
the *first* failure's output while the file on disk said something else.

## Bounds

| | Setup | Cleanup |
|---|---|---|
| Timeout | 15 minutes | 30 seconds |
| stdin | Closed | Closed |
| Output | Captured to the transcript | Discarded |
| On failure | Creation fails and rolls back | Logged; removal proceeds |

stdin is closed on purpose. A script that prompts for input (`npm login`, a
passphrase) fails immediately with the prompt visible in the transcript, instead
of hanging for the full fifteen minutes with nothing to show for it.
