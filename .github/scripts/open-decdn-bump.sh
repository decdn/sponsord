#!/usr/bin/env bash
# Opens the PR that moves the decdn pin to a decdn release
# (RELEASING.md § Pinning decdn).
#
# Usage:  .github/scripts/open-decdn-bump.sh [--dry-run] [vX.Y.Z]
#
# With no tag it takes decdn's latest published release. decdn's
# publish-crates.sh runs it with the tag it just published; a maintainer runs
# it by hand from any sponsord checkout.
#
# It works in a detached worktree at origin/main, so the working copy and its
# branch stay as they are, and opens the PR with your own `gh` login and git
# identity — a PR opened that way runs CI, which one opened by Actions'
# GITHUB_TOKEN would not.
#
# Nothing happens when the pin is already at that release or at a newer one,
# or when a PR from bump/decdn-<tag> exists in any state: close one to skip a
# release.
#
# --dry-run  make the change in the worktree, print it, and stop before the
#            commit
#
# Exit: 0 done or nothing to do; 1 error, or the PR is open but the new lock
# fails `cargo metadata --locked` / check-decdn-pin.sh; 2 usage; 3 crates.io
# does not serve the release yet (run it again later).
set -euo pipefail

DECDN=decdn/decdn
SPONSORD=decdn/sponsord
READY_ATTEMPTS=10
READY_INTERVAL=60

die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '5p' "${BASH_SOURCE[0]}" | sed 's/^# //' >&2; exit 2; }

DRY_RUN="" TAG=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h|--help) usage ;;
    v*) [[ -z "$TAG" ]] || usage; TAG="$arg" ;;
    *) usage ;;
  esac
done

# ---- preconditions -------------------------------------------------------

for tool in git cargo gh python3; do
  command -v "$tool" >/dev/null || die "$tool not found on PATH"
done
gh auth status >/dev/null 2>&1 || die "gh is not authenticated; run \`gh auth login\`"

REPO_ROOT=$(git rev-parse --show-toplevel) || die "not inside a git repository"
origin=$(git -C "$REPO_ROOT" remote get-url origin) || die "no \`origin\` remote"
# The PR goes to $SPONSORD, so the branch must too: a fork's origin would push
# somewhere `gh pr create` cannot see.
[[ "$origin" =~ [/:]${SPONSORD}(\.git)?/?$ ]] ||
  die "origin is $origin, not $SPONSORD; run this from a $SPONSORD checkout"

