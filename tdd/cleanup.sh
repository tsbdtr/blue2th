#!/usr/bin/env bash
#
# Cleanup phase of the /tdd cycle: verify the feature's pull request was merged,
# then remove the worktree, delete the branch and reset the spec templates.
#
#   tdd/cleanup.sh [slug]
#
# With no argument the slug is derived from the Feature Name in tdd/feature.md,
# the same rule the skill uses.
#
# This lives in a script rather than inside the skill so that a post-merge hook
# can call it too (#17). Anything the skill would have to re-implement belongs
# here; the skill only reports what this prints.
#
# Exit codes: 0 cleaned up · 1 refused (nothing was touched) · 2 misuse.

set -euo pipefail

die() { printf '%s\n' "$*" >&2; exit 1; }

# ── Preconditions ────────────────────────────────────────────────────────────

command -v gh >/dev/null 2>&1 || die "gh is not installed — cleanup needs it to read the pull request state."
gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run: gh auth login"

ROOT=$(git rev-parse --show-toplevel) || die "not inside a git repository"
cd "$ROOT"

# The whole script assumes the main checkout is on develop: it pulls into the
# current branch further down, and deletes a branch you cannot be standing on.
# Checked here rather than there so it fails before the first network call.
# Also catches a detached HEAD, which reads back as "HEAD".
CURRENT=$(git rev-parse --abbrev-ref HEAD)
[ "$CURRENT" = "develop" ] || die "cleanup expects the main checkout on develop, found '$CURRENT'. Switch first — it pulls and deletes a branch."

# ── Slug ─────────────────────────────────────────────────────────────────────

slugify() {
    printf '%s' "$1" \
        | iconv -f UTF-8 -t ASCII//TRANSLIT 2>/dev/null || printf '%s' "$1"
}

if [ $# -ge 1 ] && [ -n "$1" ]; then
    SLUG="$1"
else
    [ -f tdd/feature.md ] || die "tdd/feature.md not found and no slug given."
    NAME=$(awk '/^## Feature Name/{found=1; next} found && !/^<!--/ && NF {print; exit}' tdd/feature.md)
    [ -n "$NAME" ] || die "no Feature Name in tdd/feature.md — pass the slug as an argument."
    [ "$NAME" != "PENDING" ] || die "tdd/feature.md is already reset to PENDING — pass the slug as an argument."
    SLUG=$(slugify "$NAME" \
        | tr '[:upper:]' '[:lower:]' \
        | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//')
fi

[ -n "$SLUG" ] || die "empty slug"

BRANCH="feat/$SLUG"
WORKTREE_PATH="$(dirname "$ROOT")/blue2th-$SLUG"

echo "Feature: $SLUG"
echo "Branch:  $BRANCH"

# ── The pull request must be MERGED, not merely gone ─────────────────────────
#
# `state` has three values, and two of them mean "do not clean up". Treating
# anything that is not OPEN as done would delete the work of an abandoned branch.

PR_JSON=$(gh pr list --head "$BRANCH" --state all --limit 1 \
            --json number,state,mergedAt,url 2>/dev/null || echo '[]')

if [ "$(printf '%s' "$PR_JSON" | jq 'length')" -eq 0 ]; then
    die "No pull request found for $BRANCH. Open one with /tdd pr before cleaning up."
fi

PR_NUMBER=$(printf '%s' "$PR_JSON" | jq -r '.[0].number')
PR_STATE=$(printf '%s' "$PR_JSON" | jq -r '.[0].state')
PR_URL=$(printf '%s' "$PR_JSON" | jq -r '.[0].url')

case "$PR_STATE" in
    MERGED) ;;
    OPEN)
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

git pull --ff-only origin develop

# ── Worktree and branch ──────────────────────────────────────────────────────

if git worktree list --porcelain | grep -qF "worktree $WORKTREE_PATH"; then
    # --force: target/ is always present and untracked.
    git worktree remove --force "$WORKTREE_PATH"
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
