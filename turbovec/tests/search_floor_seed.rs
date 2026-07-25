//! Tests for the seeded top-k threshold
//! ([`SearchOptions::initial_threshold`] via
//! [`TurboQuantIndex::search_with_options`]).
//!
//! The contract under test, from the option's documentation:
//!
//! 1. For any floor that is a true lower bound on the final k-th best
//!    score, results are identical to an unseeded search.
//! 2. For a higher floor, the result set is the unseeded result set
//!    filtered to scores `>= floor`; ties exactly at the floor survive.
//! 3. Rows that come up short are padded to `k` with
//!    `(f32::NEG_INFINITY, -1)` sentinels; unseeded searches never pad.
//!
//! Result-set comparisons treat rows as multisets of (score, index):
//! candidates with equal scores may legitimately sort in a different
//! order between two runs whose heap fill history differs.

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

/// One query row as a sorted multiset of (score bits, index) — bitwise
/// score identity, order-insensitive for equal scores.
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

fn assert_same_results(a: &SearchResults, b: &SearchResults, what: &str) {
    assert_eq!(a.nq, b.nq, "{what}: nq differs");
    assert_eq!(a.k, b.k, "{what}: k differs");
    for qi in 0..a.nq {
        assert_eq!(
            row_multiset(a, qi),
            row_multiset(b, qi),
            "{what}: row {qi} differs",
        );
    }
}

fn build_index(n: usize, dim: usize, bits: usize) -> TurboQuantIndex {
    let mut idx = TurboQuantIndex::new(dim, bits).unwrap();
    idx.add(&unit_vectors(n, dim, 0xF100_4EED));
    idx
}

#[test]
fn floor_at_true_kth_best_is_lossless() {
    let (dim, n, nq, k) = (64, 2000, 6, 10);
    for &bits in &[2usize, 3, 4] {
        let idx = build_index(n, dim, bits);
        let queries = unit_vectors(nq, dim, 0x9E4A_11CE);
        let baseline = idx.search(&queries, k);
        assert_eq!(baseline.k, k);

        // The exact k-th best of every query row is a valid floor for the
        // whole batch only if we take the minimum across rows.
        let batch_floor = (0..nq)
            .map(|qi| baseline.scores_for_query(qi)[k - 1])
            .fold(f32::INFINITY, f32::min);
        let seeded = idx.search_with_options(
            &queries,
            k,
            SearchOptions::new().with_initial_threshold(batch_floor),
        );
        assert_same_results(&baseline, &seeded, &format!("bits={bits} batch floor"));

        // Stronger per-query variant: each query seeded with its own
        // exact k-th best score. Ties at the floor must survive, so this
        // is lossless too. The unseeded baseline here must be computed
        // with the SAME single-query shape: the batched rotation GEMM
        // accumulates in a shape-dependent order, so scores from an nq=6
        // run and an nq=1 run can differ by a few ULPs and are not
        // comparable bit-for-bit.
        for qi in 0..nq {
            let q = &queries[qi * dim..(qi + 1) * dim];
            let single_baseline = idx.search(q, k);
            let per_query_floor = single_baseline.scores_for_query(0)[k - 1];
            let single = idx.search_with_options(
                q,
                k,
                SearchOptions::new().with_initial_threshold(per_query_floor),
            );
            assert_eq!(single.nq, 1);
            assert_eq!(
                row_multiset(&single_baseline, 0),
                row_multiset(&single, 0),
                "bits={bits} qi={qi}: exact k-th floor changed the result",
            );
        }
    }
}

#[test]
fn higher_floor_filters_the_unseeded_results_and_pads() {
    let (dim, n, nq, k) = (64, 1500, 4, 12);
    let idx = build_index(n, dim, 4);
    let queries = unit_vectors(nq, dim, 0x0DD5_EED5);
    let baseline = idx.search(&queries, k);

    // Floor between each row's rank-4 and rank-3 scores would differ per
    // row; the API takes one floor per call, so pick row 0's rank-3 score
    // as the shared floor and derive every row's expectation from the
    // filter contract.
    let floor = baseline.scores_for_query(0)[3];
    let seeded = idx.search_with_options(
        &queries,
        k,
        SearchOptions::new().with_initial_threshold(floor),
    );
    assert_eq!(seeded.k, baseline.k);

    let mut any_padded = false;
    for qi in 0..nq {
        // Expected: baseline row filtered to scores >= floor (ties at the
        // floor survive), padded to k with the sentinel. This derivation
        // is exact in both regimes: when the floor is above a row's k-th
        // best, everything scoring >= floor is necessarily inside that
        // row's top-k; when it is at or below, the filter keeps the whole
        // row and the seeded search is lossless.
        let mut expected: Vec<(u32, i64)> = baseline
            .scores_for_query(qi)
            .iter()
            .zip(baseline.indices_for_query(qi))
            .filter(|(&s, _)| s >= floor)
            .map(|(&s, &i)| (s.to_bits(), i))
            .collect();
        let survivors = expected.len();
        any_padded |= survivors < k;
        expected
            .extend(std::iter::repeat((f32::NEG_INFINITY.to_bits(), -1i64)).take(k - survivors));
        expected.sort_unstable();
        assert_eq!(
            expected,
            row_multiset(&seeded, qi),
            "row {qi}: seeded result is not the floor-filtered baseline",
        );
    }
    // Row 0's floor is its own rank-3 score, so at least row 0 must have
    // dropped candidates — otherwise this test exercised nothing.
    assert!(any_padded, "no row was padded; the floor excluded nothing");
}

