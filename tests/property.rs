//! Property-based tests using proptest.
//!
//! Covers:
//! - SPSC ring buffer FIFO ordering for arbitrary push/pop sequences.
//! - Normalization monotonicity: if a <= b, then normalize(a) <= normalize(b).

use fin_stream::norm::MinMaxNormalizer;
use fin_stream::ring::SpscRing;
use proptest::prelude::*;
use rust_decimal::Decimal;

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
        // Window seeds: 5 integer values in [0, 100000]
        seeds in prop::collection::vec(0i64..=100_000, 5..=10usize),
        // Two query values in the same range
        a in 0i64..=100_000,
        b in 0i64..=100_000,
    ) {
        let mut norm = MinMaxNormalizer::new(seeds.len()).unwrap();
        for &v in &seeds {
            norm.update(Decimal::from(v));
        }
        let da = Decimal::from(a);
        let db = Decimal::from(b);
        if let (Ok(na), Ok(nb)) = (norm.normalize(da), norm.normalize(db)) {
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
        seeds in prop::collection::vec(0i64..=100_000, 2..=20usize),
        query in 0i64..=100_000,
    ) {
        let mut norm = MinMaxNormalizer::new(seeds.len()).unwrap();
        for &v in &seeds {
            norm.update(Decimal::from(v));
        }
        let v = norm.normalize(Decimal::from(query)).unwrap();
        prop_assert!((0.0..=1.0).contains(&v),
            "normalize({query}) = {v} is outside [0.0, 1.0]");
    }
}
