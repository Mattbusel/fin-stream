//! Stress tests for OrderBook: large depth, sorted order invariants, and best-bid < best-ask.

use fin_stream::book::{BookDelta, BookSide, OrderBook};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Insert 1000 bid levels and 1000 ask levels, then verify sorted order and the
/// best_bid < best_ask invariant.
#[test]
fn test_book_stress_insert_1000_levels_sorted() {
    let mut book = OrderBook::new("BTC-USD");

    // Insert 1000 bid levels at prices 1..=1000 (all below 1001)
    for i in 1u32..=1000 {
        let price = Decimal::from(i);
        let qty = Decimal::from(i);
        book.apply(BookDelta::new("BTC-USD", BookSide::Bid, price, qty)).unwrap();
    }

    // Insert 1000 ask levels at prices 1001..=2000 (all above 1000)
    for i in 1001u32..=2000 {
        let price = Decimal::from(i);
        let qty = Decimal::from(i);
        book.apply(BookDelta::new("BTC-USD", BookSide::Ask, price, qty)).unwrap();
    }

    assert_eq!(book.bid_depth(), 1000);
    assert_eq!(book.ask_depth(), 1000);

    // Best bid is the highest bid: 1000
    assert_eq!(book.best_bid().unwrap().price, Decimal::from(1000u32));

    // Best ask is the lowest ask: 1001
    assert_eq!(book.best_ask().unwrap().price, Decimal::from(1001u32));

    // Invariant: best_bid < best_ask (must never be crossed)
    let best_bid = book.best_bid().unwrap().price;
    let best_ask = book.best_ask().unwrap().price;
    assert!(best_bid < best_ask, "best_bid ({best_bid}) must be < best_ask ({best_ask})");
}

/// Verify that top_bids returns levels in strictly descending order.
#[test]
fn test_book_stress_top_bids_descending_order() {
    let mut book = OrderBook::new("ETH-USD");

    // Insert 500 bid levels in random-ish order (reverse here for variety)
    for i in (1u32..=500).rev() {
        let price = Decimal::from(i * 10);
        book.apply(BookDelta::new("ETH-USD", BookSide::Bid, price, dec!(1))).unwrap();
    }
    // Add ask levels so the book is not crossed
    for i in 501u32..=510 {
        let price = Decimal::from(i * 10);
        book.apply(BookDelta::new("ETH-USD", BookSide::Ask, price, dec!(1))).unwrap();
    }

    let top = book.top_bids(100);
    assert_eq!(top.len(), 100);
    for window in top.windows(2) {
        assert!(
            window[0].price > window[1].price,
            "top_bids not in descending order: {} vs {}",
            window[0].price,
            window[1].price,
        );
    }
}

/// Verify that top_asks returns levels in strictly ascending order.
#[test]
fn test_book_stress_top_asks_ascending_order() {
    let mut book = OrderBook::new("ETH-USD");

    // Bid levels below ask range
    book.apply(BookDelta::new("ETH-USD", BookSide::Bid, dec!(100), dec!(1))).unwrap();

    // Insert 500 ask levels in forward order
    for i in 1u32..=500 {
        let price = Decimal::from(1000 + i);
        book.apply(BookDelta::new("ETH-USD", BookSide::Ask, price, dec!(1))).unwrap();
    }

    let top = book.top_asks(100);
    assert_eq!(top.len(), 100);
    for window in top.windows(2) {
        assert!(
            window[0].price < window[1].price,
            "top_asks not in ascending order: {} vs {}",
            window[0].price,
            window[1].price,
        );
    }
}

/// After inserting 1000 levels and removing every other one, the invariant still holds.
#[test]
fn test_book_stress_remove_alternating_levels_invariant_holds() {
    let mut book = OrderBook::new("BTC-USD");

    for i in 1u32..=200 {
        book.apply(BookDelta::new("BTC-USD", BookSide::Bid, Decimal::from(i), dec!(1))).unwrap();
    }
    for i in 201u32..=400 {
        book.apply(BookDelta::new("BTC-USD", BookSide::Ask, Decimal::from(i), dec!(1))).unwrap();
    }

    // Remove every even bid level
    for i in (2u32..=200).step_by(2) {
        book.apply(BookDelta::new("BTC-USD", BookSide::Bid, Decimal::from(i), dec!(0))).unwrap();
    }
    // Remove every even ask level
    for i in (202u32..=400).step_by(2) {
        book.apply(BookDelta::new("BTC-USD", BookSide::Ask, Decimal::from(i), dec!(0))).unwrap();
    }

    let best_bid = book.best_bid().unwrap().price;
    let best_ask = book.best_ask().unwrap().price;
    assert!(best_bid < best_ask, "crossed book after removals: {best_bid} >= {best_ask}");
}

/// Mid-price remains in (best_bid, best_ask) after many updates.
#[test]
fn test_book_stress_mid_price_between_bid_and_ask() {
    let mut book = OrderBook::new("BTC-USD");

    for i in 1u32..=50 {
        book.apply(BookDelta::new("BTC-USD", BookSide::Bid, Decimal::from(i * 100), dec!(1))).unwrap();
        book.apply(BookDelta::new(
            "BTC-USD",
            BookSide::Ask,
            Decimal::from(5000 + i * 100),
            dec!(1),
        ))
        .unwrap();
    }

    let mid = book.mid_price().unwrap();
    let best_bid = book.best_bid().unwrap().price;
    let best_ask = book.best_ask().unwrap().price;
    assert!(mid > best_bid, "mid ({mid}) should be > best_bid ({best_bid})");
    assert!(mid < best_ask, "mid ({mid}) should be < best_ask ({best_ask})");
}
