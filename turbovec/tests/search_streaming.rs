//! Tests for the streaming collector
//! ([`TurboQuantIndex::search_streaming`]).
//!
//! The contract under test, from the method's documentation:
//!
//! 1. A completed stream emits, per query, exactly the candidates
//!    scoring at or above that query's floor as it stood when the
//!    candidate's chunk was scored, each exactly once, with scores
//!    bitwise identical to a top-k search of the same query batch.
//! 2. `RaiseFloor` takes effect at the next chunk and only for the
//!    batch's query; floors never go down.
//! 3. `Stop` abandons the scan: `completed` is `false` and emissions
//!    so far stand.
//! 4. Masks compose: masked-out slots are never emitted.
//! 5. Scores descend within a batch; a query's `block_base` values
//!    strictly ascend.
//!
//! The public entry point walks 8192-row chunks, so small indexes
//! stream in one batch; the multi-batch contract is exercised through
//! the chunk-size override the implementation exposes for exactly this
//! purpose (the chunk size changes the batch cadence, never the
//! emitted set).

use turbovec::{SearchOptions, StreamControl, TurboQuantIndex};

/// Small emission chunk so a few thousand rows span several batches.
const CHUNK: usize = 256;

fn unit_vectors(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_add(0x9E3779B97F4A7C15);
    let mut out = vec![0.0f32; n * dim];
    for row in out.chunks_mut(dim) {
        let mut norm = 0.0f64;
        for x in row.iter_mut() {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = ((s >> 33) as f64 / (1u64 << 31) as f64) - 1.0;
            *x = v as f32;
            norm += v * v;
        }
        let inv = 1.0 / (norm.sqrt() + 1e-9);
        for x in row.iter_mut() {
            *x = (*x as f64 * inv) as f32;
        }
    }
    out
}

/// All (score bits, slot) pairs of one query row of a `SearchResults`,
/// filtered to score >= floor, as a sorted multiset.
fn baseline_above(
    idx: &TurboQuantIndex,
    queries: &[f32],
    qi: usize,
    floor: f32,
) -> Vec<(u32, i64)> {
    let all = idx.search_with_options(
        queries,
        idx.len(),
        SearchOptions::new().with_initial_threshold(floor),
    );
    let mut v: Vec<(u32, i64)> = all
        .scores_for_query(qi)
        .iter()
        .zip(all.indices_for_query(qi))
        .filter(|(_, &i)| i >= 0)
        .map(|(&s, &i)| (s.to_bits(), i))
        .collect();
    v.sort_unstable();
    v
}

fn plain_index(n: usize, dim: usize, bits: usize) -> TurboQuantIndex {
    let mut idx = TurboQuantIndex::new(dim, bits).unwrap();
    idx.add(&unit_vectors(n, dim, 0xF100_4EED));
    idx
}

#[test]
fn completed_stream_is_exactly_the_above_floor_set() {
    let (dim, n, nq) = (64, 2000, 3);
    for &bits in &[2usize, 4] {
        let idx = plain_index(n, dim, bits);
        assert!(n > 2 * CHUNK, "the corpus must span several chunks");
        let queries = unit_vectors(nq, dim, 0x9E4A_11CE);

        // A floor with some survivors in every regime: query 0's
        // 20th-best score from a plain search.
        let probe = idx.search(&queries, 20);
        let floor = probe.scores_for_query(0)[19];

        let mut got: Vec<Vec<(u32, i64)>> = vec![Vec::new(); nq];
        let mut last_base: Vec<Option<usize>> = vec![None; nq];
        let summary = idx
            .try_search_streaming_chunked(
                &queries,
                SearchOptions::new().with_initial_threshold(floor),
                CHUNK,
                |b| {
                    assert!(!b.scores.is_empty(), "empty batches are never emitted");
                    assert_eq!(b.scores.len(), b.slots.len());
                    for w in b.scores.windows(2) {
                        assert!(w[0] >= w[1], "scores must descend within a batch");
                    }
                    if let Some(prev) = last_base[b.query_index] {
                        assert!(b.block_base > prev, "block_base must ascend per query");
                    }
                    last_base[b.query_index] = Some(b.block_base);
                    for (s, i) in b.scores.iter().zip(b.slots) {
                        assert!(*s >= floor, "score below the floor leaked");
                        got[b.query_index].push((s.to_bits(), *i));
                    }
                    StreamControl::Continue
                },
            )
            .unwrap();
        assert!(summary.completed);
        assert_eq!(summary.nq, nq);
        assert_eq!(summary.emitted, got.iter().map(Vec::len).sum::<usize>());
        assert_eq!(
            summary.blocks_scanned,
            n.div_ceil(CHUNK),
            "every chunk is scored exactly once",
        );
        for (qi, mut row) in got.into_iter().enumerate() {
            row.sort_unstable();
            assert_eq!(
                row,
                baseline_above(&idx, &queries, qi, floor),
                "bits={bits} qi={qi}: stream is not the above-floor set",
            );
        }
    }
}

#[test]
fn no_floor_streams_every_live_slot_once() {
    let (dim, n) = (64, 1500);
    let idx = plain_index(n, dim, 4);
    let query = unit_vectors(1, dim, 0x0DD5_EED5);

    let mut seen = vec![false; n];
    let summary = idx
        .try_search_streaming_chunked(&query, SearchOptions::new(), CHUNK, |b| {
            for &slot in b.slots {
                assert!(!seen[slot as usize], "slot {slot} emitted twice");
                seen[slot as usize] = true;
            }
            StreamControl::Continue
        })
        .unwrap();
    assert!(summary.completed);
    assert_eq!(summary.emitted, n);
    assert!(seen.iter().all(|&s| s), "a live slot was never emitted");
}