#[test]
fn floor_above_every_score_returns_fully_padded_rows() {
    let (dim, n, k) = (64, 800, 7);
    let idx = build_index(n, dim, 4);
    let query = unit_vectors(1, dim, 0xABCD_0123);
    let baseline = idx.search(&query, k);
    let max_score = baseline.scores_for_query(0)[0];

    let seeded = idx.search_with_options(
        &query,
        k,
        SearchOptions::new().with_initial_threshold(max_score + 1.0),
    );
    assert_eq!(seeded.nq, 1);
    assert_eq!(seeded.k, k);
    for (s, i) in seeded
        .scores_for_query(0)
        .iter()
        .zip(seeded.indices_for_query(0))
    {
        assert_eq!(*s, f32::NEG_INFINITY, "padding score sentinel");
        assert_eq!(*i, -1, "padding index sentinel");
    }
}

#[test]
fn no_floor_matches_plain_search_exactly() {
    let (dim, n, nq, k) = (64, 1000, 5, 9);
    let idx = build_index(n, dim, 4);
    let queries = unit_vectors(nq, dim, 0x1234_5678);
    let baseline = idx.search(&queries, k);

    // Default options and an explicit NEG_INFINITY floor are both "no
    // floor": bitwise-identical rows, not just multiset-equal, because
    // the heap fill history is identical.
    let default_opts = idx.search_with_options(&queries, k, SearchOptions::new());
    assert_eq!(baseline.scores, default_opts.scores);
    assert_eq!(baseline.indices, default_opts.indices);

    let neg_inf = idx.search_with_options(
        &queries,
        k,
        SearchOptions::new().with_initial_threshold(f32::NEG_INFINITY),
    );
    assert_eq!(baseline.scores, neg_inf.scores);
    assert_eq!(baseline.indices, neg_inf.indices);
}

#[test]
fn floor_composes_with_mask() {
    let (dim, n, nq, k) = (64, 1200, 3, 8);
    let idx = build_index(n, dim, 4);
    let queries = unit_vectors(nq, dim, 0x0C0A_BB00);
    // Allow only every third slot.
    let mask: Vec<bool> = (0..n).map(|i| i % 3 == 0).collect();
    let baseline = idx.search_with_mask(&queries, k, Some(&mask));

    let batch_floor = (0..nq)
        .map(|qi| baseline.scores_for_query(qi)[k - 1])
        .fold(f32::INFINITY, f32::min);
    let seeded = idx.search_with_options(
        &queries,
        k,
        SearchOptions::new()
            .with_mask(&mask)
            .with_initial_threshold(batch_floor),
    );
    assert_same_results(&baseline, &seeded, "mask + true-floor");

    // Masked-out slots must stay excluded even when they beat the floor:
    // every returned index still satisfies the mask.
    for qi in 0..nq {
        for &i in seeded.indices_for_query(qi) {
            assert!(i >= 0 && mask[i as usize], "slot {i} violates the mask");
        }
    }
}

