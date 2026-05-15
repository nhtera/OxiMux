#!/usr/bin/env bash
#
# Pre-commit guard: warn when a staged diff deletes a `pub fn` / `pub struct`
# / `pub enum` declaration. This is the producer/consumer guard from the v0.9
# post-mortem — deleting a producer without auditing consumers caused dead
# UI code to linger for weeks.
#
# Install:
#   ln -sf ../../scripts/pre-commit-producer-consumer.sh .git/hooks/pre-commit
#
# This script never blocks the commit. It only prints a warning so the dev
# eyeballs the diff. Hard-blocking would slow refactors without catching the
# real failure mode, which is forgetting to delete consumers — not the
# deletion itself.

set -u

added_or_modified=$(git diff --cached --name-only --diff-filter=AM | grep -E '\.rs$' || true)
if [[ -z "$added_or_modified" ]]; then
    exit 0
fi

deleted_pubs=$(
    git diff --cached -U0 -- '*.rs' \
        | grep -E '^-[[:space:]]*pub (fn|struct|enum|trait|const|static) ' \
        || true
)

if [[ -n "$deleted_pubs" ]]; then
    echo "==> producer/consumer guard"
    echo "    Staged commit deletes public symbol(s). Audit consumers before merging:"
    echo
    echo "$deleted_pubs" | sed 's/^/      /'
    echo
    echo "    (Warning only — commit will proceed.)"
fi

exit 0
