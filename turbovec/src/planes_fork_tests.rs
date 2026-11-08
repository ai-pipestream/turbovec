//! The fork's patches (seeded floor, streaming collector, stored rows)
//! over the planes layout (`pack::planes_for`), against the classic one.
//!
//! Kept apart from `planes_tests` so upstream's file syncs untouched. Same
//! thread-local switch: the layout is opt-in through the environment.

use crate::{pack, SearchOptions, StreamControl, TurboQuantIndex};

const DIM: usize = 64;

struct PlanesOn(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>);
impl PlanesOn {
    fn new(min_vectors: usize) -> Self {
        let gate = crate::search::SCALAR_FALLBACK_GATE.read().unwrap_or_else(|e| e.into_inner());
        pack::PLANES_TEST.with(|c| c.set(Some((true, min_vectors))));
        PlanesOn(gate)
    }
}
impl Drop for PlanesOn {
    fn drop(&mut self) {
        pack::PLANES_TEST.with(|c| c.set(None));
    }
}

fn classic<R>(f: impl FnOnce() -> R) -> R {
    let prev = pack::PLANES_TEST.with(|c| c.replace(Some((false, usize::MAX))));
    let r = f();
    pack::PLANES_TEST.with(|c| c.set(prev));
    r
}

fn planes_supported() -> bool {
    planes_supported_at(2)
}

fn planes_supported_at(bits: usize) -> bool {
    let _on = PlanesOn::new(0);
    let ok = pack::planes_for(bits, DIM * bits / 8);
    if !ok {
        eprintln!("planes layout unsupported on this host; skipping");
    }
    ok
}

