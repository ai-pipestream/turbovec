#!/usr/bin/env bash
# Sync our fork patch branch with upstream turbovec.
#
# Remote naming in this repo:
#   origin = upstream (RyanCodrai/turbovec)
#   fork   = ours     (ai-pipestream/turbovec)
#
# The fork carries ONE branch of core patches rebased onto origin/main:
#
#   turbovec-pipestream   seeded TQ+ calibration (new_with_calibration)
#                         and the seeded top-k floor (initial_threshold)
#
# The gRPC server lives in its own repo (ai-pipestream/turbovec-grpc)
# against upstream's public API and is not part of this chain.
#
# Usage: scripts/sync-upstream.sh [--push]
#   --push   after a clean rebase, force-with-lease the branch to fork
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [ -n "$(git status --porcelain)" ]; then
  echo "sync-upstream: working tree is dirty; commit or stash first." >&2
  exit 1
fi

git fetch origin --prune

if [ "$(git rev-parse origin/main)" = "$(git merge-base turbovec-pipestream origin/main)" ]; then
  echo "sync-upstream: already up to date with origin/main ($(git rev-parse --short origin/main))."
else
  echo "sync-upstream: rebasing turbovec-pipestream onto origin/main"
  if ! git rebase origin/main turbovec-pipestream; then
    cat >&2 <<EOF
sync-upstream: CONFLICT rebasing turbovec-pipestream onto origin/main.
Resolve manually, then:
  git rebase --continue
Or back out entirely:
  git rebase --abort
The recurring seams are the two patch commits: seeded calibration must
mirror upstream's constructor fields, and every new heap-init site in
the search kernels must seed floor_seed instead of NEG_INFINITY.
EOF
    exit 1
  fi
  echo "sync-upstream: done. turbovec-pipestream now sits on origin/main ($(git rev-parse --short origin/main))."
fi

if [ "${1:-}" = "--push" ]; then
  git push fork turbovec-pipestream --force-with-lease
  echo "sync-upstream: pushed to fork."
fi
