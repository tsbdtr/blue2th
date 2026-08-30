#!/usr/bin/env bash
#
# Cleanup phase of the /tdd cycle: verify the feature's pull request was merged,
# then remove the worktree, delete the branch and reset the working files.
#
#   tdd/cleanup.sh [--from-hook] [slug]
#
# With no slug the feature worktree is found by its own marker — the
# .tdd-base-sha file the skill writes into it. The branch comes from git's
# worktree list, never from a name rebuilt out of the spec: tdd/feature.md is a
# working file that may already describe the *next* feature by the time this
# runs, and guessing a branch to delete from a stale guess is not a mistake worth
# risking.
#
# --from-hook is for .githooks/post-merge: it skips the pull (the merge that
# fired the hook *is* the pull — calling it again recurses), and turns every
# unmet precondition into a silent exit 0, because a hook runs on every merge and
# most of them have nothing to do with a feature branch.
#
# This lives in a script rather than inside the skill so that a post-merge hook
# can call it too (#17). Anything the skill would have to re-implement belongs
# here; the skill only reports what this prints.
#
# Exit codes: 0 cleaned up, or nothing to do · 1 refused (nothing was touched) · 2 misuse.

set -euo pipefail

die() { printf '%s\n' "$*" >&2; exit 1; }

FROM_HOOK=0
if [ "${1:-}" = "--from-hook" ]; then
    FROM_HOOK=1
    shift
fi

# A precondition that is a real error when a human typed the command, and simply
# "not now" when a hook fired on an unrelated merge.
bail() {
    if [ "$FROM_HOOK" -eq 1 ]; then
        exit 0
    fi
    die "$@"
}

# ── Preconditions ────────────────────────────────────────────────────────────

command -v gh >/dev/null 2>&1 || bail "gh is not installed — cleanup needs it to read the pull request state."
gh auth status >/dev/null 2>&1 || bail "gh is not authenticated — run: gh auth login"

ROOT=$(git rev-parse --show-toplevel) || die "not inside a git repository"
cd "$ROOT"

# The whole script assumes the main checkout is on develop: it pulls into the
# current branch further down, and deletes a branch you cannot be standing on.
# Checked here rather than there so it fails before the first network call.
# Also catches a detached HEAD, which reads back as "HEAD".
CURRENT=$(git rev-parse --abbrev-ref HEAD)
[ "$CURRENT" = "develop" ] || bail "cleanup expects the main checkout on develop, found '$CURRENT'. Switch first — it pulls and deletes a branch."

# ── Which feature ────────────────────────────────────────────────────────────
#
# Found by marker, not by name. Every feature worktree carries .tdd-base-sha,
# written by the skill when it created the worktree; git itself knows which
# branch is checked out there. So both the path and the branch come from facts
# recorded at creation time, and nothing is rebuilt from tdd/feature.md — which
# is a working file, gitignored, and may already hold the next feature's spec.

# Each line: <worktree path><TAB><branch>, skipping the main checkout.
tdd_worktrees() {
    git worktree list --porcelain | awk -v root="$ROOT" '
        /^worktree /  { wt = substr($0, 10) }
        /^branch /    { br = substr($0, 8); sub(/^refs\/heads\//, "", br) }
        /^$/          { if (wt != "" && wt != root) print wt "\t" br; wt = ""; br = "" }
        END           { if (wt != "" && wt != root) print wt "\t" br }
    '
}

if [ $# -ge 1 ] && [ -n "$1" ]; then
    # Explicit slug: the escape hatch for a worktree already gone, or one whose
    # marker was lost.
    SLUG="$1"
    BRANCH="feat/$SLUG"
    WORKTREE_PATH="$(dirname "$ROOT")/blue2th-$SLUG"
else
    # `if` rather than `[ … ] && printf`: the latter leaves the loop's exit
    # status at the last iteration's test, so a non-/tdd worktree listed last
    # made the whole substitution fail — and `set -e` turned that into the
    # script aborting for no reason. Caught by testing it with two worktrees.
    MATCHES=$(tdd_worktrees | while IFS=$'\t' read -r wt br; do
        if [ -f "$wt/.tdd-base-sha" ]; then
            printf '%s\t%s\n' "$wt" "$br"
        fi
    done)

    COUNT=$(printf '%s' "$MATCHES" | grep -c . || true)
    case "$COUNT" in
        0) bail "No /tdd worktree found (none carries .tdd-base-sha). Pass the slug if the worktree is already gone." ;;
        1) ;;
        *) bail "$COUNT /tdd worktrees are open; cleanup acts on one feature. Pass the slug:
