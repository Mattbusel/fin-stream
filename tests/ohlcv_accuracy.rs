//! Accuracy tests for OhlcvAggregator: hand-computed expected values.

use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::tick::{Exchange, NormalizedTick, TradeSide};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn make_tick(symbol: &str, price: Decimal, qty: Decimal, ts_ms: u64) -> NormalizedTick {
    NormalizedTick {
        exchange: Exchange::Binance,
        symbol: symbol.to_string(),
        price,
        quantity: qty,
        side: Some(TradeSide::Buy),
        trade_id: None,
        exchange_ts_ms: None,
        received_at_ms: ts_ms,
    }
}

/// Feed a hand-designed tick sequence and verify every OHLCV field against
/// manually computed expected values.
///
/// Sequence (all in the 60_000–119_999 ms window, i.e. minute 1):
///   1. price=100, qty=5   -> open=100, high=100, low=100, close=100, vol=5,  count=1
///   2. price=110, qty=3   -> open=100, high=110, low=100, close=110, vol=8,  count=2
///   3. price=90,  qty=2   -> open=100, high=110, low=90,  close=90,  vol=10, count=3
///   4. price=105, qty=4   -> open=100, high=110, low=90,  close=105, vol=14, count=4
///
/// Then a tick at 120_000 closes the bar.
#[test]
fn test_ohlcv_accuracy_hand_computed_values() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();

    agg.feed(&make_tick("BTC-USD", dec!(100), dec!(5), 60_000)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(110), dec!(3), 60_100)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(90),  dec!(2), 60_200)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(105), dec!(4), 60_300)).unwrap();

    // Check partial bar before close
    {
        let bar = agg.current_bar().unwrap();
        assert_eq!(bar.open,        dec!(100),  "open mismatch");
        assert_eq!(bar.high,        dec!(110),  "high mismatch");
        assert_eq!(bar.low,         dec!(90),   "low mismatch");
        assert_eq!(bar.close,       dec!(105),  "close mismatch");
        assert_eq!(bar.volume,      dec!(14),   "volume mismatch");
        assert_eq!(bar.trade_count, 4,          "trade_count mismatch");
        assert!(!bar.is_complete,               "bar should not yet be complete");
        assert_eq!(bar.bar_start_ms, 60_000,    "bar_start_ms mismatch");
    }

    // Trigger bar completion
    let mut completed = agg.feed(&make_tick("BTC-USD", dec!(200), dec!(1), 120_000)).unwrap();
    assert_eq!(completed.len(), 1, "expected exactly one completed bar");
    let bar = completed.remove(0);

    assert_eq!(bar.open,        dec!(100),  "completed open mismatch");
    assert_eq!(bar.high,        dec!(110),  "completed high mismatch");
    assert_eq!(bar.low,         dec!(90),   "completed low mismatch");
    assert_eq!(bar.close,       dec!(105),  "completed close mismatch");
    assert_eq!(bar.volume,      dec!(14),   "completed volume mismatch");
    assert_eq!(bar.trade_count, 4,          "completed trade_count mismatch");
    assert!(bar.is_complete,                "completed bar must be marked complete");
    assert_eq!(bar.bar_start_ms, 60_000,   "completed bar_start_ms mismatch");
}

/// A single-tick bar: OHLCV are all equal to that tick's price and volume.
#[test]
fn test_ohlcv_accuracy_single_tick_bar() {
    let mut agg = OhlcvAggregator::new("ETH-USD", Timeframe::Seconds(30)).unwrap();
    agg.feed(&make_tick("ETH-USD", dec!(3000), dec!(7), 30_000)).unwrap();
    let bar = agg.flush().unwrap();

    assert_eq!(bar.open,        dec!(3000));
    assert_eq!(bar.high,        dec!(3000));
    assert_eq!(bar.low,         dec!(3000));
    assert_eq!(bar.close,       dec!(3000));
    assert_eq!(bar.volume,      dec!(7));
    assert_eq!(bar.trade_count, 1);
    assert!(bar.is_complete);
}

/// Two consecutive 1-minute bars; verify open of bar 2 == close of the last tick in bar 1.
#[test]
fn test_ohlcv_accuracy_consecutive_bar_open_close_relationship() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();

    // Bar 1 ticks
    agg.feed(&make_tick("BTC-USD", dec!(50000), dec!(1), 60_000)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(51000), dec!(1), 60_500)).unwrap(); // close of bar 1

    // Bar 2 first tick closes bar 1
    let bars = agg.feed(&make_tick("BTC-USD", dec!(52000), dec!(1), 120_000)).unwrap();
    let bar1 = &bars[0];

    // Close of bar 1 = last price seen in bar 1 = 51000
    assert_eq!(bar1.close, dec!(51000));

    // Open of bar 2 = first price in bar 2 = 52000
    let bar2 = agg.current_bar().unwrap();
    assert_eq!(bar2.open, dec!(52000));
}

/// Verify volume accumulation precision with decimal quantities.
#[test]
fn test_ohlcv_accuracy_volume_decimal_precision() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();

    let quantities = [dec!(0.1), dec!(0.25), dec!(0.333), dec!(1.5)];
    let expected_volume = dec!(0.1) + dec!(0.25) + dec!(0.333) + dec!(1.5);

    for (i, &qty) in quantities.iter().enumerate() {
        agg.feed(&make_tick("BTC-USD", dec!(50000), qty, 60_000 + i as u64 * 10)).unwrap();
    }

    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.volume, expected_volume, "volume decimal precision incorrect");
    assert_eq!(bar.trade_count, 4);
}

/// High and low are tracked correctly even when ticks arrive out of monotone order.
#[test]
fn test_ohlcv_accuracy_high_low_nonmonotone_sequence() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1)).unwrap();

    // Prices: 200, 50, 300, 100, 150 — high=300, low=50
    let prices = [dec!(200), dec!(50), dec!(300), dec!(100), dec!(150)];
    for (i, &price) in prices.iter().enumerate() {
        agg.feed(&make_tick("BTC-USD", price, dec!(1), 60_000 + i as u64 * 100)).unwrap();
    }

    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.high, dec!(300), "high not tracked correctly");
    assert_eq!(bar.low,  dec!(50),  "low not tracked correctly");
    assert_eq!(bar.open, dec!(200), "open should equal first tick price");
    assert_eq!(bar.close, dec!(150), "close should equal last tick price");
}

/// Verify that the 5-minute timeframe correctly aligns bar boundaries.
/// A tick at 299_999 ms is in the same bar as one at 300_000 only if rounded down.
/// 299_999 / 300_000 = 0 => bar_start=0. 300_000 / 300_000 = 1 => bar_start=300_000.
#[test]
fn test_ohlcv_accuracy_5min_boundary_alignment() {
    let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(5)).unwrap();

    // Both ticks at ts_ms 300_000 and 599_999 land in the same 5-min bar (start=300_000)
    agg.feed(&make_tick("BTC-USD", dec!(100), dec!(1), 300_000)).unwrap();
    agg.feed(&make_tick("BTC-USD", dec!(110), dec!(1), 599_999)).unwrap();

    let bar = agg.current_bar().unwrap();
    assert_eq!(bar.bar_start_ms, 300_000);
    assert_eq!(bar.trade_count, 2);

    // Tick at 600_000 opens a new bar
    let completed = agg.feed(&make_tick("BTC-USD", dec!(120), dec!(1), 600_000)).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].bar_start_ms, 300_000);
    assert_eq!(agg.current_bar().unwrap().bar_start_ms, 600_000);
}
