#!/usr/bin/env bash
# Drive the REAL `oximux serve` and `oximux worktree` against a REAL git repo.
#
#   scripts/live-verify-worktree.sh                        # target/debug/oximux-cli
#   scripts/live-verify-worktree.sh path/to/oximux         # or a binary you name
#
# Exits 0 when every check passes, 1 otherwise, and prints one PASS/FAIL line
# per check so a failure names the property that broke.
#
# WHY THIS EXISTS, and why the unit suite is not enough.
#
# The worktree engine is a pile of `git` subprocesses whose behaviour depends on
# the SHAPE of the repository they run in — and a fixture is a repository shape
# somebody chose. When phase 2 (base refs) shipped, 5761 unit tests passed over
# a worktree being cut from the wrong branch entirely, because every fixture had
# a local `main` and the field frequently does not: a worktree-centric checkout
# routinely has its default branch only at `origin/main`, the local copy having
# been deleted by a user who never sits on it. In that shape `git worktree add`
# DWIMs a bare `main` into `--track -b main` and silently OVERRIDES an explicit
# `-b`, so creation reported success while the worktree sat on the wrong branch
# and the `workspaces` row named a branch that did not exist.
#
# So the repo this builds is deliberately shaped like the FAILURE, not like a
# happy path: default branch remote-only, checkout parked on a feature branch,
# and a second branch to adopt. Run it after touching anything under
# `crates/git/src/worktree.rs`, `crates/worktree-ops`, or the CLI's worktree
# verbs. It is not wired into CI (it wants a built binary and a writable temp
# dir); it is a thing you run before you believe a green suite.
#
# NOT `set -o pipefail`: several checks read from commands that exit non-zero by
# design (usage errors), and pipefail would turn a matching grep into a failure.
set -u

CLI="${1:-target/debug/oximux-cli}"
[ -x "$CLI" ] || { echo "no CLI at $CLI — build it first: cargo build -p oximux-cli" >&2; exit 2; }
CLI=$(cd "$(dirname "$CLI")" && pwd)/$(basename "$CLI")
# `/usr/bin/sqlite3` explicitly: PATH may resolve to an Android SDK build whose
# behaviour is not the system one's.
SQLITE=/usr/bin/sqlite3
for tool in git python3 "$SQLITE"; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing prerequisite: $tool" >&2; exit 2; }
done

FAILED=0
say() { printf '\n=== %s ===\n' "$*"; }
ok()  { printf 'PASS  %s\n' "$*"; }
bad() { printf 'FAIL  %s\n' "$*"; FAILED=1; }