$MATCHES" ;;
    esac

    WORKTREE_PATH=$(printf '%s' "$MATCHES" | cut -f1)
    BRANCH=$(printf '%s' "$MATCHES" | cut -f2)
    [ -n "$BRANCH" ] || bail "The worktree at $WORKTREE_PATH has a detached HEAD; cleanup needs a branch."
    SLUG=${BRANCH#*/}
fi

[ -n "$SLUG" ] || die "empty slug"

echo "Feature: $SLUG"
echo "Branch:  $BRANCH"

# ── The pull request must be MERGED, not merely gone ─────────────────────────
#
# `state` has three values, and two of them mean "do not clean up". Treating
# anything that is not OPEN as done would delete the work of an abandoned branch.

PR_JSON=$(gh pr list --head "$BRANCH" --state all --limit 1 \
            --json number,state,mergedAt,url 2>/dev/null || echo '[]')

if [ "$(printf '%s' "$PR_JSON" | jq 'length')" -eq 0 ]; then
    bail "No pull request found for $BRANCH. Open one with /tdd pr before cleaning up."
fi

PR_NUMBER=$(printf '%s' "$PR_JSON" | jq -r '.[0].number')
PR_STATE=$(printf '%s' "$PR_JSON" | jq -r '.[0].state')
PR_URL=$(printf '%s' "$PR_JSON" | jq -r '.[0].url')

case "$PR_STATE" in
    MERGED) ;;
    OPEN)
        # The common case for a hook: you pulled develop for some other reason
        # while the feature is still in review. Say nothing.
        [ "$FROM_HOOK" -eq 1 ] && exit 0
        echo
        echo "PR #$PR_NUMBER is still open — nothing cleaned up."
        echo "$PR_URL"
        CHECKS=$(gh pr checks "$PR_NUMBER" 2>&1 | grep -Ei '\bfail' || true)
        if [ -n "$CHECKS" ]; then
            echo
            echo "It is open because CI is red:"
            printf '%s\n' "$CHECKS"
        else
            echo "CI reports no failure; it is waiting on a human."
        fi
        exit 1
        ;;
    CLOSED)
        # Never silent, even from a hook: abandoned work still on disk is
        # something to know about, and nothing is deleted either way.
        echo
        echo "PR #$PR_NUMBER was closed WITHOUT being merged — this work was abandoned."
        echo "$PR_URL"
        echo "Nothing was cleaned up. Delete the worktree by hand if that is really what you want:"
        echo "  git worktree remove --force $WORKTREE_PATH && git branch -D $BRANCH"
        exit 1
        ;;
    *)
        die "Unexpected pull request state '$PR_STATE' for #$PR_NUMBER."
        ;;
esac

echo "PR #$PR_NUMBER is merged — cleaning up."

# ── Pull first ───────────────────────────────────────────────────────────────
#
# Order matters: without this, local develop does not know about the merge and
# `git branch -d` refuses the branch as unmerged.
#
# This pulls into the current branch, which the precondition at the top of the
# script has already established is develop.

if [ "$FROM_HOOK" -eq 1 ]; then
    # The merge that fired this hook is the pull. Calling it again here would
    # recurse through post-merge.
    echo "Skipping the pull — the merge that fired the hook already landed develop."
else
    git pull --ff-only origin develop
fi

# ── Worktree and branch ──────────────────────────────────────────────────────

if git worktree list --porcelain | grep -qF "worktree $WORKTREE_PATH"; then
    # No --force, deliberately. It used to be passed "because target/ is always
    # present and untracked" — but target/ is gitignored, and `git worktree
    # remove` ignores ignored files: it was never needed. Without it, git refuses
    # on a file that is genuinely untracked, which is exactly the wanted
    # behaviour when this runs unattended from a hook: someone's unsaved work in
    # the worktree stops the deletion instead of being discarded by it.
    if ! git worktree remove "$WORKTREE_PATH"; then
        echo
        echo "Worktree kept: it holds files git does not know about."
        echo "Look at them, then either save them or remove it by hand:"
        echo "  git worktree remove --force $WORKTREE_PATH && git branch -d $BRANCH"
        exit 1
    fi
    echo "Worktree removed: $WORKTREE_PATH"
else
    echo "No worktree at $WORKTREE_PATH — skipping."
fi

if git show-ref --verify --quiet "refs/heads/$BRANCH"; then
    git branch -d "$BRANCH"
    echo "Branch deleted: $BRANCH"
else
    echo "No local branch $BRANCH — skipping."
fi

# ── Reset the spec templates ─────────────────────────────────────────────────

# The .tdd-base-sha and .tdd-issue markers live inside the worktree, so removing
# it above already took them; nothing to clean here.
# Copied from the versioned templates, not `git checkout HEAD --`: that only
# discards *uncommitted* changes, so the day a filled spec reached develop the
# checkout became a no-op and this script announced a reset it had not done.
# A copy restores the template whatever HEAD holds.
cp tdd/feature.template.md tdd/feature.md
cp tdd/REVIEW.template.md tdd/REVIEW.md
echo "tdd/feature.md and tdd/REVIEW.md reset from their templates."

# ── Knowledge graph ──────────────────────────────────────────────────────────
#
# Refreshed (per #12) but no longer committed: the whole of graphify-out/ is
# gitignored, so there is nothing to stage. That also removes the one place where
# this script wrote straight to develop — the branching model says every change
# reaches it through a pull request, and a regenerated artifact was a silent
# exception to that.

if command -v graphify >/dev/null 2>&1; then
    if graphify update . >/dev/null 2>&1; then
        echo "Knowledge graph refreshed (local only, not tracked)."
    else
        echo "graphify update failed — the local graph is stale; rerun: graphify update ."
    fi
else
    echo "graphify not found — knowledge graph not updated."
fi

echo
echo "Cleaned up $SLUG (PR #$PR_NUMBER)."
