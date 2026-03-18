//! Property-based tests using proptest.
//!
//! Covers:
//! - SPSC ring buffer FIFO ordering for arbitrary push/pop sequences.
//! - Normalization monotonicity: if a <= b, then normalize(a) <= normalize(b).

use fin_stream::norm::MinMaxNormalizer;
use fin_stream::ring::SpscRing;
use proptest::prelude::*;

// ── SPSC ring buffer FIFO ordering ───────────────────────────────────────────

proptest! {
    /// For any sequence of up to 15 u64 values, the ring buffer must return
    /// them in FIFO order and return exactly as many items as were pushed.
    #[test]
    fn prop_ring_fifo_ordering(items in prop::collection::vec(any::<u64>(), 0..=15usize)) {
        let ring: SpscRing<u64, 32> = SpscRing::new();
        let mut pushed = Vec::new();
        for &v in &items {
            if ring.push(v).is_ok() {
                pushed.push(v);
            }
        }
        let mut popped = Vec::new();
        while let Ok(v) = ring.pop() {
            popped.push(v);
        }
        prop_assert_eq!(popped, pushed, "FIFO ordering violated");
    }
}

// ── Normalization monotonicity ────────────────────────────────────────────────

proptest! {
    /// For any two values a and b drawn from the current window, if a <= b
    /// then normalize(a) <= normalize(b). Tests the monotonicity invariant.
    #[test]
    fn prop_normalization_monotonicity(
        // Window seeds: 5 values in [0.0, 1000.0]
        seeds in prop::collection::vec(0.0f64..=1000.0, 5..=10usize),
        // Two query values in the same range
        a in 0.0f64..=1000.0,
        b in 0.0f64..=1000.0,
    ) {
        let mut norm = MinMaxNormalizer::new(seeds.len());
        for &v in &seeds {
            norm.update(v);
        }
        if let (Ok(na), Ok(nb)) = (norm.normalize(a), norm.normalize(b)) {
            if a <= b {
                prop_assert!(na <= nb + 1e-12,
                    "monotonicity violated: normalize({a}) = {na} > normalize({b}) = {nb}");
            }
        }
    }
}

// ── Normalization range always [0, 1] ─────────────────────────────────────────

proptest! {
    /// normalize() always returns a value in [0.0, 1.0].
    #[test]
    fn prop_normalization_range_0_to_1(
        seeds in prop::collection::vec(0.0f64..=1000.0, 2..=20usize),
        query in any::<f64>(),
    ) {
        // Skip NaN/inf queries (not representable as market prices).
        prop_assume!(query.is_finite());
        let mut norm = MinMaxNormalizer::new(seeds.len());
        for &v in &seeds {
            norm.update(v);
        }
        let v = norm.normalize(query).unwrap();
        prop_assert!((0.0..=1.0).contains(&v),
            "normalize({query}) = {v} is outside [0.0, 1.0]");
    }
}
