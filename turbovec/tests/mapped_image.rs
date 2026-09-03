//! Mapped serving of a v7 image ([`TurboQuantIndex::load_mapped`]).
//!
//! The contract: a mapped index answers every search — top-k, masked,
//! floor-seeded, batched, streaming — with the same scores and slots,
//! bit for bit, as the same file loaded into memory; it serves a file
//! whose commit header carries pending removal ops and a partial tail
//! block; it is read-only and says so; a `write` from it reproduces the
//! loaded index's bytes; opening it costs a small fraction of the
//! image in resident memory where a load costs the image; and a v6 file
//! is refused with the conversion advice, not served.

use std::path::PathBuf;

use turbovec::{CalibrateError, SearchOptions, SearchResults, StreamControl, TurboQuantIndex};

const DIM: usize = 64;
const BITS: usize = 4;

fn unit_vectors(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut out = vec![0.0f32; n * dim];
    for row in out.chunks_mut(dim) {
        let mut norm = 0.0f64;
        for x in row.iter_mut() {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
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

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("turbovec-mapped-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An index of `n` rows, calibrated, written as a v7 image at `path`.
fn build(n: usize, dim: usize, seed: u64, path: &std::path::Path) -> TurboQuantIndex {
    let rows = unit_vectors(n, dim, seed);
    let mut index = TurboQuantIndex::new(dim, BITS).unwrap();
    index
        .calibrate(&rows[..(2048 * dim).min(rows.len())])
        .unwrap();
    index.add(&rows);
    index.write(path).unwrap();
    index
}

fn bits(results: &SearchResults) -> Vec<(u32, i64)> {
    results
        .scores
        .iter()
        .zip(&results.indices)
        .map(|(s, i)| (s.to_bits(), *i))
        .collect()
}

fn assert_same_search(loaded: &TurboQuantIndex, mapped: &TurboQuantIndex, queries: &[f32], k: usize) {
    let a = loaded.search_with_options(queries, k, SearchOptions::new());
    let b = mapped.search_with_options(queries, k, SearchOptions::new());
    assert_eq!(a.k, b.k);
    assert_eq!(a.nq, b.nq);
    assert_eq!(bits(&a), bits(&b), "k={k}");
}

#[test]
fn mapped_search_equals_the_loaded_index_bit_for_bit() {
    // Two full chunks, a partial chunk, and a partial last block.
    let n = 2 * 8192 + 1000 + 17;
    let dir = tempdir("equal");
    let path = dir.join("image.tv");
    let built = build(n, DIM, 1, &path);
    let loaded = TurboQuantIndex::load(&path).unwrap();
    let mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    assert!(mapped.is_mapped());
    assert!(!loaded.is_mapped());
    assert_eq!(mapped.len(), n);
    assert_eq!(mapped.dim(), DIM);
    assert_eq!(mapped.tqplus_shift(), built.tqplus_shift());
    assert_eq!(mapped.tqplus_scale(), built.tqplus_scale());
    let queries = unit_vectors(3, DIM, 99);
    for k in [1, 10, 257, 5000] {
        assert_same_search(&loaded, &mapped, &queries, k);
        assert_same_search(&built, &mapped, &queries, k);
    }
    // One query of the batch, alone: the batch floor is a minimum
    // across queries, and pruning under it must not change any row.
    for q in 0..3 {
        assert_same_search(&loaded, &mapped, &queries[q * DIM..(q + 1) * DIM], 100);
    }
    // A mask that keeps ~30% of the rows, including the tail block.
    let mask: Vec<bool> = (0..n).map(|i| i % 10 < 3 || i >= n - 17).collect();
    let a = loaded.search_with_options(&queries, 100, SearchOptions::new().with_mask(&mask));
    let b = mapped.search_with_options(&queries, 100, SearchOptions::new().with_mask(&mask));
    assert_eq!(bits(&a), bits(&b));
    for &i in &b.indices {
        assert!(mask[i as usize]);
    }
    // A seeded floor: the loaded index's own k-th best, and one above it.
    let kth = a.scores_for_query(0)[9];
    for floor in [kth, kth + 1e-3] {
        let options = || SearchOptions::new().with_mask(&mask).with_initial_threshold(floor);
        let a = loaded.search_with_options(&queries, 10, options());
        let b = mapped.search_with_options(&queries, 10, options());
        assert_eq!(bits(&a), bits(&b), "floor {floor}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mapped_streaming_equals_the_loaded_streaming_scan() {
    let n = 8192 + 4096 + 5;
    let dir = tempdir("stream");
    let path = dir.join("image.tv");
    build(n, DIM, 2, &path);
    let loaded = TurboQuantIndex::load(&path).unwrap();
    let mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    let queries = unit_vectors(2, DIM, 7);
    let collect = |index: &TurboQuantIndex| {
        let mut out: Vec<(usize, i64, u32)> = Vec::new();
        let summary = index.search_streaming(
            &queries,
            SearchOptions::new().with_initial_threshold(0.05),
            |batch| {
                for (slot, score) in batch.slots.iter().zip(batch.scores) {
                    out.push((batch.query_index, *slot, score.to_bits()));
                }
                StreamControl::Continue
            },
        );
        assert!(summary.completed);
        out.sort_unstable();
        (summary.emitted, out)
    };
    assert_eq!(collect(&loaded), collect(&mapped));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_synced_file_with_pending_removal_ops_serves_mapped() {
    let n = 8192 + 300;
    let dir = tempdir("ops");
    let path = dir.join("image.tv");
    let mut index = build(n, DIM, 3, &path);
    // Sync, remove rows in a committed block and in the tail, sync
    // again: the removals ride the commit header as redo ops until a
    // later sync materializes them.
    index.sync(&path).unwrap();
    for idx in [5, 40, 8192 + 100, 1] {
        index.swap_remove(idx);
    }
    index.sync(&path).unwrap();
    let loaded = TurboQuantIndex::load(&path).unwrap();
    let mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    assert_eq!(loaded.len(), index.len());
    assert_eq!(mapped.len(), index.len());
    let queries = unit_vectors(2, DIM, 11);
    for k in [1, 50, 400] {
        assert_same_search(&loaded, &mapped, &queries, k);
        assert_same_search(&index, &mapped, &queries, k);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mapped_index_is_read_only_and_says_so() {
    let dir = tempdir("readonly");
    let path = dir.join("image.tv");
    let loaded = build(100, DIM, 4, &path);
    let mut mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    let sample = unit_vectors(2048, DIM, 5);
    assert_eq!(
        mapped.calibrate(&sample).unwrap_err(),
        CalibrateError::MappedReadOnly
    );
    let sync = mapped.sync(dir.join("other.tv")).unwrap_err();
    assert_eq!(sync.kind(), std::io::ErrorKind::Unsupported);
    assert!(sync.to_string().contains("read-only"), "{sync}");
    // Reads that need the whole layout materialize it on request and
    // reproduce the loaded index's bytes.
    assert_eq!(mapped.to_bytes(), loaded.to_bytes());
    assert_eq!(mapped.packed_codes(), loaded.packed_codes());
    let copy = dir.join("copy.tv");
    mapped.write(&copy).unwrap();
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&path).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[should_panic(expected = "mapped image and read-only")]
fn adding_to_a_mapped_index_panics_by_name() {
    let dir = tempdir("add");
    let path = dir.join("image.tv");
    build(100, DIM, 6, &path);
    let mut mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    mapped.add(&unit_vectors(1, DIM, 8));
}

#[test]
#[should_panic(expected = "mapped image and read-only")]
fn removing_from_a_mapped_index_panics_by_name() {
    let dir = tempdir("remove");
    let path = dir.join("image.tv");
    build(100, DIM, 6, &path);
    let mut mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    mapped.swap_remove(3);
}

#[test]
fn a_legacy_file_is_refused_with_conversion_advice() {
    let dir = tempdir("legacy");
    let v7 = dir.join("image.tv");
    build(2000, DIM, 9, &v7);
    let v6 = dir.join("image-v6.tv");
    turbovec::convert::convert_file(&v7, &v6, turbovec::convert::Version::V6).unwrap();
    let error = TurboQuantIndex::load_mapped(&v6).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("version 6"), "{message}");
    assert!(message.contains("convert"), "{message}");
    assert!(TurboQuantIndex::load(&v6).is_err());
    // Converted forward again, the same rows serve mapped.
    let back = dir.join("image-back.tv");
    turbovec::convert::convert_file(&v6, &back, turbovec::convert::Version::V7).unwrap();
    let loaded = TurboQuantIndex::load(&v7).unwrap();
    let mapped = TurboQuantIndex::load_mapped(&back).unwrap();
    assert_same_search(&loaded, &mapped, &unit_vectors(1, DIM, 10), 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let resident_pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    resident_pages * 4096
}

/// Opening a mapped image costs the headers, not the image; a load
/// costs the image. The same file, measured in this process.
#[cfg(target_os = "linux")]
#[test]
fn mapped_open_keeps_resident_memory_far_below_a_load() {
    let dim = 1024;
    let n = 40_000; // 1024 x 4 bits = 512 B/row -> ~20 MiB of codes
    let dir = tempdir("rss");
    let path = dir.join("image.tv");
    {
        let rows = unit_vectors(n, dim, 12);
        let mut index = TurboQuantIndex::new(dim, BITS).unwrap();
        index.calibrate(&rows[..2048 * dim]).unwrap();
        index.add(&rows);
        index.write(&path).unwrap();
    }
    let image_bytes = std::fs::metadata(&path).unwrap().len() as usize;
    assert!(image_bytes > 16 * 1024 * 1024, "image is {image_bytes} bytes");

    let before = rss_bytes();
    let mapped = TurboQuantIndex::load_mapped(&path).unwrap();
    let after_map = rss_bytes();
    let mapped_growth = after_map.saturating_sub(before);
    assert!(
        mapped_growth < image_bytes / 8,
        "mapped open grew RSS by {mapped_growth} bytes for a {image_bytes}-byte image"
    );

    let before = rss_bytes();
    let loaded = TurboQuantIndex::load(&path).unwrap();
    let after_load = rss_bytes();
    let load_growth = after_load.saturating_sub(before);
    assert!(
        load_growth > image_bytes / 2,
        "a load grew RSS by only {load_growth} bytes for a {image_bytes}-byte image"
    );
    // Both serve the same answer; the mapped one is still bounded by
    // its chunk cache, not the image.
    let queries = unit_vectors(1, dim, 13);
    assert_same_search(&loaded, &mapped, &queries, 10);
    let _ = std::fs::remove_dir_all(&dir);
}
