# AGENTS.md

Guidance for coding agents in ai-pipestream's turbovec fork. Read
`FORK.md` first: it names the patches and why they exist. Upstream
(RyanCodrai/turbovec) owns everything else; do not edit upstream code
here except as part of a patch FORK.md describes.

## Remotes

| Remote | Repository | Role |
|---|---|---|
| `origin` | github.com/RyanCodrai/turbovec | upstream, read-only |
| `forgejo` | git.rokkon.com/ai-pipestream/turbovec | fork mirror, push first |
| `fork` | github.com/ai-pipestream/turbovec | fork, push second |

Run `git remote -v` first; a fresh clone names these differently.

## The published crate

The fork ships on crates.io as
[`pipestream-turbovec`](https://crates.io/crates/pipestream-turbovec):
upstream turbovec with the fork's patches. Its library name is still
`turbovec`, so downstream code keeps `use turbovec::...` and depends on
it with:

```toml
turbovec = { package = "pipestream-turbovec", version = "=1.1.2-pipestream.1" }
```

The source tree keeps upstream's package name. Only
`scripts/publish-pipestream.sh` renames, in a staged copy, so the
rename never conflicts with an upstream rebase. Versions are
`<upstream version>-pipestream.<K>`, with K restarting at 1 for each
upstream version. Each publish is tagged
`pipestream-turbovec-v<version>` on the published commit. A published
version is permanent: never reuse a number, and never publish a commit
whose tests have not passed.

## Upkeep routine (every other day)

Do these in order and stop at the first failure. Report what each step
found and changed.

1. **Fetch.** `git fetch origin && git fetch fork --tags && git fetch forgejo --tags`.
2. **Upstream drift.** If `git merge-base --is-ancestor origin/main fork/main`
   succeeds, upstream has nothing new; go to step 4.
3. **Rebase the patch chain.** Run `scripts/sync-upstream.sh` on a clean
   checkout of fork `main`. It creates the next immutable
   `turbovec-pipestream-sN` branch, rebases it onto upstream, and runs
   `cargo test -p turbovec`. Conflicts or test failures mean a human
   must look. Report them and stop. On success:
   - Push the new `sN` branch to `forgejo`, then `fork`.
   - Move `main` to the new tip on both remotes with
     `git push --force-with-lease=main:<old main sha> <remote> <new sha>:refs/heads/main`,
     forgejo first. This is the only allowed rewrite of `main`. Never
     force-push an `sN` branch.
4. **Mirror.** Every branch head and every `pipestream-turbovec-v*` tag
   on `fork` must match `forgejo`. Compare with `git ls-remote --heads`
   and `--tags`. Fast-forward whichever side is behind. If the two have
   diverged, report it and stop.
5. **Publish.** On a clean checkout of `main`, run
   `scripts/publish-pipestream.sh --check`.
   - Exit 0: the latest crate already matches; nothing to do.
   - Exit 3: run `cargo test -p turbovec --release --locked`, then
     `scripts/publish-pipestream.sh --dry-run`, then `--publish`. The
     last needs a crates.io token (`cargo login` or `CARGO_REGISTRY_TOKEN`).
     Without one, report the dry run and stop. The script tags the commit
     and pushes the tag to both remotes.
6. **Consumers.** `protomolt-search` and `turbovec-grpc` take turbovec
   from this fork's git `main`. Their `Cargo.lock` should pin the
   current `fork/main` tip. If not, propose `cargo update -p turbovec`
   plus their full test gate as a branch in each repository. Do not
   push their `main`.

## Rules

- Do not add AI attribution or co-author trailers to commits.
- Upstream's `release-crates.yml` publishes `turbovec` on `v*` tags.
  Never create a `v*` tag in the fork.
- Do not bulk-format files; upstream carries its own formatting.
- `rand_chacha` and `statrs` are exact pins that decide encoded bytes.
  A change to either is an index compatibility event, not a routine bump.
