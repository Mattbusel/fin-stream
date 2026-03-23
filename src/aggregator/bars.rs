//! Tick-to-Bar Aggregator.
//!
//! Aggregates [`NormalizedTick`] streams into OHLCV bars.
//!
//! ## Bar Types ([`BarSpec`])
//!
//! | Variant | Close condition |
//! |---------|----------------|
//! | `Time(Duration)` | Bar duration elapses since first tick |
//! | `Tick(usize)` | N ticks accumulated |
//! | `Volume(f64)` | Cumulative volume ≥ threshold |
//! | `Dollar(f64)` | Cumulative dollar volume (price × qty) ≥ threshold |
//!
//! ## VWAP
//!
//! Updated online: `vwap = (vwap * cum_vol + price * qty) / (cum_vol + qty)`.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use fin_stream::aggregator::bars::{BarSpec, BarBuilder as NewBarBuilder};
//! use fin_stream::tick::{NormalizedTick, Exchange};
//! use rust_decimal::Decimal;
//!
//! let mut builder = NewBarBuilder::new("BTCUSDT", BarSpec::Tick(3));
//! // push ticks …
//! ```

use std::collections::HashMap;
use std::time::Duration;

use rust_decimal::prelude::ToPrimitive;

use crate::tick::NormalizedTick;

// ─── BarSpec ──────────────────────────────────────────────────────────────────

/// Determines when an open bar is closed and emitted.
#[derive(Debug, Clone)]
pub enum BarSpec {
    /// Close a bar after the given wall-clock duration since the first tick.
    Time(Duration),
    /// Close a bar after N individual ticks.
    Tick(usize),
    /// Close a bar when cumulative volume (quantity) reaches the threshold.
    Volume(f64),
    /// Close a bar when cumulative dollar volume (price × qty) reaches the threshold.
    Dollar(f64),
}

// ─── Bar ──────────────────────────────────────────────────────────────────────

/// A completed OHLCV bar.
#[derive(Debug, Clone)]
pub struct Bar {
    /// Instrument symbol.
    pub symbol: String,
    /// Opening price (price of the first tick).
    pub open: f64,
    /// Highest price seen during the bar.
    pub high: f64,
    /// Lowest price seen during the bar.
    pub low: f64,
    /// Closing price (price of the last tick).
    pub close: f64,
    /// Cumulative traded volume (sum of tick quantities).
    pub volume: f64,
    /// Volume-weighted average price.
    pub vwap: f64,
    /// Number of ticks accumulated in this bar.
    pub tick_count: usize,
    /// Millisecond timestamp of the first tick.
    pub start_ms: u64,
    /// Millisecond timestamp of the last tick (bar close).
    pub end_ms: u64,
}

// ─── BarBuilder ───────────────────────────────────────────────────────────────

/// Accumulates [`NormalizedTick`]s for a single symbol until a bar closes.
///
/// Call [`push`](BarBuilder::push) for each tick; it returns `Some(Bar)` when
/// the configured [`BarSpec`] boundary is crossed.
pub struct BarBuilder {
    symbol: String,
    spec: BarSpec,
    open: Option<f64>,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    vwap: f64,
    tick_count: usize,
    start_ms: u64,
    end_ms: u64,
    dollar_volume: f64,
}

impl BarBuilder {
    /// Creates a new [`BarBuilder`] for `symbol` with the given [`BarSpec`].
    pub fn new(symbol: impl Into<String>, spec: BarSpec) -> Self {
        Self {
            symbol: symbol.into(),
            spec,
            open: None,
            high: 0.0,
            low: f64::MAX,
            close: 0.0,
            volume: 0.0,
            vwap: 0.0,
            tick_count: 0,
            start_ms: 0,
            end_ms: 0,
            dollar_volume: 0.0,
        }
    }

    /// Returns `true` if no ticks have been accumulated yet.
    pub fn is_empty(&self) -> bool {
        self.open.is_none()
    }

