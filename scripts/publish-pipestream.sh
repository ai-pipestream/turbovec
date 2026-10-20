#!/usr/bin/env bash
# Publish this fork's turbovec crate to crates.io as `pipestream-turbovec`.
#
# The source tree keeps upstream's package name (`turbovec`) so the
# patch chain rebases onto RyanCodrai/turbovec without a new conflict
# seam and git consumers (protomolt-search, turbovec-grpc) are
# unaffected. This script stages a copy of the crate, rewrites only its
# manifest, and publishes that copy:
#
#   name        turbovec -> pipestream-turbovec
#   [lib] name  turbovec  (so `use turbovec::...` keeps working)
#   version     <upstream version>-pipestream.<K>
#   repository  the ai-pipestream fork; description credits upstream
#
# K starts at 1 for each upstream version and rises by one per publish.
# Every publish is recorded by an annotated tag
# `pipestream-turbovec-v<version>` on the published commit, pushed to
# forgejo first and then fork. The tag name does not start with `v`, so
# upstream's release-crates.yml (tags `v*`) never fires on it.
#
# Usage: scripts/publish-pipestream.sh [--check | --dry-run | --publish]
#   --check     report whether HEAD's crate content differs from the last
#               published tag; exit 0 = up to date, 3 = a publish is due
#   --dry-run   (default) stage and run `cargo publish --dry-run`
#   --publish   stage, publish to crates.io, then tag and push the tag.
#               Needs a crates.io token (`cargo login` or
#               CARGO_REGISTRY_TOKEN). A published version is permanent.
#
# Publish only from a clean checkout of fork `main` whose tests pass.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

MODE=dry-run
case "${1:-}" in
  ""|--dry-run) MODE=dry-run ;;
  --check) MODE=check ;;
  --publish) MODE=publish ;;
  *) echo "publish-pipestream: unknown argument '$1'" >&2; exit 1 ;;
esac

CRATE=pipestream-turbovec
# What ends up in the published crate. A change anywhere else (FORK.md,
# CI, benchmarks, the Python bindings) does not need a new version.
CONTENT=(turbovec README.md LICENSE)

upstream_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' turbovec/Cargo.toml | head -1)
if [ -z "${upstream_version}" ]; then
  echo "publish-pipestream: no version in turbovec/Cargo.toml" >&2
  exit 1
fi

git fetch --quiet --tags fork
last_tag=$(git tag --list "${CRATE}-v*" --sort=-v:refname | head -1)

if [ -n "${last_tag}" ] && git diff --quiet "${last_tag}" HEAD -- "${CONTENT[@]}"; then
  echo "publish-pipestream: HEAD matches ${last_tag}; nothing to publish."
  exit 0
fi
if [ "${MODE}" = check ]; then
  echo "publish-pipestream: crate content changed since ${last_tag:-<never published>}; a publish is due."
  exit 3
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "publish-pipestream: working tree is dirty; commit first." >&2
  exit 1
fi

# Next K for this upstream version.
last_k=$(git tag --list "${CRATE}-v${upstream_version}-pipestream.*" \
  | sed "s/^${CRATE}-v${upstream_version}-pipestream\.//" | sort -n | tail -1)
version="${upstream_version}-pipestream.$(( ${last_k:-0} + 1 ))"
tag="${CRATE}-v${version}"
commit=$(git rev-parse HEAD)

stage=$(mktemp -d)
trap 'rm -rf "${stage}"' EXIT
mkdir -p "${stage}/crate"
cp -R turbovec/. "${stage}/crate/"
cp README.md LICENSE "${stage}/"

manifest="${stage}/crate/Cargo.toml"
# Exact-line rewrites; each must match once or the manifest moved and a
# human has to look.
rewrite() {
  local from="$1" to="$2"
  if [ "$(grep -cxF -- "${from}" "${manifest}")" != 1 ]; then
    echo "publish-pipestream: expected exactly one line '${from}' in turbovec/Cargo.toml" >&2
    exit 1
  fi
  local tmp="${manifest}.tmp"
  awk -v from="${from}" -v to="${to}" '$0 == from { print to; next } { print }' "${manifest}" > "${tmp}"
  mv "${tmp}" "${manifest}"
}
rewrite 'name = "turbovec"' "name = \"${CRATE}\""
rewrite "version = \"${upstream_version}\"" "version = \"${version}\""
rewrite 'description = "Fast vector quantization with 2-4 bit compression and SIMD search"' \
  "description = \"RyanCodrai/turbovec ${upstream_version} with ai-pipestream's distributed-search patches (seedable floor, streaming collector, mapped serving)\""
rewrite 'repository = "https://github.com/RyanCodrai/turbovec"' \
  'repository = "https://github.com/ai-pipestream/turbovec"'
# The self dev-dependency only enables a test feature; published crates
# drop path-only dev-dependencies, but it must name the renamed package.
rewrite 'turbovec = { path = ".", features = ["mask-skip-counter"] }' \
  "turbovec = { package = \"${CRATE}\", path = \".\", features = [\"mask-skip-counter\"] }"
cat >> "${manifest}" <<EOF

[lib]
name = "turbovec"

[package.metadata.pipestream]
upstream = "https://github.com/RyanCodrai/turbovec"
source-commit = "${commit}"
EOF
# A one-member workspace so cargo does not climb into a parent one.
cat > "${stage}/Cargo.toml" <<'EOF'
[workspace]
members = ["crate"]
resolver = "2"
EOF

echo "publish-pipestream: ${CRATE} ${version} from ${commit}"
cd "${stage}"
if [ "${MODE}" = dry-run ]; then
  cargo publish -p "${CRATE}" --dry-run --allow-dirty
  echo "publish-pipestream: dry run passed. Publish with --publish."
  exit 0
fi

cargo publish -p "${CRATE}" --allow-dirty
cd - > /dev/null
git tag -a "${tag}" -m "${CRATE} ${version} (upstream turbovec ${upstream_version})" "${commit}"
git push forgejo "${tag}"
git push fork "${tag}"
echo "publish-pipestream: published ${CRATE} ${version}; tagged ${tag} on forgejo and fork."
