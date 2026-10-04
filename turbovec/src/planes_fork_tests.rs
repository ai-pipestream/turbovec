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
    let _on = PlanesOn::new(0);
    let ok = pack::planes_for(2, DIM / 4);
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
    let mut ix = TurboQuantIndex::new(DIM, 2).unwrap();
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
        SearchOptions { mask: None, initial_threshold: Some(floor) },
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
            SearchOptions { mask: None, initial_threshold: Some(floor) },
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
    if !planes_supported() {
        return;
    }
    let n = 1_000;
    let data = unit_vectors(n, 31);
    let base = classic(|| build(&data));
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
