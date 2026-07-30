//! Tests for the shared floor between ranges of the single-query
//! parallel scan paths.
//!
//! The contract: ranges scanning one query concurrently share their
//! running k-th-best through an atomic floor. Sharing only raises each
//! range's pruning floor to values that are true lower bounds on the
//! final k-th best, so the returned top-k must be IDENTICAL — same
//! multiset of (score, index) per row — to any other search path over
//! the same index, on every run regardless of thread interleaving.
//!
//! The reference is the multi-query batch path: searching `[q, q]`
//! takes the batch kernels (nq > 1), while searching `q` alone over a
//! >=256-block index takes the single-query parallel path under test.

use turbovec::{SearchOptions, SearchResults, TurboQuantIndex};

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

fn row_multiset(r: &SearchResults, qi: usize) -> Vec<(u32, i64)> {
    let mut row: Vec<(u32, i64)> = r
        .scores_for_query(qi)
        .iter()
        .zip(r.indices_for_query(qi))
        .map(|(&s, &i)| (s.to_bits(), i))
        .collect();
    row.sort_unstable();
    row
}

/// 40k vectors = 1250 blocks, well past the >=256-block gate for the
/// single-query parallel path.
const N: usize = 40_000;
const DIM: usize = 64;

fn build() -> (TurboQuantIndex, Vec<f32>) {
    let corpus = unit_vectors(N, DIM, 0xF10011);
    let mut index = TurboQuantIndex::new(DIM, 4).unwrap();
    index.add(&corpus);
    index.prepare();
    let query = unit_vectors(1, DIM, 0x51E5EED)[..DIM].to_vec();
    (index, query)
}

#[test]
fn parallel_single_query_matches_batch_path_at_every_k() {
    let (index, query) = build();
    let pair: Vec<f32> = [query.clone(), query.clone()].concat();
    for k in [1usize, 7, 64, 500, 2000] {
        let single = index.search(&query, k);
        let batch = index.search(&pair, k);
        assert_eq!(single.k, batch.k, "k={k}: effective k differs");
        assert_eq!(
            row_multiset(&single, 0),
            row_multiset(&batch, 0),
            "k={k}: single-query parallel path differs from batch path"
        );
    }
}

#[test]
fn parallel_single_query_is_deterministic_across_runs() {
    let (index, query) = build();
    for k in [10usize, 500] {
        let first = index.search(&query, k);
        for run in 1..10 {
            let again = index.search(&query, k);
            assert_eq!(
                row_multiset(&first, 0),
                row_multiset(&again, 0),
                "k={k} run {run}: thread interleaving changed the result"
            );
        }
    }
}

#[test]
fn shared_floor_composes_with_caller_floor_and_mask() {
    let (index, query) = build();
    let k = 100;
    let unseeded = index.search(&query, k);
    // A true lower bound on the k-th best must not change results.
    let kth = unseeded.scores_for_query(0)[unseeded.k - 1];
    let mut options = SearchOptions::new();
    options.initial_threshold = Some(kth);
    let seeded = index.search_with_options(&query, k, options);
    assert_eq!(
        row_multiset(&unseeded, 0),
        row_multiset(&seeded, 0),
        "seeding the true k-th best changed results"
    );

    // Masked: even slots only; parallel path vs batch path.
    let mask: Vec<bool> = (0..N).map(|i| i % 2 == 0).collect();
    let single = index.search_with_mask(&query, k, Some(&mask));
    let pair: Vec<f32> = [query.clone(), query.clone()].concat();
    let batch = index.search_with_mask(&pair, k, Some(&mask));
    assert_eq!(
        row_multiset(&single, 0),
        row_multiset(&batch, 0),
        "masked single-query parallel path differs from batch path"
    );
    for &i in single.indices_for_query(0) {
        assert!(i >= 0 && i % 2 == 0, "masked-out slot {i} returned");
    }
}