    /// Push one tick and return `Some(Bar)` if the bar boundary was crossed.
    ///
    /// The triggering tick is included in the returned bar before the builder
    /// resets.
    pub fn push(&mut self, tick: &NormalizedTick) -> Option<Bar> {
        let price = tick.price.to_f64().unwrap_or(0.0);
        let qty = tick.quantity.to_f64().unwrap_or(0.0);
        let ts_ms = tick.exchange_ts_ms.unwrap_or(tick.received_at_ms);

        // Accumulate this tick.
        if self.open.is_none() {
            self.open = Some(price);
            self.high = price;
            self.low = price;
            self.start_ms = ts_ms;
        } else {
            if price > self.high { self.high = price; }
            if price < self.low { self.low = price; }
        }
        self.close = price;
        self.end_ms = ts_ms;

        // Online VWAP: vwap = (vwap * cum_vol + price * qty) / (cum_vol + qty)
        if self.volume + qty > 0.0 {
            self.vwap = (self.vwap * self.volume + price * qty) / (self.volume + qty);
        }
        self.volume += qty;
        self.dollar_volume += price * qty;
        self.tick_count += 1;

        // Check boundary.
        let should_close = self.check_boundary(ts_ms);

        if should_close {
            let bar = self.build();
            self.reset();
            Some(bar)
        } else {
            None
        }
    }

    /// Force-flush: emit any partially accumulated bar (e.g. end-of-session).
    ///
    /// Returns `None` if no ticks have been accumulated.
    pub fn flush(&mut self) -> Option<Bar> {
        if self.is_empty() {
            return None;
        }
        let bar = self.build();
        self.reset();
        Some(bar)
    }

    // ── private ───────────────────────────────────────────────────────────

    fn check_boundary(&self, ts_ms: u64) -> bool {
        match &self.spec {
            BarSpec::Tick(n) => self.tick_count >= *n,
            BarSpec::Volume(threshold) => self.volume >= *threshold,
            BarSpec::Dollar(threshold) => self.dollar_volume >= *threshold,
            BarSpec::Time(duration) => {
                let elapsed_ms = ts_ms.saturating_sub(self.start_ms);
                elapsed_ms >= duration.as_millis() as u64
            }
        }
    }

    fn build(&self) -> Bar {
        Bar {
            symbol: self.symbol.clone(),
            open: self.open.unwrap_or(self.close),
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            vwap: self.vwap,
            tick_count: self.tick_count,
            start_ms: self.start_ms,
            end_ms: self.end_ms,
        }
    }

    fn reset(&mut self) {
        self.open = None;
        self.high = 0.0;
        self.low = f64::MAX;
        self.close = 0.0;
        self.volume = 0.0;
        self.vwap = 0.0;
        self.tick_count = 0;
        self.start_ms = 0;
        self.end_ms = 0;
        self.dollar_volume = 0.0;
    }
}

// ─── BarStreamConfig ─────────────────────────────────────────────────────────

/// Maps symbols to their bar specifications.
#[derive(Debug, Clone, Default)]
pub struct BarStreamConfig {
    /// Symbol → BarSpec mapping.
    pub specs: Vec<(String, BarSpec)>,
}

impl BarStreamConfig {
    /// Creates a new empty config.
    pub fn new() -> Self {
        Self { specs: Vec::new() }
    }

    /// Registers a symbol with a bar spec.
    pub fn add(mut self, symbol: impl Into<String>, spec: BarSpec) -> Self {
        self.specs.push((symbol.into(), spec));
        self
    }
}

// ─── BarStream ────────────────────────────────────────────────────────────────

/// Routes [`NormalizedTick`]s to per-symbol [`BarBuilder`]s.
///
/// Each call to [`push_tick`](BarStream::push_tick) routes the tick to the
/// matching builder and returns any completed bars.
pub struct BarStream {
    builders: HashMap<String, BarBuilder>,
}