#[test]
fn raised_floor_applies_from_the_next_chunk_for_that_query_only() {
    let (dim, n, nq) = (64, 2000, 2);
    let idx = plain_index(n, dim, 4);
    let queries = unit_vectors(nq, dim, 0x0C0A_BB00);

    // Raise query 0's floor after its first batch to its true 5th-best
    // score; query 1 keeps streaming unfloored.
    let probe = idx.search(&queries, 5);
    let raised = probe.scores_for_query(0)[4];

    let mut q0_batches = 0usize;
    let mut q0_after_raise: Vec<(u32, i64)> = Vec::new();
    let mut q0_first_base = None;
    let mut q1_count = 0usize;
    let summary = idx
        .try_search_streaming_chunked(&queries, SearchOptions::new(), CHUNK, |b| {
            if b.query_index == 1 {
                q1_count += b.slots.len();
                return StreamControl::Continue;
            }
            q0_batches += 1;
            if q0_batches == 1 {
                q0_first_base = Some(b.block_base);
                return StreamControl::RaiseFloor(raised);
            }
            for (s, i) in b.scores.iter().zip(b.slots) {
                assert!(
                    *s >= raised,
                    "query 0 emitted {s} below its raised floor {raised}",
                );
                q0_after_raise.push((s.to_bits(), *i));
            }
            StreamControl::Continue
        })
        .unwrap();
    assert!(summary.completed);
    // Query 1 was untouched by query 0's floor: every live slot.
    assert_eq!(q1_count, n);
    // Query 0's post-raise emissions are exactly the above-floor set
    // minus whatever lives in its first-emitted chunk.
    let first_base = q0_first_base.expect("query 0 emitted at least one batch");
    let mut expected: Vec<(u32, i64)> = baseline_above(&idx, &queries, 0, raised)
        .into_iter()
        .filter(|&(_, i)| (i as usize) / CHUNK != first_base / CHUNK)
        .collect();
    expected.sort_unstable();
    q0_after_raise.sort_unstable();
    assert_eq!(
        q0_after_raise, expected,
        "post-raise stream is not the above-floor set of the remaining chunks",
    );
}

#[test]
fn stop_abandons_the_scan() {
    let (dim, n) = (64, 2000);
    let idx = plain_index(n, dim, 4);
    let query = unit_vectors(1, dim, 0x1234_5678);

    let mut batches = 0usize;
    let mut emitted = 0usize;
    let summary = idx
        .try_search_streaming_chunked(&query, SearchOptions::new(), CHUNK, |b| {
            batches += 1;
            emitted += b.slots.len();
            StreamControl::Stop
        })
        .unwrap();
    assert_eq!(batches, 1, "sink must not be called after Stop");
    assert!(!summary.completed);
    assert_eq!(summary.emitted, emitted);
    assert!(summary.emitted < n, "Stop after one chunk cannot cover the index");
    assert_eq!(summary.blocks_scanned, 1);
}

#[test]
fn mask_composes_with_streaming() {
    let (dim, n) = (64, 1200);
    let idx = plain_index(n, dim, 4);
    let query = unit_vectors(1, dim, 0xABCD_0123);
    let mask: Vec<bool> = (0..n).map(|i| i % 3 == 0).collect();

    let mut count = 0usize;
    let summary = idx
        .try_search_streaming_chunked(
            &query,
            SearchOptions::new().with_mask(&mask),
            CHUNK,
            |b| {
                for &slot in b.slots {
                    assert!(mask[slot as usize], "slot {slot} violates the mask");
                }
                count += b.slots.len();
                StreamControl::Continue
            },
        )
        .unwrap();
    assert!(summary.completed);
    assert_eq!(count, mask.iter().filter(|&&m| m).count());
}

#[test]
fn empty_and_lazy_indexes_complete_trivially() {
    let dim = 64;
    let idx = TurboQuantIndex::new(dim, 4).unwrap();
    let query = unit_vectors(1, dim, 1);
    let summary = idx.search_streaming(&query, SearchOptions::new(), |_| {
        panic!("sink must not be called on an empty index")
    });
    assert!(summary.completed);
    assert_eq!(summary.emitted, 0);
    assert_eq!(summary.blocks_scanned, 0);
}

#[test]
fn small_index_streams_in_one_default_chunk() {
    // Under the default 8192-row chunk the whole index is one batch per
    // query, through the public entry point.
    let (dim, n) = (64, 900);
    let idx = plain_index(n, dim, 4);
    let query = unit_vectors(1, dim, 0x7157_1E5A);

    let probe = idx.search(&query, 10);
    let floor = probe.scores_for_query(0)[9];

    let mut batches = 0usize;
    let mut got: Vec<(u32, i64)> = Vec::new();
    let summary = idx.search_streaming(
        &query,
        SearchOptions::new().with_initial_threshold(floor),
        |b| {
            batches += 1;
            for (s, i) in b.scores.iter().zip(b.slots) {
                got.push((s.to_bits(), *i));
            }
            StreamControl::Continue
        },
    );
    assert!(summary.completed);
    assert_eq!(batches, 1);
    got.sort_unstable();
    assert_eq!(got, baseline_above(&idx, &query, 0, floor));
}

#[test]
#[should_panic(expected = "raised floor must not be NaN")]
fn nan_raise_panics() {
    let idx = plain_index(500, 64, 4);
    let query = unit_vectors(1, 64, 1);
    let _ = idx.search_streaming(&query, SearchOptions::new(), |_| {
        StreamControl::RaiseFloor(f32::NAN)
    });
}