#[test]
fn ties_exactly_at_the_floor_survive() {
    let dim = 64;
    let bits = 4;
    // A corpus with one vector duplicated three times: identical input
    // vectors quantize to identical codes, so all three copies score
    // identically for every query.
    let corpus = unit_vectors(500, dim, 0x7157_1E5A);
    let dup = corpus[0..dim].to_vec();
    let mut idx = TurboQuantIndex::new(dim, bits).unwrap();
    idx.add(&corpus);
    idx.add(&dup); // slot 500
    idx.add(&dup); // slot 501

    let query = dup.clone();
    let k = 8;
    let baseline = idx.search(&query, k);
    // The three copies (slots 0, 500, 501) share the top score.
    let dup_score = baseline.scores_for_query(0)[0];

    let seeded = idx.search_with_options(
        &query,
        k,
        SearchOptions::new().with_initial_threshold(dup_score),
    );
    let survivors: Vec<i64> = seeded
        .indices_for_query(0)
        .iter()
        .copied()
        .filter(|&i| i >= 0)
        .collect();
    for slot in [0i64, 500, 501] {
        assert!(
            survivors.contains(&slot),
            "duplicate slot {slot} scored exactly at the floor and must survive; got {survivors:?}",
        );
    }
    for &s in seeded.scores_for_query(0) {
        assert!(
            s == f32::NEG_INFINITY || s >= dup_score,
            "score {s} below the floor {dup_score} leaked through",
        );
    }
}

#[test]
fn floor_semantics_hold_across_calibration_blocks() {
    // Every test above runs inside one open block (n < the 8192
    // default), so none reaches the cross-block merge. A small block
    // size forces many sealed blocks, exercising the per-block floor
    // seeding, the sentinel skip in the merge (rebasing a -1 sentinel
    // would mint a fake slot at base - 1), and the row re-pad.
    let (dim, n, nq, k) = (64, 2000, 4, 10);
    for &bits in &[2usize, 4] {
        let mut idx = TurboQuantIndex::with_block_size(dim, bits, 256).unwrap();
        idx.add(&unit_vectors(n, dim, 0xB10C_F100));
        assert!(idx.sealed_blocks() >= 2, "test needs a multi-block index");

        let queries = unit_vectors(nq, dim, 0x9E4A_11CE);
        let baseline = idx.search(&queries, k);
        assert_eq!(baseline.k, k);

        // 1. True lower bound: lossless.
        let batch_floor = (0..nq)
            .map(|qi| baseline.scores_for_query(qi)[k - 1])
            .fold(f32::INFINITY, f32::min);
        let seeded = idx.search_with_options(
            &queries,
            k,
            SearchOptions::new().with_initial_threshold(batch_floor),
        );
        assert_same_results(&baseline, &seeded, &format!("bits={bits} blocked batch floor"));

        // 2. Higher floor: exactly the floor-filtered baseline, padded.
        let floor = baseline.scores_for_query(0)[2];
        let seeded = idx.search_with_options(
            &queries,
            k,
            SearchOptions::new().with_initial_threshold(floor),
        );
        assert_eq!(seeded.k, baseline.k);
        let mut any_padded = false;
        for qi in 0..nq {
            let mut expected: Vec<(u32, i64)> = baseline
                .scores_for_query(qi)
                .iter()
                .zip(baseline.indices_for_query(qi))
                .filter(|(&s, _)| s >= floor)
                .map(|(&s, &i)| (s.to_bits(), i))
                .collect();
            let survivors = expected.len();
            any_padded |= survivors < k;
            expected.extend(
                std::iter::repeat((f32::NEG_INFINITY.to_bits(), -1i64)).take(k - survivors),
            );
            expected.sort_unstable();
            assert_eq!(
                expected,
                row_multiset(&seeded, qi),
                "bits={bits} row {qi}: blocked seeded result is not the filtered baseline",
            );
            // Every real index is a genuine slot: a rebased sentinel
            // would surface as some block's base - 1 with a real score.
            for &i in seeded.indices_for_query(qi) {
                assert!(i == -1 || (0..n as i64).contains(&i), "fake slot {i}");
            }
        }
        assert!(any_padded, "no row was padded; the floor excluded nothing");

        // 3. Floor above every score: fully padded rows, correct width.
        let max_score = (0..nq)
            .map(|qi| baseline.scores_for_query(qi)[0])
            .fold(f32::NEG_INFINITY, f32::max);
        let all_padded = idx.search_with_options(
            &queries,
            k,
            SearchOptions::new().with_initial_threshold(max_score + 1.0),
        );
        assert_eq!(all_padded.k, k);
        for qi in 0..nq {
            for (s, i) in all_padded
                .scores_for_query(qi)
                .iter()
                .zip(all_padded.indices_for_query(qi))
            {
                assert_eq!(*s, f32::NEG_INFINITY, "bits={bits} padding score");
                assert_eq!(*i, -1, "bits={bits} padding index");
            }
        }
    }
}

#[test]
#[should_panic(expected = "initial_threshold must not be NaN")]
fn nan_floor_panics() {
    let idx = build_index(64, 64, 4);
    let query = unit_vectors(1, 64, 1);
    let _ = idx.search_with_options(
        &query,
        4,
        SearchOptions::new().with_initial_threshold(f32::NAN),
    );
}
