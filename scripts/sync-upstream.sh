#!/usr/bin/env bash
# Sync our fork patch branch with upstream turbovec.
#
# Remote naming in this repo:
#   origin = upstream (RyanCodrai/turbovec)
#   fork   = ours     (ai-pipestream/turbovec)
#
# The fork carries a small patch stack (seeded top-k floor, streaming
# collector — see FORK.md) rebased onto origin/main. Each sync publishes
# a NEW branch turbovec-pipestream-s<N+1> because the rebase rewrites
# history; old -sN branches are left alone (no force-push), and
# turbovec-search flips its Cargo.toml branch to the new one.
#
# Usage: scripts/sync-upstream.sh [--push] [--skip-tests]
#   --push        after a clean rebase + green tests, push the new -sN
#                 branch to fork
#   --skip-tests  do not run cargo test before publishing (not
#                 recommended; the test suite is the rebase gate)
#
# Exit codes: 0 synced (or already up to date), 1 needs manual help.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

PUSH=0
RUN_TESTS=1
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    --skip-tests) RUN_TESTS=0 ;;
    *) echo "sync-upstream: unknown argument '$arg'" >&2; exit 1 ;;
  esac
done

if [ -n "$(git status --porcelain)" ]; then
  echo "sync-upstream: working tree is dirty; commit or stash first." >&2
  exit 1
fi

git fetch origin --prune
git fetch fork --prune

# Find the current chain tip: the highest -sN branch on fork (or local).
latest=$(git for-each-ref --format='%(refname:short)' \
    'refs/remotes/fork/turbovec-pipestream-s[0-9]*' \
    'refs/heads/turbovec-pipestream-s[0-9]*' \
  | sed 's|.*/turbovec-pipestream-s||' | sort -n | tail -1)
if [ -z "$latest" ]; then
  echo "sync-upstream: no turbovec-pipestream-sN branch found; nothing to sync." >&2
  exit 1
fi
cur="turbovec-pipestream-s${latest}"
next="turbovec-pipestream-s$((latest + 1))"
cur_ref="fork/${cur}"
git show-ref --verify --quiet "refs/remotes/${cur_ref}" || cur_ref="${cur}"

echo "sync-upstream: chain tip is ${cur} ($(git rev-parse --short "${cur_ref}"))"

if [ "$(git rev-parse origin/main)" = "$(git merge-base "${cur_ref}" origin/main)" ]; then
  echo "sync-upstream: already up to date with origin/main ($(git rev-parse --short origin/main))."
  exit 0
fi

new_commits=$(git log --oneline "${cur_ref}"..origin/main | wc -l)
echo "sync-upstream: ${new_commits} new upstream commit(s); rebasing as ${next}"
git log --oneline "${cur_ref}"..origin/main

git checkout -b "${next}" "${cur_ref}"
if ! git rebase origin/main; then
  cat >&2 <<EOF
sync-upstream: CONFLICT rebasing ${next} onto origin/main.
Resolve manually, then:
  git rebase --continue
Or back out entirely:
  git rebase --abort && git branch -D ${next}
The recurring seams are the two patch commits: every new heap-init
site in the search kernels must seed floor_seed instead of
NEG_INFINITY, and search_streaming must ride the same chunk loop as
the top-k scan.
EOF
  exit 1
fi

# Point FORK.md at the new branch name.
sed -i "s/turbovec-pipestream-s[0-9][0-9]*/${next}/g" FORK.md
if ! git diff --quiet FORK.md; then
  git commit -am "Name the chain branch FORK.md actually lives on (${next#turbovec-pipestream-})"
fi

if [ "${RUN_TESTS}" = 1 ]; then
  echo "sync-upstream: running cargo test -p turbovec (the rebase gate)"
  if ! cargo test -p turbovec; then
    cat >&2 <<EOF
sync-upstream: TESTS FAILED on ${next}. The rebase applied but the
patches no longer hold. Fix on ${next} (amend the patch commits with
interactive rebase, or add a fixup commit and note it in FORK.md),
re-run the tests, then push with:
  git push fork ${next}
EOF
    exit 1
  fi
fi

echo "sync-upstream: done. ${next} sits on origin/main ($(git rev-parse --short origin/main))."

if [ "${PUSH}" = 1 ]; then
  git push fork "${next}"
  echo "sync-upstream: pushed ${next} to fork (new branch, no force-push)."
  cat <<EOF
Next step: in turbovec-search, set
  turbovec = { git = "https://github.com/ai-pipestream/turbovec.git", branch = "${next}" }
then cargo update -p turbovec && cargo test, commit, push forgejo first.
EOF
else
  echo "sync-upstream: not pushed (pass --push). Branch ${next} is local only."
fi