impl BarStream {
    /// Creates a new [`BarStream`] from the given config.
    pub fn new(config: &BarStreamConfig) -> Self {
        let mut builders = HashMap::new();
        for (symbol, spec) in &config.specs {
            builders.insert(symbol.clone(), BarBuilder::new(symbol, spec.clone()));
        }
        Self { builders }
    }

    /// Pushes a tick to the matching symbol's builder.
    ///
    /// Returns `Some(Bar)` when the bar closes, `None` otherwise.
    /// Ticks for unregistered symbols are silently ignored.
    pub fn push_tick(&mut self, tick: &NormalizedTick) -> Option<Bar> {
        self.builders.get_mut(&tick.symbol)?.push(tick)
    }

    /// Force-flushes all open bars (e.g. end-of-session).
    pub fn flush_all(&mut self) -> Vec<Bar> {
        self.builders.values_mut().filter_map(|b| b.flush()).collect()
    }

    /// Returns the list of registered symbols.
    pub fn symbols(&self) -> Vec<&str> {
        self.builders.keys().map(|s| s.as_str()).collect()
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::Exchange;
    use rust_decimal::Decimal;

    fn tick(symbol: &str, price: f64, qty: f64, ts_ms: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            price: Decimal::try_from(price).unwrap_or(Decimal::ZERO),
            quantity: Decimal::try_from(qty).unwrap_or(Decimal::ZERO),
            side: None,
            trade_id: None,
            exchange_ts_ms: Some(ts_ms),
            received_at_ms: ts_ms,
        }
    }

    // ── BarBuilder: Tick-based ────────────────────────────────────────────

    #[test]
    fn tick_bar_closes_after_n_ticks() {
        let mut b = BarBuilder::new("BTC", BarSpec::Tick(3));
        assert!(b.push(&tick("BTC", 100.0, 1.0, 1)).is_none());
        assert!(b.push(&tick("BTC", 101.0, 2.0, 2)).is_none());
        let bar = b.push(&tick("BTC", 102.0, 1.0, 3)).expect("bar on 3rd tick");
        assert_eq!(bar.tick_count, 3);
        assert!((bar.open - 100.0).abs() < 1e-9);
        assert!((bar.close - 102.0).abs() < 1e-9);
        assert!((bar.high - 102.0).abs() < 1e-9);
        assert!((bar.low - 100.0).abs() < 1e-9);
    }

    #[test]
    fn tick_bar_resets_after_close() {
        let mut b = BarBuilder::new("BTC", BarSpec::Tick(2));
        b.push(&tick("BTC", 100.0, 1.0, 1));
        b.push(&tick("BTC", 101.0, 1.0, 2)).expect("bar1");
        // Next two ticks form bar 2
        b.push(&tick("BTC", 200.0, 1.0, 3));
        let bar2 = b.push(&tick("BTC", 201.0, 1.0, 4)).expect("bar2");
        assert!((bar2.open - 200.0).abs() < 1e-9);
    }

    // ── BarBuilder: Volume-based ──────────────────────────────────────────

    #[test]
    fn volume_bar_closes_at_threshold() {
        let mut b = BarBuilder::new("ETH", BarSpec::Volume(10.0));
        assert!(b.push(&tick("ETH", 1000.0, 4.0, 1)).is_none());
        assert!(b.push(&tick("ETH", 1001.0, 4.0, 2)).is_none());
        let bar = b.push(&tick("ETH", 1002.0, 2.0, 3)).expect("volume bar");
        assert!((bar.volume - 10.0).abs() < 1e-9, "vol={}", bar.volume);
    }

    #[test]
    fn volume_bar_does_not_close_early() {
        let mut b = BarBuilder::new("X", BarSpec::Volume(10.0));
        assert!(b.push(&tick("X", 1.0, 3.0, 1)).is_none());
        assert!(b.push(&tick("X", 1.0, 3.0, 2)).is_none());
        // Only 6 accumulated, not 10
        assert!(b.is_empty() == false);
    }

    // ── BarBuilder: Dollar-based ──────────────────────────────────────────

