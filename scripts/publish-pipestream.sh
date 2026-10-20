#!/usr/bin/env bash
# Publish this fork's patched turbovec as the `pipestream-turbovec` crate.
#
# The source tree keeps upstream's package name (`turbovec`) so the patch
# stack rebases onto upstream without touching Cargo.toml, CI, or the
# `-p turbovec` invocations. The rename happens only here, in a throwaway
# worktree, at publish time:
#
#   package name  turbovec        -> pipestream-turbovec
#   library name  turbovec        (unchanged: `use turbovec::...` still works)
#   version       <upstream>      -> <upstream>-pipestream.<N>
#   repository    upstream        -> ai-pipestream/turbovec
#
# <N> is the chain number from FORK.md (turbovec-pipestream-s<N>), so every
# chain branch publishes exactly one version and a published version is
# never reused (registries refuse to overwrite one).
#
# Usage: scripts/publish-pipestream.sh [--publish] [--registry NAME]
#   default       package and verify only (cargo publish --dry-run)
#   --publish     actually upload; needs a token for the registry
#   --registry    a registry named in ~/.cargo/config.toml (default: crates.io)
#
# Consumers depend on it as:
#   turbovec = { package = "pipestream-turbovec", version = "=<version>" }
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

PUBLISH=0
REGISTRY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --publish) PUBLISH=1 ;;
    --registry) REGISTRY="${2:?--registry needs a name}"; shift ;;
    *) echo "publish-pipestream: unknown argument '$1'" >&2; exit 1 ;;
  esac
  shift
done

if [ -n "$(git status --porcelain)" ]; then
  echo "publish-pipestream: working tree is dirty; commit or stash first." >&2
  exit 1
fi

chain=$(grep -o 'turbovec-pipestream-s[0-9][0-9]*' FORK.md | head -1 | sed 's/.*-s//')
if [ -z "${chain}" ]; then
  echo "publish-pipestream: FORK.md names no turbovec-pipestream-sN branch." >&2
  exit 1
fi
upstream_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' turbovec/Cargo.toml | head -1)
version="${upstream_version}-pipestream.${chain}"
head=$(git rev-parse --short HEAD)

work=$(mktemp -d)
trap 'git worktree remove --force "${work}" >/dev/null 2>&1 || true; rm -rf "${work}"' EXIT
git worktree add --detach --quiet "${work}" HEAD

python3 - "${work}/turbovec/Cargo.toml" "${version}" <<'EOF'
import re, sys
path, version = sys.argv[1], sys.argv[2]
s = open(path).read()
def one(pattern, repl):
    global s
    s, n = re.subn(pattern, repl, s, count=1, flags=re.M)
    if n != 1:
        sys.exit(f"publish-pipestream: manifest line not found: {pattern}")
one(r'^name = "turbovec"$', 'name = "pipestream-turbovec"')
one(r'^version = ".*"$', f'version = "{version}"')
one(r'^description = ".*"$',
    'description = "turbovec by Ryan Codrai with ai-pipestream\'s distributed-search '
    'patches (seeded floors, streaming collector, mapped serving); see FORK.md"')
one(r'^repository = ".*"$', 'repository = "https://github.com/ai-pipestream/turbovec"')
one(r'^homepage = ".*"$', 'homepage = "https://github.com/ai-pipestream/turbovec"')
# The self dev-dependency must name the renamed package.
one(r'^turbovec = \{ path = "\.",', 'turbovec = { package = "pipestream-turbovec", path = ".",')
# Keep the library importable as `turbovec`.
one(r'^\[dependencies\]$', '[lib]\nname = "turbovec"\n\n[dependencies]')
open(path, "w").write(s)
EOF

# turbovec-python is never published from here, but it is a workspace member
# whose path dependency names the package, so it must follow the rename.
sed -i 's/^turbovec-core = { package = "turbovec",/turbovec-core = { package = "pipestream-turbovec",/' \
  "${work}/turbovec-python/Cargo.toml"
grep -q 'package = "pipestream-turbovec"' "${work}/turbovec-python/Cargo.toml" || {
  echo "publish-pipestream: turbovec-python's turbovec dependency line moved." >&2
  exit 1
}

# The lockfile names the old package; let cargo refresh it in the worktree.
(cd "${work}" && cargo update -w --quiet)

args=(publish -p pipestream-turbovec --allow-dirty)
[ -n "${REGISTRY}" ] && args+=(--registry "${REGISTRY}")
[ "${PUBLISH}" = 1 ] || args+=(--dry-run)

echo "publish-pipestream: pipestream-turbovec ${version} from ${head} (chain s${chain})"
(cd "${work}" && cargo "${args[@]}")

if [ "${PUBLISH}" = 1 ]; then
  echo "publish-pipestream: published pipestream-turbovec ${version}."
else
  echo "publish-pipestream: dry run passed; re-run with --publish to upload."
fi