if [[ -n "$TAG" ]]; then
  [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
    die "$TAG is not a decdn release tag (vX.Y.Z)"
  state=$(gh release view "$TAG" -R "$DECDN" --json isDraft,isPrerelease \
    --jq 'if .isDraft then "draft" elif .isPrerelease then "prerelease" else "published" end') ||
    die "$DECDN has no release $TAG"
  [[ "$state" == published ]] || die "$DECDN $TAG is a $state, not a published release"
fi

# ---- worktree at origin/main ---------------------------------------------

git -C "$REPO_ROOT" fetch --quiet origin main || die "could not fetch origin main"

WORKTREE=$(mktemp -d)
cleanup() {
  # Removed through git, so the parent repository keeps no stale entry.
  git -C "$REPO_ROOT" worktree remove --force "$WORKTREE" >/dev/null 2>&1 ||
    rm -rf "$WORKTREE"
  git -C "$REPO_ROOT" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT
rmdir "$WORKTREE"   # git worktree add wants to create the directory itself
git -C "$REPO_ROOT" worktree add --quiet --detach "$WORKTREE" origin/main ||
  die "could not create a worktree at origin/main"
cd "$WORKTREE"
bump() { .github/scripts/bump-decdn.sh "$@"; }

# ---- which release -------------------------------------------------------

CURRENT=$(bump pinned) || die "could not read the decdn pin on origin/main"
if [[ -z "$TAG" ]]; then
  latest=$(bump latest) || die "could not read decdn's latest release"
  TAG=$(sed -n 's/^tag=//p' <<<"$latest")
  [[ -n "$TAG" ]] || die "could not read decdn's latest release"
fi
if [[ "$TAG" == "$CURRENT" ]]; then
  echo "sponsord main already pins decdn $TAG; nothing to do"
  exit 0
fi
# A patch release on an older line is not a bump for a pin that is already
# past it.
if [[ -n "$CURRENT" ]] &&
   [[ "$(printf '%s\n' "${CURRENT#v}" "${TAG#v}" | sort -V | tail -n1)" == "${CURRENT#v}" ]]; then
  echo "sponsord main pins decdn $CURRENT, newer than $TAG; nothing to do"
  exit 0
fi

BRANCH="bump/decdn-${TAG}"
existing=$(gh pr list -R "$SPONSORD" --head "$BRANCH" --state all \
  --json url,state --jq '.[0] | select(.) | "\(.url) (\(.state | ascii_downcase))"') ||
  die "could not list $SPONSORD PRs"
if [[ -n "$existing" ]]; then
  echo "a PR for the bump to $TAG exists: $existing; nothing to do"
  exit 0
fi

# publish-crates.sh runs this right after its uploads, and the crates.io index
# can trail them by minutes.
attempt=1
until bump ready "$TAG"; do
  rc=$?
  (( rc == 3 )) || exit "$rc"
  if (( attempt >= READY_ATTEMPTS )); then
    echo "crates.io does not serve decdn $TAG yet; run this again later" >&2
    exit 3
  fi
  echo "    waiting ${READY_INTERVAL}s for crates.io (attempt $attempt/$READY_ATTEMPTS)" >&2
  sleep "$READY_INTERVAL"
  attempt=$((attempt + 1))
done

# ---- bump ----------------------------------------------------------------

echo "==> Pinning decdn $TAG (from ${CURRENT:-a non-tag ref})"
OLD_SHA=$(bump locked-sha) || die "could not read the locked decdn commit"
bump rewrite "$TAG"
cargo update --quiet -p decdn-client -p decdn-common -p decdn-incentive -p decdn-e2e
bump changelog "$TAG"
NEW_SHA=$(bump locked-sha) || die "could not read the new locked decdn commit"

# A broken lock still gets a PR, for a human to finish; the exit code says so.
CHECK=ok
if ! cargo metadata --locked --format-version 1 >/dev/null || ! .github/scripts/check-decdn-pin.sh; then
  CHECK=failed
  echo "warning: the new lock does not check out; the PR needs work" >&2
fi

git --no-pager diff --stat
if [[ -n "$DRY_RUN" ]]; then
  echo
  echo "dry run: would push $BRANCH and open \"build: pin decdn to $TAG\""
  [[ "$CHECK" == ok ]]
  exit
fi

# ---- commit, push, PR ----------------------------------------------------

git add Cargo.toml Cargo.lock crates/*/CHANGELOG.md
# --no-verify: CI is the gate on this PR, and the hooks would compile the whole
# workspace in the middle of decdn's publish-crates.sh.
git commit --quiet --no-verify -F - <<EOF
build: pin decdn to ${TAG}

Lock the decdn crates at decdn's ${TAG} release tag (${NEW_SHA})
instead of ${CURRENT:-the previous pin} (${OLD_SHA}), and require ${TAG#v},
which is on crates.io.
EOF
# No local branch: the commit is pushed from the detached worktree. Forced,
# because the branch may be left over from a run that pushed it and opened no
# PR; a run stops above when a PR exists.
git push --quiet --force origin "HEAD:refs/heads/${BRANCH}"

others=$(gh pr list -R "$SPONSORD" --state open --json number,headRefName \
  --jq "[.[] | select(.headRefName | startswith(\"bump/decdn-\")) | select(.headRefName != \"${BRANCH}\") | \"#\\(.number)\"] | join(\", \")") ||
  others=""   # only a note in the body; not worth stranding the pushed branch
body=$(mktemp)
{
  echo "Moves the decdn pin to [decdn ${TAG}](https://github.com/${DECDN}/releases/tag/${TAG})."
  echo
  echo "- From: \`${CURRENT:-(not a tag)}\` (\`${OLD_SHA}\`)"
  echo "- To: \`${TAG}\` (\`${NEW_SHA}\`), on crates.io"
  echo "- decdn changes: https://github.com/${DECDN}/compare/${OLD_SHA}...${NEW_SHA}"
  echo
  if [[ "$CHECK" != ok ]]; then
    echo "**The lock does not check out:** \`cargo metadata --locked\` or"
    echo "\`check-decdn-pin.sh\` failed when this PR was opened."
    echo
  fi
  if [[ -n "$others" ]]; then
    echo "Older bump PRs still open: ${others}. This one supersedes them."
    echo
  fi
  echo "Opened by \`.github/scripts/open-decdn-bump.sh\` (RELEASING.md § Pinning decdn)."
  echo "A red CI means the new decdn needs code changes here: push them to"
  echo "this branch."
} > "$body"
url=$(gh pr create -R "$SPONSORD" --base main --head "$BRANCH" \
  --title "build: pin decdn to ${TAG}" --body-file "$body") ||
  die "pushed $BRANCH but could not open the PR; run this again to retry"
rm -f "$body"
echo "==> Opened $url"

[[ "$CHECK" == ok ]]