    #[test]
    fn dollar_bar_closes_at_threshold() {
        // 100 * 5 = 500, then 100 * 6 = 600 → total 1100 ≥ 1000
        let mut b = BarBuilder::new("X", BarSpec::Dollar(1000.0));
        assert!(b.push(&tick("X", 100.0, 5.0, 1)).is_none()); // $500
        let bar = b.push(&tick("X", 100.0, 6.0, 2)).expect("dollar bar"); // $1100
        assert!((bar.volume - 11.0).abs() < 1e-9);
    }

    // ── BarBuilder: Time-based ────────────────────────────────────────────

    #[test]
    fn time_bar_closes_after_duration() {
        let mut b = BarBuilder::new("X", BarSpec::Time(Duration::from_secs(60)));
        b.push(&tick("X", 1.0, 1.0, 0));          // start at t=0
        b.push(&tick("X", 1.0, 1.0, 30_000));     // t=30s, no close
        let bar = b.push(&tick("X", 2.0, 1.0, 60_000)).expect("time bar"); // t=60s
        assert_eq!(bar.tick_count, 3);
        assert!((bar.close - 2.0).abs() < 1e-9);
    }

    #[test]
    fn time_bar_does_not_close_early() {
        let mut b = BarBuilder::new("X", BarSpec::Time(Duration::from_secs(60)));
        b.push(&tick("X", 1.0, 1.0, 0));
        assert!(b.push(&tick("X", 1.0, 1.0, 30_000)).is_none());
    }

    // ── VWAP correctness ──────────────────────────────────────────────────

    #[test]
    fn vwap_correct_two_ticks() {
        // VWAP = (100*1 + 200*1) / 2 = 150
        let mut b = BarBuilder::new("X", BarSpec::Tick(2));
        b.push(&tick("X", 100.0, 1.0, 1));
        let bar = b.push(&tick("X", 200.0, 1.0, 2)).expect("bar");
        assert!((bar.vwap - 150.0).abs() < 1e-9, "vwap={}", bar.vwap);
    }

    #[test]
    fn vwap_weighted_correctly() {
        // VWAP = (100*1 + 200*3) / 4 = 700/4 = 175
        let mut b = BarBuilder::new("X", BarSpec::Tick(2));
        b.push(&tick("X", 100.0, 1.0, 1));
        let bar = b.push(&tick("X", 200.0, 3.0, 2)).expect("bar");
        assert!((bar.vwap - 175.0).abs() < 1e-9, "vwap={}", bar.vwap);
    }

