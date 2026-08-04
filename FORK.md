# About this fork

This is ai-pipestream's patch fork of
[RyanCodrai/turbovec](https://github.com/RyanCodrai/turbovec). The
patches open up the ability to shard one index across many machines and
have those shards collaborate on a query — the pieces turbovec needs to
scale horizontally without changing what it computes. Both patches exist
for that collaboration; neither alters single-index behavior when unused.
The `turbovec-pipestream-s11` branch carries them rebased onto
upstream `main` (`scripts/sync-upstream.sh`); each sync publishes a
new `-sN` branch because the rebase rewrites history, and
[turbovec-search](https://github.com/ai-pipestream/turbovec-search) is
the distributed engine built on top.

## The patches

**Shared calibration is now upstream.** A quantized score is only
comparable across separately built indexes if every index encodes
vectors identically. The fork used to carry `new_with_calibration` for
this — constructing an index from externally supplied TQ+ parameters
instead of letting it fit its own from the first batch. Upstream #474
("TQ+ becomes an explicit, user-supplied calibration") removed the
automatic fit entirely and made calibration a caller-supplied act:
`calibrate(sample)` commits a pair fitted deterministically from the
caller's sample, and `from_parts` reconstructs an index under an
explicit pair. Fit once, apply the same pair (or the same sample) to
every shard, and the same vector produces byte-identical codes
everywhere — the property the patch existed for, now stock. The fork
therefore no longer patches calibration at all.

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

**Streaming collector.** `search_streaming` emits every candidate at
or above a live per-query floor to a caller-supplied sink, chunk by
chunk, with no shard-local top-k and no heap: the sink may raise the
floor between chunks (`RaiseFloor`, floors only rise) or abandon the
scan (`Stop`), and a completed scan is the exactness certificate that
nothing at or above the floor was withheld. Each chunk goes through
the same kernel as a top-k search with the floor seeded, so emitted
scores are bitwise identical to a top-k of the same query batch. This
is the collector for a coordinator that owns k itself and relays the
merged k-th best back as the floor while several shards scan in
tandem.

## How they are used together

A coordinator fans a query out to N shard indexes, all calibrated
under one shared pair (upstream's explicit `calibrate`). In top-k
mode, each shard reports its k-th-best score upstream as its heap
fills; the coordinator tracks the maximum across shards and pushes it
back down, and every shard seeds its scan with that floor via
`initial_threshold`. In streaming mode, the coordinator owns k
outright: shards run `search_streaming` and emit above the relayed
floor, and per-shard top-k disappears entirely. Either way shards
prune against the best global knowledge available mid-query instead of
only their own progress, and the merged result — same calibration,
same score space — equals what one monolithic index would have
returned, exactly.

The same two primitives also make shards reorganizable offline: because
calibration is explicit and portable, a shard set can be split, merged,
or redistributed and rebuilt with the identical encoding, and the
results remain byte-for-byte consistent with the original.

## Repository map

| Repository | Role | Depends on |
|---|---|---|
| [RyanCodrai/turbovec](https://github.com/RyanCodrai/turbovec) | Upstream vector index library: 4-bit TurboQuant encoding, SIMD top-k search | — |
| [ai-pipestream/turbovec](https://github.com/ai-pipestream/turbovec), branch `turbovec-pipestream-s11` (this repo) | Patch fork carrying the patches above | upstream `main` |
| [ai-pipestream/turbovec-grpc](https://github.com/ai-pipestream/turbovec-grpc) | Standalone single-node gRPC server for the upstream index, with client examples in Go, Java, Python, TypeScript, and Rust | upstream `turbovec` |
| [ai-pipestream/turbovec-search](https://github.com/ai-pipestream/turbovec-search) | Distributed hybrid search: sharded vector + BM25 nodes, coordinator with floor sharing, write-ahead log, offline resharding | fork branch `turbovec-pipestream-s11` |
| [ai-pipestream/grpc-opennlp-analysis](https://github.com/ai-pipestream/grpc-opennlp-analysis) | Text-analysis sidecar: sentence/token spans, term vectors, static embeddings, served over gRPC | — |