ROOT=$(mktemp -d)
SERVE_PID=""
# SIGTERM, then a bounded wait, then SIGKILL. A plain `kill` is not enough:
# `serve` does not always exit promptly, and the temp dir below would then be
# removed out from under a live host. A process wedged in uninterruptible state
# survives even the SIGKILL — that is a machine-level fault, not this script's,
# and it clears on reboot.
cleanup() {
  if [ -n "$SERVE_PID" ]; then
    kill "$SERVE_PID" 2>/dev/null
    for _ in $(seq 1 20); do kill -0 "$SERVE_PID" 2>/dev/null || break; sleep 0.25; done
    kill -9 "$SERVE_PID" 2>/dev/null
    wait "$SERVE_PID" 2>/dev/null
  fi
  rm -rf "$ROOT" 2>/dev/null
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# A repository shaped like the failure case.
# ---------------------------------------------------------------------------
git init -q --bare "$ROOT/origin"
git init -q "$ROOT/repo"
cd "$ROOT/repo" || exit 2
git config user.email live@verify.test; git config user.name Live; git config commit.gpgsign false
echo v1 > a.txt; git add a.txt; git commit -qm init
git branch -m main
git remote add origin "$ROOT/origin"; git push -q origin main
git branch side                                          # a branch to adopt
echo v2 > b.txt; git add b.txt; git commit -qm second    # main moves past side
git checkout -q -b wip                                   # park the checkout off-default
echo wip > c.txt; git add c.txt; git commit -qm wip
git branch -D main -q                                    # the local default is gone
git remote set-head origin main

say "repo state"
echo "local branches: $(git branch --format='%(refname:short)' | tr '\n' ' ')"
echo "HEAD: $(git branch --show-current)  (deliberately NOT the default branch)"

# ---------------------------------------------------------------------------
# The real host.
# ---------------------------------------------------------------------------
say "booting oximux serve"
"$CLI" serve --data-dir "$ROOT/data" > "$ROOT/serve.out" 2> "$ROOT/serve.err" &
SERVE_PID=$!
for _ in $(seq 1 60); do [ -s "$ROOT/serve.out" ] && break; sleep 0.5; done
DIR=$(python3 -c "import json;print(json.load(open('$ROOT/serve.out'))['dataDir'])" 2>/dev/null)
[ -n "$DIR" ] || { echo "serve never became ready:"; tail -5 "$ROOT/serve.err"; exit 1; }
echo "host up, dataDir=$DIR"

# The CLI has no `projects add`, so register the project the way the desktop
# would — straight into the host's own database.
"$SQLITE" "$ROOT/data/oximux.db" \
  "INSERT INTO projects (id,name,root_path,default_branch,created_at) \
   VALUES ('p1','Live','$ROOT/repo','main','2026-01-01T00:00:00Z');" \
  || { echo "could not seed the project row" >&2; exit 1; }

wt()        { "$CLI" --dir "$DIR" --json worktree "$@"; }
branch_of() { python3 -c "import json,sys;print(json.load(sys.stdin)['data']['branch'])"; }
path_of()   { python3 -c "import json,sys;print(json.load(sys.stdin)['data']['path'])"; }
id_of()     { python3 -c "import json,sys;print(json.load(sys.stdin)['data']['id'])"; }

# ---------------------------------------------------------------------------
say "1. plain create — the default branch lives only on the remote"
OUT=$(wt create feat --project "$ROOT/repo" 2>&1)
B=$(echo "$OUT" | branch_of 2>/dev/null)
[ "$B" = "oximux/feat" ] && ok "branch is $B (git's DWIM did not hijack it onto 'main')" \
                         || bad "branch is '$B', expected oximux/feat -- $OUT"
git rev-parse --verify --quiet main >/dev/null && bad "a local 'main' was created behind our back" \
                                                || ok "no stray local 'main'"
git rev-parse --verify --quiet oximux/feat >/dev/null && ok "oximux/feat exists (the row does not name a phantom)" \
                                                      || bad "oximux/feat missing — the row names a branch that was never made"
[ "$(git rev-parse oximux/feat)" = "$(git rev-parse origin/main)" ] \
  && ok "based on origin/main, not on the wip HEAD" \
  || bad "based on $(git rev-parse --short oximux/feat), wanted origin/main $(git rev-parse --short origin/main)"
[ -f "$(echo "$OUT" | path_of)/c.txt" ] && bad "wip work leaked into the worktree" || ok "no wip files in the worktree"

say "2. create --from <ref>"
OUT=$(wt create fromside --project "$ROOT/repo" --from side 2>&1)
B=$(echo "$OUT" | branch_of 2>/dev/null)
[ "$B" = "oximux/fromside" ] && ok "branch is $B" || bad "branch is '$B' -- $OUT"
[ "$(git rev-parse oximux/fromside)" = "$(git rev-parse side)" ] && ok "cut from 'side' exactly" || bad "not based on side"

say "3. create --branch <name> (adopt)"
OUT=$(wt create --project "$ROOT/repo" --branch side 2>&1)
B=$(echo "$OUT" | branch_of 2>/dev/null); ADOPTED_ID=$(echo "$OUT" | id_of 2>/dev/null)
[ "$B" = "side" ] && ok "adopted 'side' under its own name" || bad "branch is '$B', expected side -- $OUT"
git branch --format='%(refname:short)' | grep -q '^oximux/side$' \
  && bad "a prefixed branch was minted anyway" || ok "no prefixed branch minted"

say "4. flag-shaped and revision-shaped refs are refused before git runs"
for v in "-B main" "--force" "main..side" "side~1" "main@{1}"; do
  if "$CLI" --dir "$DIR" worktree create probe --project "$ROOT/repo" --from "$v" >/dev/null 2>&1
  then bad "--from '$v' was ACCEPTED"; else ok "--from '$v' refused"; fi
done

say "5. usage rules"
OUT=$("$CLI" --dir "$DIR" worktree create x --project "$ROOT/repo" --from main --branch side 2>&1)
case "$OUT" in *"cannot be used with"*) ok "--from/--branch mutually exclusive";;
                                     *) bad "mutual exclusion not enforced -- $OUT";; esac
# A usage error must not depend on a host being up.
OUT=$("$CLI" --dir /nonexistent worktree create 2>&1)
case "$OUT" in *"wants a slug"*) ok "missing-slug usage error needs no reachable host";;
                              *) bad "usage error still requires a host -- $OUT";; esac

say "6. deleting an ADOPTED worktree must not delete the branch"
SIDE_SHA=$(git rev-parse side)
"$CLI" --dir "$DIR" worktree rm "$ADOPTED_ID" >/dev/null 2>&1
if git rev-parse --verify --quiet side >/dev/null; then
  ok "adopted branch 'side' survived the delete"
  [ "$(git rev-parse side)" = "$SIDE_SHA" ] && ok "and still points where it did" || bad "'side' moved"
else
  bad "DATA LOSS: the adopted branch was deleted"
fi

say "7. deleting a MINTED worktree must still clean its branch up"
MINTED_ID=$(wt ls 2>/dev/null | python3 -c "
import json,sys
print(next(r['id'] for r in json.load(sys.stdin)['data'] if r['branch']=='oximux/fromside'))" 2>/dev/null)
"$CLI" --dir "$DIR" worktree rm "$MINTED_ID" >/dev/null 2>&1
git rev-parse --verify --quiet oximux/fromside >/dev/null \
  && bad "a minted branch leaked — cleanup regressed" || ok "minted branch cleaned up"

say "RESULT"
[ "$FAILED" = 0 ] && echo "ALL LIVE CHECKS PASSED" || echo "SOME LIVE CHECKS FAILED"
exit $FAILED