    #[test]
    fn vwap_single_tick_equals_price() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(1));
        let bar = b.push(&tick("X", 123.45, 5.0, 1)).expect("bar");
        assert!((bar.vwap - 123.45).abs() < 1e-9);
    }

    // ── OHLCV invariants ──────────────────────────────────────────────────

    #[test]
    fn high_gte_low() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(5));
        for i in 0..4 {
            b.push(&tick("X", (i as f64) * 10.0 + 100.0, 1.0, i as u64));
        }
        let bar = b.push(&tick("X", 50.0, 1.0, 5)).expect("bar");
        assert!(bar.high >= bar.low, "high={} low={}", bar.high, bar.low);
    }

    #[test]
    fn volume_is_sum_of_quantities() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(3));
        b.push(&tick("X", 1.0, 2.5, 1));
        b.push(&tick("X", 1.0, 1.5, 2));
        let bar = b.push(&tick("X", 1.0, 3.0, 3)).expect("bar");
        assert!((bar.volume - 7.0).abs() < 1e-9);
    }

    #[test]
    fn open_is_first_price() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(3));
        b.push(&tick("X", 42.0, 1.0, 1));
        b.push(&tick("X", 43.0, 1.0, 2));
        let bar = b.push(&tick("X", 44.0, 1.0, 3)).expect("bar");
        assert!((bar.open - 42.0).abs() < 1e-9);
    }

    #[test]
    fn close_is_last_price() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(3));
        b.push(&tick("X", 42.0, 1.0, 1));
        b.push(&tick("X", 43.0, 1.0, 2));
        let bar = b.push(&tick("X", 99.0, 1.0, 3)).expect("bar");
        assert!((bar.close - 99.0).abs() < 1e-9);
    }

    // ── flush ──────────────────────────────────────────────────────────────

    #[test]
    fn flush_emits_partial_bar() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(100));
        b.push(&tick("X", 1.0, 1.0, 1));
        b.push(&tick("X", 2.0, 1.0, 2));
        let bar = b.flush().expect("partial bar");
        assert_eq!(bar.tick_count, 2);
    }

    #[test]
    fn flush_empty_returns_none() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(100));
        assert!(b.flush().is_none());
    }

    #[test]
    fn flush_resets_builder() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(100));
        b.push(&tick("X", 1.0, 1.0, 1));
        b.flush();
        assert!(b.is_empty());
    }

    // ── BarStream ──────────────────────────────────────────────────────────

    #[test]
    fn bar_stream_routes_to_correct_builder() {
        let config = BarStreamConfig::new()
            .add("BTC", BarSpec::Tick(2))
            .add("ETH", BarSpec::Tick(3));
        let mut stream = BarStream::new(&config);

        stream.push_tick(&tick("BTC", 100.0, 1.0, 1));
        let bar = stream.push_tick(&tick("BTC", 101.0, 1.0, 2)).expect("BTC bar");
        assert_eq!(bar.symbol, "BTC");
    }

    #[test]
    fn bar_stream_ignores_unknown_symbol() {
        let config = BarStreamConfig::new().add("BTC", BarSpec::Tick(2));
        let mut stream = BarStream::new(&config);
        // ETH is not registered
        assert!(stream.push_tick(&tick("ETH", 100.0, 1.0, 1)).is_none());
    }

    #[test]
    fn bar_stream_flush_all_emits_partial_bars() {
        let config = BarStreamConfig::new()
            .add("BTC", BarSpec::Tick(100))
            .add("ETH", BarSpec::Tick(100));
        let mut stream = BarStream::new(&config);
        stream.push_tick(&tick("BTC", 100.0, 1.0, 1));
        stream.push_tick(&tick("ETH", 200.0, 1.0, 1));
        let bars = stream.flush_all();
        assert_eq!(bars.len(), 2);
    }

    #[test]
    fn bar_stream_symbols_returns_registered() {
        let config = BarStreamConfig::new()
            .add("AAPL", BarSpec::Tick(10))
            .add("MSFT", BarSpec::Volume(100.0));
        let stream = BarStream::new(&config);
        let mut syms = stream.symbols();
        syms.sort();
        assert_eq!(syms, vec!["AAPL", "MSFT"]);
    }

    #[test]
    fn bar_timestamps_set_correctly() {
        let mut b = BarBuilder::new("X", BarSpec::Tick(2));
        b.push(&tick("X", 1.0, 1.0, 1000));
        let bar = b.push(&tick("X", 2.0, 1.0, 2000)).expect("bar");
        assert_eq!(bar.start_ms, 1000);
        assert_eq!(bar.end_ms, 2000);
    }

    #[test]
    fn dollar_bar_volume_accumulated() {
        let mut b = BarBuilder::new("X", BarSpec::Dollar(500.0));
        b.push(&tick("X", 50.0, 4.0, 1)); // $200
        b.push(&tick("X", 50.0, 4.0, 2)); // $200 = $400 total
        let bar = b.push(&tick("X", 50.0, 2.0, 3)).expect("bar"); // $100 → $500
        assert!((bar.volume - 10.0).abs() < 1e-9);
    }

    #[test]
    fn time_bar_start_ms_is_first_tick() {
        let mut b = BarBuilder::new("X", BarSpec::Time(Duration::from_secs(60)));
        b.push(&tick("X", 1.0, 1.0, 5000));
        let bar = b.push(&tick("X", 2.0, 1.0, 65_000)).expect("bar");
        assert_eq!(bar.start_ms, 5000);
    }
}
