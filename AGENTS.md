# Keeping this fork current

This repository is ai-pipestream's patch fork of
[RyanCodrai/turbovec](https://github.com/RyanCodrai/turbovec). It is Ryan's
work with a small patch stack on top that lets shards of one index
collaborate on a query. It is published as the `pipestream-turbovec` crate
until those patches are accepted upstream. Read [FORK.md](FORK.md) first. It
says what each patch does and why, and it names the current chain branch.

An agent checks this fork every other day. Each check runs the steps below in
order and stops at the first one that has nothing to do.

## 1. Is upstream ahead?

```
git fetch origin --prune     # origin = RyanCodrai/turbovec
git fetch fork --prune       # fork   = ai-pipestream/turbovec
git log --oneline fork/main..origin/main
```

If that prints nothing, the fork is current, and the check ends here. Report
"up to date at <short sha>".

## 2. Carry the patches onto upstream

```
scripts/sync-upstream.sh            # rebase + `cargo test -p turbovec`
```

The script creates the next chain branch, `turbovec-pipestream-s<N+1>`,
from `main`, which holds the chain tip plus fork-only commits such as this
file. It rebases that stack onto upstream main, renames the branch in FORK.md,
and runs the tests as the gate. On a conflict or a red test it stops and
names the recurring seams. Every new heap-init site in the search kernels
must seed the floor instead of `NEG_INFINITY`, and `search_streaming` must
ride the same chunk loop as the top-k scan.

- Read the new upstream commits before resolving anything. When upstream
  now does what a patch did, drop the patch and say so in FORK.md, as was
  done for shared calibration (upstream #474).
- The patches must not change single-index behavior when unused. Results
  stay bitwise identical to upstream for an unseeded, non-streaming search.
- Update FORK.md whenever a patch changes shape.

When the tests pass, push the new branch with `scripts/sync-upstream.sh
--push`, or `git push fork turbovec-pipestream-s<N+1>`.

## 3. Move main

`main` carries the same stack as the newest chain branch. Point it there:

```
git push --force-with-lease=main:<old fork/main sha> fork turbovec-pipestream-s<N+1>:main
```

Never force-push a `turbovec-pipestream-sN` branch. They are immutable, and
older consumers may still pin one.

## 4. Publish `pipestream-turbovec`

```
scripts/publish-pipestream.sh             # dry run: package and build
scripts/publish-pipestream.sh --publish   # upload
```

The source keeps upstream's package name, so rebases never touch
`Cargo.toml` or CI. The script renames the crate only at publish time, to
`pipestream-turbovec` with version `<upstream version>-pipestream.<N>`.
The library name stays `turbovec`. Each chain number publishes exactly
once, because a registry never lets a version be overwritten. If a fix is
needed after publishing, it goes on a new chain branch.

## 5. Bump the consumers

| Repository | What to change |
|---|---|
| ai-pipestream/protomolt-search | `Cargo.toml` turbovec line, then `cargo update -p pipestream-turbovec` (or `-p turbovec` while it is still a git dependency) |
| ai-pipestream/turbovec-grpc | the same, in lockstep with protomolt-search; two different turbovec versions in one build cause type mismatches |

Consumers depend on the crate as
`turbovec = { package = "pipestream-turbovec", version = "=<version>" }`,
so `use turbovec::...` keeps working.

## Rules

- Push to Forgejo (`git.rokkon.com/ai-pipestream/<repo>`) first, then GitHub.
  Afterwards, check that both hosts show the same SHA for every branch you
  pushed.
- Cloud sessions cannot reach Forgejo, so do those pushes from krick.
- Report what changed: the upstream commits taken, the patches carried or
  dropped, the version published, and the consumer bumps made.
