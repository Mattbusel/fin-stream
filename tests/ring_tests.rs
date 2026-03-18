//! Integration tests for the SPSC ring buffer (`fin_stream::ring`).
//!
//! These tests exercise the public API through the crate boundary, verifying
//! the boundary conditions specified in the task: overwrite-on-full semantics,
//! empty-pop behaviour, and capacity-one edge cases.

use fin_stream::error::StreamError;
use fin_stream::ring::SpscRing;

// ── test_ring_full_overwrites_oldest ─────────────────────────────────────────

/// When the ring is full, pushing an additional item must fail rather than
/// silently overwrite.  After draining to make room, the oldest item that was
/// present when the ring was full is the first one returned (FIFO).
///
/// This test validates both the "full → error" and the "oldest is first out"
/// contract together.
#[test]
fn test_ring_full_overwrites_oldest() {
    // Capacity = N - 1 = 3 (N = 4).
    let ring: SpscRing<u32, 4> = SpscRing::new();

    // Fill to capacity.
    ring.push(10).unwrap();
    ring.push(20).unwrap();
    ring.push(30).unwrap();
    assert!(
        ring.is_full(),
        "ring should be full after 3 pushes (capacity = 3)"
    );

    // Push on a full ring must fail with RingBufferFull.
    let err = ring.push(99).unwrap_err();
    assert!(
        matches!(err, StreamError::RingBufferFull { capacity: 3 }),
        "expected RingBufferFull {{ capacity: 3 }}, got {:?}",
        err
    );

    // The oldest item (10) was NOT overwritten — it is still the first item out.
    assert_eq!(ring.pop().unwrap(), 10, "oldest item must still be 10");
    assert_eq!(ring.pop().unwrap(), 20);
    assert_eq!(ring.pop().unwrap(), 30);
    assert!(ring.is_empty());
}

// ── test_ring_empty_pop_returns_none ─────────────────────────────────────────

/// Popping from an empty ring must return `Err(RingBufferEmpty)`, not a
/// sentinel `None` or a panic.
#[test]
fn test_ring_empty_pop_returns_none() {
    let ring: SpscRing<u64, 8> = SpscRing::new();

    // Pop on a freshly constructed ring.
    let err = ring.pop().unwrap_err();
    assert!(
        matches!(err, StreamError::RingBufferEmpty),
        "expected RingBufferEmpty on initial pop, got {:?}",
        err
    );

    // Push one item, drain it, then pop again — ring is empty again.
    ring.push(42).unwrap();
    assert_eq!(ring.pop().unwrap(), 42);

    let err2 = ring.pop().unwrap_err();
    assert!(
        matches!(err2, StreamError::RingBufferEmpty),
        "expected RingBufferEmpty after draining, got {:?}",
        err2
    );
}

// ── test_ring_capacity_one ────────────────────────────────────────────────────

/// With N = 2 the usable capacity is exactly 1 (one sentinel slot is reserved).
/// Verify that:
/// - one push succeeds,
/// - a second push fails,
/// - after popping the item the ring accepts another push.
#[test]
fn test_ring_capacity_one() {
    // N = 2 → capacity = 1.
    let ring: SpscRing<i32, 2> = SpscRing::new();
    assert_eq!(ring.capacity(), 1);
    assert!(ring.is_empty());

    // Push the single allowed item.
    ring.push(7).unwrap();
    assert!(
        ring.is_full(),
        "ring of capacity 1 must be full after one push"
    );
    assert_eq!(ring.len(), 1);

    // A second push must be rejected.
    let err = ring.push(99).unwrap_err();
    assert!(
        matches!(err, StreamError::RingBufferFull { capacity: 1 }),
        "expected RingBufferFull {{ capacity: 1 }}, got {:?}",
        err
    );

    // Pop the item — ring is empty again.
    let v = ring.pop().unwrap();
    assert_eq!(v, 7);
    assert!(ring.is_empty());

    // Now we can push again.
    ring.push(42).unwrap();
    assert_eq!(ring.pop().unwrap(), 42);
}

// ── Additional boundary tests ────────────────────────────────────────────────

/// Verify that a ring accurately reports its capacity as N - 1.
#[test]
fn test_ring_capacity_n_minus_one() {
    let ring: SpscRing<u8, 8> = SpscRing::new();
    assert_eq!(ring.capacity(), 7);

    let ring16: SpscRing<u8, 16> = SpscRing::new();
    assert_eq!(ring16.capacity(), 15);
}

/// Interleaved push/pop across the capacity boundary must not corrupt data.
#[test]
fn test_ring_interleaved_push_pop_at_boundary() {
    let ring: SpscRing<u32, 4> = SpscRing::new(); // capacity = 3
    ring.push(1).unwrap();
    ring.push(2).unwrap();
    ring.push(3).unwrap();
    assert!(ring.is_full());

    // Pop one to make room.
    assert_eq!(ring.pop().unwrap(), 1);
    assert!(!ring.is_full());

    // Push a new item — wraps around the backing array.
    ring.push(4).unwrap();
    assert!(ring.is_full());

    assert_eq!(ring.pop().unwrap(), 2);
    assert_eq!(ring.pop().unwrap(), 3);
    assert_eq!(ring.pop().unwrap(), 4);
    assert!(ring.is_empty());
}

/// The `RingBufferFull` error carries the correct capacity in its payload.
#[test]
fn test_ring_buffer_full_error_capacity_matches() {
    let ring: SpscRing<u8, 8> = SpscRing::new(); // capacity = 7
    for i in 0..7u8 {
        ring.push(i).unwrap();
    }
    let err = ring.push(99).unwrap_err();
    match err {
        StreamError::RingBufferFull { capacity } => {
            assert_eq!(capacity, 7, "capacity in error must match ring capacity");
        }
        other => panic!("unexpected error: {:?}", other),
    }
}

/// Error Display implementations are stable and contain expected substrings.
#[test]
fn test_ring_error_display() {
    let full = StreamError::RingBufferFull { capacity: 1024 };
    let empty = StreamError::RingBufferEmpty;
    assert!(
        full.to_string().contains("1024"),
        "full display must include capacity"
    );
    assert!(
        full.to_string().contains("full"),
        "full display must say 'full'"
    );
    assert!(
        empty.to_string().contains("empty"),
        "empty display must say 'empty'"
    );
}
