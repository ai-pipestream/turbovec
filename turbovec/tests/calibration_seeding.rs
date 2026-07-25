//! Tests for seeded TQ+ calibration
//! ([`TurboQuantIndex::new_with_calibration`] / [`TurboQuantIndex::calibration`]).
//!
//! The property under test: with a seeded calibration, encoding a vector
//! is a pure function of `(vector, calibration, dim, bit_width)` —
//! independent of build history, batch composition, or insertion order.
//! Two indexes seeded identically therefore produce byte-identical codes
//! and bit-identical scores for the same vector, which is what makes
//! results comparable across separately built indexes.

use turbovec::{io, ConstructError, IdMapIndex, TurboQuantIndex};

fn gaussian_normalized(n: usize, dim: usize, seed: u64) -> Vec<f32> {
    let mut state = seed | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut uniform = || {
        let raw = (next() >> 40) as u32 | 1;
        raw as f32 / (1u32 << 24) as f32
    };
    let two_pi = 2.0_f32 * std::f32::consts::PI;
    let mut data = vec![0.0f32; n * dim];
    let mut i = 0;
    while i < data.len() {
        let u1 = uniform().max(1e-7);
        let u2 = uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = two_pi * u2;
        data[i] = r * theta.cos();
        if i + 1 < data.len() {
            data[i + 1] = r * theta.sin();
        }
        i += 2;
    }
    for row in data.chunks_mut(dim) {
        let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            let inv = 1.0 / norm;
            for x in row.iter_mut() {
                *x *= inv;
            }
        }
    }
    data
}

/// Fit a non-identity calibration on a representative sample and return it.
fn fitted_calibration(dim: usize) -> (Vec<f32>, Vec<f32>) {
    let mut sample = TurboQuantIndex::new(dim, 4).unwrap();
    sample.add(&gaussian_normalized(1500, dim, 0x5EED_CA1B));
    let (shift, scale) = sample.calibration().expect("first add fits calibration");
    (shift.to_vec(), scale.to_vec())
}

#[test]
fn calibration_getter_none_before_first_add_some_after() {
    let dim = 64;
    let mut idx = TurboQuantIndex::new(dim, 4).unwrap();
    assert!(idx.calibration().is_none());

    idx.add(&gaussian_normalized(1500, dim, 0xA11C_E001));
    let (shift, scale) = idx.calibration().expect("calibration fitted by first add");
    assert_eq!(shift.len(), dim);
    assert_eq!(scale.len(), dim);
    // A 1500-sample gaussian batch must fit a non-identity calibration.
    assert!(
        shift.iter().any(|&x| x.abs() > 1e-6) || scale.iter().any(|&x| (x - 1.0).abs() > 1e-6),
        "fitted calibration is exactly identity",
    );
}

#[test]
fn seeded_index_reports_calibration_before_any_add() {
    let dim = 64;
    let (shift, scale) = fitted_calibration(dim);
    let idx = TurboQuantIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    assert_eq!(idx.len(), 0);
    let (got_shift, got_scale) = idx.calibration().expect("seeded at construction");
    assert_eq!(got_shift, shift.as_slice());
    assert_eq!(got_scale, scale.as_slice());
}

#[test]
fn same_seed_encodes_a_vector_byte_identically_across_indexes() {
    let dim = 64;
    let (shift, scale) = fitted_calibration(dim);
    let probe = gaussian_normalized(1, dim, 0x0B5E_55ED);

    // Index A: the probe alone. Index B: the probe first, then a corpus
    // that (unseeded) would have shifted the fitted calibration.
    let mut a = TurboQuantIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    a.add(&probe);
    let mut b = TurboQuantIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    b.add(&probe);
    b.add(&gaussian_normalized(1200, dim, 0xD1FF_C0DE));

    let dir = std::env::temp_dir();
    let pa = dir.join(format!("turbovec_seed_a_{}.tv", std::process::id()));
    let pb = dir.join(format!("turbovec_seed_b_{}.tv", std::process::id()));
    a.write(&pa).unwrap();
    b.write(&pb).unwrap();
    let (_, dim_a, _, _, scales_a, shift_a, scale_a) = io::load(&pa).unwrap();
    let (_, _, _, _, scales_b, shift_b, scale_b) = io::load(&pb).unwrap();
    let loaded_a = TurboQuantIndex::load(&pa).unwrap();
    let loaded_b = TurboQuantIndex::load(&pb).unwrap();
    let _ = std::fs::remove_file(&pa);
    let _ = std::fs::remove_file(&pb);

    // Same persisted calibration in both.
    assert_eq!(shift_a, shift_b);
    assert_eq!(scale_a, scale_b);
    assert_eq!(shift_a, shift);
    assert_eq!(scale_a, scale);

    // The probe occupies slot 0 in both indexes: its packed codes and
    // per-vector scale must be byte-identical despite the different
    // corpora that follow it. Compared through the bit-plane rows
    // (`packed_codes` reconstructs them from the v6 blocked payload),
    // where a vector's codes are one contiguous row.
    let bytes_per_row = (dim_a / 8) * 4; // bit_width 4
    assert_eq!(
        &loaded_a.packed_codes()[..bytes_per_row],
        &loaded_b.packed_codes()[..bytes_per_row],
    );
    assert_eq!(scales_a[0], scales_b[0]);
}

