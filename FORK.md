# About this fork

This is ai-pipestream's patch fork of
[RyanCodrai/turbovec](https://github.com/RyanCodrai/turbovec). The
patches open up the ability to shard one index across many machines and
have those shards collaborate on a query — the pieces turbovec needs to
scale horizontally without changing what it computes. Both changes exist
for that collaboration; neither alters single-index behavior when unused.
The `turbovec-pipestream` branch carries them rebased onto upstream
`main` (`scripts/sync-upstream.sh`), and
[turbovec-search](https://github.com/ai-pipestream/turbovec-search) is
the distributed engine built on top.

## The two patches

**Seeded calibration.** A quantized score is only comparable across
separately built indexes if every index encodes vectors identically.
`new_with_calibration` constructs an index from externally supplied
TQ+ parameters instead of fitting them from its own first batch, and
`calibration()` reads them back. Fit once on a corpus sample, construct
every shard with the result, and the same vector produces byte-identical
codes everywhere — so per-shard top-k lists merge into an exact global
top-k rather than an approximation. The calibration locks for the
index's lifetime; a seeded index also skips the warm-up refit that would
otherwise silently replace the seed.

**Seedable search floor.** `SearchOptions::initial_threshold` lets a
caller start a top-k scan with a score floor already in place, rather
than pruning only after the local heap fills. On its own an index gains
little from this — the value appears when shards search together: one
shard's current k-th-best score is a valid floor for every other shard
(the global k-th best can only be higher), so a late-starting or
slow-scanning shard can skip work another shard has already ruled out.
The floor is exact, never heuristic: seeding it can only skip vectors
that provably cannot reach the top k, so results are bitwise identical
to an unseeded scan.

## How they are used together

A coordinator fans a query out to N shard indexes, all built with one
shared calibration. Each shard scans in chunks; once its heap holds k
candidates it reports its k-th-best score upstream. The coordinator
tracks the maximum across shards and pushes it back down, and every
shard seeds its next chunk's scan with that floor via
`initial_threshold`. Shards therefore prune against the best global
knowledge available mid-query instead of only their own progress. When
all shards finish, the coordinator merges the per-shard lists directly —
same calibration, same score space — and the merged top-k equals what
one monolithic index would have returned, exactly.

The same two primitives also make shards reorganizable offline: because
calibration is explicit and portable, a shard set can be split, merged,
or redistributed and rebuilt with the identical encoding, and the
results remain byte-for-byte consistent with the original.

## Repository map

| Repository | Role | Depends on |
|---|---|---|
| [RyanCodrai/turbovec](https://github.com/RyanCodrai/turbovec) | Upstream vector index library: 4-bit TurboQuant encoding, SIMD top-k search | — |
| [ai-pipestream/turbovec](https://github.com/ai-pipestream/turbovec), branch `turbovec-pipestream` (this repo) | Patch fork carrying the two changes above | upstream `main` |
| [ai-pipestream/turbovec-grpc](https://github.com/ai-pipestream/turbovec-grpc) | Standalone single-node gRPC server for the upstream index, with client examples in Go, Java, Python, TypeScript, and Rust | upstream `turbovec` |
| [ai-pipestream/turbovec-search](https://github.com/ai-pipestream/turbovec-search) | Distributed hybrid search: sharded vector + BM25 nodes, coordinator with floor sharing, write-ahead log, offline resharding | fork branch `turbovec-pipestream` |
| [ai-pipestream/grpc-opennlp-analysis](https://github.com/ai-pipestream/grpc-opennlp-analysis) | Text-analysis sidecar: sentence/token spans, term vectors, static embeddings, served over gRPC | — |