fn unit_vectors(n: usize, seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut out = vec![0.0f32; n * DIM];
    for row in out.chunks_mut(DIM) {
        let mut norm = 0.0f64;
        for x in row.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let v = ((s >> 33) as f64 / (1u64 << 30) as f64) - 1.0;
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

fn build(data: &[f32]) -> TurboQuantIndex {
    build_bits(data, 2)
}

fn build_bits(data: &[f32], bits: usize) -> TurboQuantIndex {
    let mut ix = TurboQuantIndex::new(DIM, bits).unwrap();
    ix.add(data);
    let _ = ix.search(&data[..DIM], 1);
    ix
}

fn is_planes(ix: &TurboQuantIndex) -> bool {
    ix.blocked.get().is_some_and(|c| c.is_planes())
}

/// (slot, score bits) per query row, padding included.
fn seeded(ix: &TurboQuantIndex, q: &[f32], k: usize, floor: f32) -> Vec<Vec<(i64, u32)>> {
    let r = ix.search_with_options(
        q,
        k,
        SearchOptions::new().with_initial_threshold(floor),
    );
    (0..r.nq)
        .map(|qi| {
            (0..r.k)
                .map(|j| (r.indices[qi * r.k + j], r.scores[qi * r.k + j].to_bits()))
                .collect()
        })
        .collect()
}

#[test]
fn a_seeded_planes_search_equals_the_seeded_classic_scan() {
    if !planes_supported() {
        return;
    }
    // 100 vectors < the 128-candidate shortlist, so the planes result is
    // the exact scan's and a floor must cut both at the same place,
    // padding rows with (NEG_INFINITY, -1) the same way.
    let data = unit_vectors(100, 11);
    let q = unit_vectors(7, 12);
    let base = classic(|| build(&data));
    let _on = PlanesOn::new(0);
    let ix = build(&data);
    assert!(is_planes(&ix) && !is_planes(&base));
    let k = 20;
    let unseeded = base.search(&q, k);
    for qi in 0..7 {
        let one = &q[qi * DIM..(qi + 1) * DIM];
        // A floor exactly at the 10th best keeps it (ties survive) and
        // pads the rest of the row.
        let floor = unseeded.scores[qi * k + 9];
        let got = seeded(&ix, one, k, floor);
        assert_eq!(got, seeded(&base, one, k, floor), "q={qi}");
        assert!(got[0][9].0 >= 0 && f32::from_bits(got[0][9].1) == floor);
        assert!(got[0][10..].iter().all(|&(i, s)| i == -1 && f32::from_bits(s) == f32::NEG_INFINITY));
    }
    // Unseeded is unchanged by the floor plumbing.
    assert_eq!(
        seeded(&ix, &q, k, f32::NEG_INFINITY),
        seeded(&base, &q, k, f32::NEG_INFINITY)
    );
}

fn streamed(ix: &TurboQuantIndex, q: &[f32], floor: f32, chunk_rows: usize) -> Vec<Vec<(i64, u32)>> {
    let nq = q.len() / DIM;
    let mut out = vec![Vec::new(); nq];
    let summary = ix
        .try_search_streaming_chunked(
            q,
            SearchOptions::new().with_initial_threshold(floor),
            chunk_rows,
            |b| {
                for (&s, &i) in b.scores.iter().zip(b.slots) {
                    out[b.query_index].push((i, s.to_bits()));
                }
                StreamControl::Continue
            },
        )
        .unwrap();
    assert!(summary.completed);
    for row in &mut out {
        row.sort_unstable();
    }
    out
}

#[test]
fn streaming_over_a_planes_cache_emits_the_classic_candidates() {
    if !planes_supported() {
        return;
    }
    let n = 5_000;
    let data = unit_vectors(n, 21);
    let q = unit_vectors(3, 22);
    let base = classic(|| build(&data));
    let _on = PlanesOn::new(0);
    let ix = build(&data);
    assert!(is_planes(&ix));
    let top = base.search(&q, 50);
    let floor = (0..3).map(|qi| top.scores[qi * 50 + 49]).fold(f32::INFINITY, f32::min);
    let want = streamed(&base, &q, floor, 256);
    assert!(want.iter().all(|r| r.len() >= 50));
    assert_eq!(streamed(&ix, &q, floor, 256), want);
}

#[test]
fn stored_rows_read_a_planes_cache_like_a_classic_one() {
    stored_rows_planes_vs_classic(2);
}

#[test]
fn stored_rows_read_a_4bit_planes_cache_like_a_classic_one() {
    stored_rows_planes_vs_classic(4);
}

fn stored_rows_planes_vs_classic(bits: usize) {
    if !planes_supported_at(bits) {
        return;
    }
    let n = 1_000;
    let data = unit_vectors(n, 31);
    let base = classic(|| build_bits(&data, bits));
    let _on = PlanesOn::new(0);
    // Built from the image bytes, so neither index keeps packed rows and
    // both read from their blocked cache.
    let ix = TurboQuantIndex::from_bytes(&base.to_bytes()).unwrap();
    let _ = ix.search(&data[..DIM], 1);
    assert!(is_planes(&ix));
    let base = classic(|| {
        let b = TurboQuantIndex::from_bytes(&base.to_bytes()).unwrap();
        let _ = b.search(&data[..DIM], 1);
        b
    });
    let (mut c0, mut s0, mut c1, mut s1) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for range in [0..n, 5..37, 31..33, 960..n, 0..1] {
        ix.stored_rows(range.clone(), &mut c0, &mut s0).unwrap();
        base.stored_rows(range.clone(), &mut c1, &mut s1).unwrap();
        assert_eq!(c0, c1, "codes {range:?}");
        assert_eq!(s0, s1, "scales {range:?}");
    }
}

/// A bit-plane (v8 planes layout) file written by this build serves through
/// `load_mapped`: every row reads back as a classic load's, and a mapped
/// scan (classic kernels over assembled chunks) scores bit for bit what a
/// classic-layout load of the same file scores. Covers a partial tail
/// block and pending redo ops from an incremental sync.
fn a_planes_layout_file_serves_mapped(bits: usize, stock_threshold: bool) {
    if !planes_supported_at(bits) {
        return;
    }
    let n = if stock_threshold { pack::PLANES_MIN_VECTORS + 7_239 } else { 1_000 + 7 };
    let data = unit_vectors(n, 47 + bits as u64);
    let q = unit_vectors(4, 48 + bits as u64);
    let _on = if stock_threshold { None } else { Some(PlanesOn::new(0)) };
    let mut ix = TurboQuantIndex::new(DIM, bits).unwrap();
    ix.calibrate(&data[..(2048 * DIM).min(data.len())]).unwrap();
    ix.add(&data);
    let _ = ix.search(&data[..DIM], 1);
    assert!(is_planes(&ix), "bits={bits}");
    let path = std::env::temp_dir().join(format!(
        "turbovec-fork-planes-mapped-{}-{bits}-{n}.tv",
        std::process::id()
    ));
    ix.sync(&path).unwrap();
    // A few removals land as redo ops in the next commit header.
    for v in [3usize, 40, 41, 700] {
        ix.swap_remove(v);
    }
    ix.sync(&path).unwrap();
    let n = ix.len();

    let mapped = unsafe { TurboQuantIndex::load_mapped(&path) }.unwrap();
    let base = classic(|| {
        let b = TurboQuantIndex::load(&path).unwrap();
        let _ = b.search(&data[..DIM], 1);
        b
    });
    assert!(!is_planes(&base));
    assert_eq!(mapped.len(), n);

    let (mut c0, mut s0, mut c1, mut s1) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for range in [0..n, 0..1, 31..33, 640..710, n - 7..n] {
        mapped.stored_rows(range.clone(), &mut c0, &mut s0).unwrap();
        base.stored_rows(range.clone(), &mut c1, &mut s1).unwrap();
        assert_eq!(c0, c1, "bits={bits} codes {range:?}");
        assert_eq!(s0, s1, "bits={bits} scales {range:?}");
    }
    for k in [1usize, 10, 100] {
        let (a, b) = (mapped.search(&q, k), base.search(&q, k));
        assert_eq!(a.indices, b.indices, "bits={bits} k={k}");
        let bits_of = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits_of(&a.scores), bits_of(&b.scores), "bits={bits} k={k}");
    }
    let top = base.search(&q, 50);
    let floor = (0..4).map(|qi| top.scores[qi * 50 + 49]).fold(f32::INFINITY, f32::min);
    assert_eq!(streamed(&mapped, &q, floor, 4_096), streamed(&base, &q, floor, 4_096), "bits={bits}");
    drop(mapped);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_2bit_planes_layout_file_serves_mapped() {
    a_planes_layout_file_serves_mapped(2, false);
}

#[test]
fn a_4bit_planes_layout_file_serves_mapped() {
    a_planes_layout_file_serves_mapped(4, false);
}

#[test]
fn a_2bit_planes_file_above_the_stock_threshold_serves_mapped() {
    a_planes_layout_file_serves_mapped(2, true);
}

#[test]
fn a_4bit_planes_file_above_the_stock_threshold_serves_mapped() {
    a_planes_layout_file_serves_mapped(4, true);
}

#[test]
fn streaming_over_a_4bit_planes_cache_emits_the_classic_candidates() {
    // At 4 bits the sign region is a quarter of the code bytes, not half
    // as at 2 bits; a chunk sliced with the 2-bit size reads the wrong
    // rows (and runs off the end of the cache past the first chunks).
    if !planes_supported_at(4) {
        return;
    }
    let n = 5_000;
    let data = unit_vectors(n, 51);
    let q = unit_vectors(3, 52);
    let base = classic(|| build_bits(&data, 4));
    let _on = PlanesOn::new(0);
    let ix = build_bits(&data, 4);
    assert!(is_planes(&ix) && !is_planes(&base));
    let top = base.search(&q, 50);
    let floor = (0..3).map(|qi| top.scores[qi * 50 + 49]).fold(f32::INFINITY, f32::min);
    let want = streamed(&base, &q, floor, 256);
    assert!(want.iter().all(|r| r.len() >= 50));
    assert_eq!(streamed(&ix, &q, floor, 256), want);
}

/// No test override: the stock threshold (`PLANES_MIN_VECTORS`) puts the
/// index in the planes layout, and an unseeded stream over every chunk
/// must emit every row with the classic scan's exact score.
fn streaming_above_the_stock_planes_threshold(bits: usize) {
    if !planes_supported_at(bits) {
        return;
    }
    let n = pack::PLANES_MIN_VECTORS + 7_232; // 40,000: whole and partial chunks
    let data = unit_vectors(n, 60 + bits as u64);
    let q = unit_vectors(2, 70 + bits as u64);
    let base = classic(|| build_bits(&data, bits));
    let ix = build_bits(&data, bits);
    assert!(is_planes(&ix) && !is_planes(&base), "bits={bits}");
    let want = streamed(&base, &q, f32::NEG_INFINITY, 4_096);
    assert!(want.iter().all(|r| r.len() == n), "bits={bits}");
    assert_eq!(streamed(&ix, &q, f32::NEG_INFINITY, 4_096), want, "bits={bits}");
}

#[test]
fn streaming_a_2bit_index_above_the_stock_planes_threshold() {
    streaming_above_the_stock_planes_threshold(2);
}

#[test]
fn streaming_a_4bit_index_above_the_stock_planes_threshold() {
    streaming_above_the_stock_planes_threshold(4);
}

/// (slot, score bits) per query, for comparing whole result sets.
fn rows_of(r: &crate::SearchResults) -> Vec<Vec<(i64, u32)>> {
    (0..r.nq)
        .map(|qi| {
            (0..r.k)
                .map(|j| (r.indices[qi * r.k + j], r.scores[qi * r.k + j].to_bits()))
                .collect()
        })
        .collect()
}

/// `SearchOptions::exhaustive` on a planes cache past the stock threshold
/// returns the whole-index scan's top-k bit for bit: ids, scores, padding,
/// with and without a mask and a seeded floor, from k = 1 to past n.
fn exact(o: SearchOptions<'_>) -> SearchOptions<'_> {
    o.with_exhaustive(true)
}

fn exhaustive_planes_top_k_equals_the_classic_scan(bits: usize) {
    if !planes_supported_at(bits) {
        return;
    }
    let n = pack::PLANES_MIN_VECTORS + 7_232;
    let data = unit_vectors(n, 80 + bits as u64);
    let q = unit_vectors(5, 90 + bits as u64);
    let base = classic(|| build_bits(&data, bits));
    let ix = build_bits(&data, bits);
    assert!(is_planes(&ix) && !is_planes(&base), "bits={bits}");
    let mask: Vec<bool> = (0..n).map(|v| v % 3 != 1).collect();
    for k in [1usize, 10, 100, 1_000, n + 5] {
        let want = base.search(&q, k);
        let got = ix.search_with_options(&q, k, exact(SearchOptions::new()));
        assert_eq!(rows_of(&got), rows_of(&want), "bits={bits} k={k}");
        let want = base.search_with_options(&q, k, SearchOptions::new().with_mask(&mask));
        let got = ix.search_with_options(&q, k, exact(SearchOptions::new().with_mask(&mask)));
        assert_eq!(rows_of(&got), rows_of(&want), "bits={bits} k={k} masked");
        if k <= 100 {
            // A floor at the k-th best of the first query cuts every row.
            let floor = base.search(&q[..DIM], k).scores[k - 1];
            let seed = SearchOptions::new().with_initial_threshold(floor);
            let want = base.search_with_options(&q, k, seed);
            let got = ix.search_with_options(&q, k, exact(seed));
            assert_eq!(rows_of(&got), rows_of(&want), "bits={bits} k={k} seeded");
        }
    }
    // The flag is a no-op on the classic layout.
    let k = 10;
    assert_eq!(
        rows_of(&base.search_with_options(&q, k, exact(SearchOptions::new()))),
        rows_of(&base.search(&q, k))
    );
}

#[test]
fn exhaustive_2bit_planes_top_k_equals_the_classic_scan() {
    exhaustive_planes_top_k_equals_the_classic_scan(2);
}

#[test]
fn exhaustive_4bit_planes_top_k_equals_the_classic_scan() {
    exhaustive_planes_top_k_equals_the_classic_scan(4);
}

/// The staged (default) search is deterministic: the same index and
/// queries return the same ids and score bits on every call, and a batch
/// returns the same rows on 1, 2 or 8 threads. One query per call is
/// stable across every pool of two or more threads, but its shortlist is
/// built differently on one thread (upstream's single-query pool route),
/// so a serial and a parallel caller can disagree on ids at k >= 100;
/// only `exhaustive` removes that.
fn staged_top_k_is_deterministic(bits: usize) {
    if !planes_supported_at(bits) {
        return;
    }
    let n = pack::PLANES_MIN_VECTORS + 7_232;
    let data = unit_vectors(n, 100 + bits as u64);
    let q = unit_vectors(6, 110 + bits as u64);
    let ix = build_bits(&data, bits);
    assert!(is_planes(&ix), "bits={bits}");
    let in_pool = |threads: usize, k: usize| {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        pool.install(|| {
            let batched = rows_of(&ix.search(&q, k));
            let singles: Vec<Vec<(i64, u32)>> =
                q.chunks(DIM).flat_map(|one| rows_of(&ix.search(one, k))).collect();
            (batched, singles)
        })
    };
    for k in [1usize, 10, 100, 1_000] {
        let (batched1, singles1) = in_pool(1, k);
        let (batched2, singles2) = in_pool(2, k);
        assert_eq!(batched1, singles1, "bits={bits} k={k} serial single vs batch");
        for threads in [2usize, 8] {
            let (batched, singles) = in_pool(threads, k);
            assert_eq!(batched, batched1, "bits={bits} k={k} threads={threads} batched");
            assert_eq!(singles, singles2, "bits={bits} k={k} threads={threads} singles");
            assert_eq!(in_pool(threads, k), (batched, singles), "bits={bits} k={k} repeat");
        }
        // Exhaustive is one answer everywhere.
        let want = rows_of(&ix.search_with_options(&q, k, exact(SearchOptions::new())));
        for threads in [1usize, 2, 8] {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            let got = pool.install(|| {
                q.chunks(DIM)
                    .flat_map(|one| rows_of(&ix.search_with_options(one, k, exact(SearchOptions::new()))))
                    .collect::<Vec<_>>()
            });
            assert_eq!(got, want, "bits={bits} k={k} threads={threads} exhaustive");
        }
    }
}

#[test]
fn staged_2bit_top_k_is_deterministic() {
    staged_top_k_is_deterministic(2);
}

#[test]
fn staged_4bit_top_k_is_deterministic() {
    staged_top_k_is_deterministic(4);
}