#[test]
fn same_seed_scores_a_vector_identically_across_corpora() {
    let dim = 64;
    let (shift, scale) = fitted_calibration(dim);
    let probe = gaussian_normalized(1, dim, 0x0B5E_55ED);
    let query = gaussian_normalized(1, dim, 0x9E4A_11CE);

    // Two id-map indexes with the probe under the same id but otherwise
    // disjoint corpora. The batch shapes are chosen so the test actually
    // discriminates a dropped seed: index A's first add is the lone
    // probe (which unseeded would lock identity calibration), while
    // index B's first add is 1200 vectors — above TQPLUS_MIN_SAMPLES —
    // so unseeded it would fit a non-identity calibration of its own
    // and the probe's codes would differ between the two indexes.
    // (An earlier version used 500 fillers for B: below the fitting
    // threshold, both indexes coincidentally fell back to identity and
    // the assertion kept passing with the seeding removed. Verified by
    // injection in both directions.)
    let mut a = IdMapIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    a.add_with_ids(&probe, &[42]).unwrap();
    a.add_with_ids(
        &gaussian_normalized(300, dim, 0xAAAA_0001),
        &(1000u64..1300).collect::<Vec<_>>(),
    )
    .unwrap();
    let mut b = IdMapIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    b.add_with_ids(
        &gaussian_normalized(1200, dim, 0xBBBB_0002),
        &(2000u64..3200).collect::<Vec<_>>(),
    )
    .unwrap();
    b.add_with_ids(&probe, &[42]).unwrap();

    let score_of = |idx: &IdMapIndex| {
        let (scores, ids) = idx.search(&query, idx.len());
        let pos = ids
            .iter()
            .position(|&id| id == 42)
            .expect("probe id present");
        scores[pos]
    };
    // Bit-identical, not approximately equal: the probe's codes and scale
    // are identical (see the byte-level test), and scoring a slot is a
    // pure function of its codes, its scale, the calibration, and the query.
    assert_eq!(score_of(&a), score_of(&b));
}

#[test]
fn seeded_calibration_survives_small_first_add() {
    // A 10-vector first add is far below TQPLUS_MIN_SAMPLES; unseeded it
    // would lock identity calibration. Seeded, the small batch must be
    // encoded with — and keep — the seeded calibration.
    let dim = 64;
    let (shift, scale) = fitted_calibration(dim);
    let mut idx = TurboQuantIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    idx.add(&gaussian_normalized(10, dim, 0x5A11_0B0B));
    assert_eq!(idx.len(), 10);
    let (got_shift, got_scale) = idx.calibration().unwrap();
    assert_eq!(got_shift, shift.as_slice());
    assert_eq!(got_scale, scale.as_slice());
}

#[test]
fn seeded_calibration_round_trips_through_write_load() {
    let dim = 64;
    let (shift, scale) = fitted_calibration(dim);
    let mut idx = TurboQuantIndex::new_with_calibration(dim, 4, &shift, &scale).unwrap();
    idx.add(&gaussian_normalized(20, dim, 0x0E0E_0E0E));

    let path =
        std::env::temp_dir().join(format!("turbovec_seed_roundtrip_{}.tv", std::process::id()));
    idx.write(&path).unwrap();
    let loaded = TurboQuantIndex::load(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let (got_shift, got_scale) = loaded.calibration().expect("calibration persisted");
    assert_eq!(got_shift, shift.as_slice());
    assert_eq!(got_scale, scale.as_slice());
}

/// `unwrap_err` needs `T: Debug`, which `TurboQuantIndex` deliberately
/// doesn't implement — extract the error by hand.
fn construct_err(dim: usize, bit_width: usize, shift: &[f32], scale: &[f32]) -> ConstructError {
    match TurboQuantIndex::new_with_calibration(dim, bit_width, shift, scale) {
        Ok(_) => panic!("expected new_with_calibration to fail"),
        Err(e) => e,
    }
}

#[test]
fn invalid_calibrations_are_rejected() {
    let dim = 64;
    let good_shift = vec![0.1f32; dim];
    let good_scale = vec![1.5f32; dim];

    // Wrong lengths.
    assert_eq!(
        construct_err(dim, 4, &good_shift[..dim - 1], &good_scale),
        ConstructError::CalibrationLengthMismatch {
            dim,
            shift_len: dim - 1,
            scale_len: dim,
        }
    );

    // Non-finite shift.
    let mut bad_shift = good_shift.clone();
    bad_shift[7] = f32::NAN;
    assert_eq!(
        construct_err(dim, 4, &bad_shift, &good_scale),
        ConstructError::CalibrationShiftNotFinite { coord_index: 7 }
    );

    // Zero, negative, and non-finite scales.
    for bad in [0.0f32, -1.0, f32::INFINITY, f32::NAN] {
        let mut bad_scale = good_scale.clone();
        bad_scale[3] = bad;
        assert_eq!(
            construct_err(dim, 4, &good_shift, &bad_scale),
            ConstructError::CalibrationScaleNotPositive { coord_index: 3 },
            "scale value {bad} must be rejected",
        );
    }

    // dim / bit_width validation still fires first.
    assert_eq!(
        construct_err(63, 4, &good_shift, &good_scale),
        ConstructError::DimNotPositiveMultipleOf8(63),
    );
    assert_eq!(
        construct_err(dim, 5, &good_shift, &good_scale),
        ConstructError::BitWidthOutOfRange(5),
    );
}
