[![CI](https://github.com/Mattbusel/fin-stream/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/fin-stream/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/fin-stream.svg)](https://crates.io/crates/fin-stream)
[![docs.rs](https://img.shields.io/docsrs/fin-stream)](https://docs.rs/fin-stream)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![codecov](https://codecov.io/gh/Mattbusel/fin-stream/branch/main/graph/badge.svg)](https://codecov.io/gh/Mattbusel/fin-stream)
[![Rust MSRV](https://img.shields.io/badge/MSRV-1.75-orange.svg)](https://blog.rust-lang.org/2023/12/28/Rust-1.75.0.html)

# fin-stream

Lock-free streaming primitives for real-time financial market data. Provides a
composable ingestion pipeline from raw exchange ticks to normalized, transformed
features ready for downstream models or trade execution. Built on Tokio. Targets
100 K+ ticks/second throughput with zero heap allocation on the fast path.

## What Is Included

| Module | Purpose | Key types |
|---|---|---|
| `ws` | WebSocket connection lifecycle with exponential-backoff reconnect and backpressure | `WsManager`, `ConnectionConfig`, `ReconnectPolicy` |
| `tick` | Convert raw exchange payloads (Binance/Coinbase/Alpaca/Polygon) into a single canonical form | `RawTick`, `NormalizedTick`, `Exchange`, `TradeSide`, `TickNormalizer` |
| `ring` | Lock-free SPSC ring buffer: zero-allocation hot path between normalizer and consumers | `SpscRing<T, N>`, `SpscProducer`, `SpscConsumer` |
| `book` | Incremental order book delta streaming with snapshot reset and crossed-book detection | `OrderBook`, `BookDelta`, `BookSide`, `PriceLevel` |
| `ohlcv` | Bar construction at any `Seconds / Minutes / Hours` timeframe with optional gap-fill bars | `OhlcvAggregator`, `OhlcvBar`, `Timeframe` |
| `health` | Per-feed staleness detection with configurable thresholds and a circuit-breaker | `HealthMonitor`, `FeedHealth`, `HealthStatus` |
| `session` | Trading-status classification (Open / Extended / Closed) for US Equity, Crypto, Forex | `SessionAwareness`, `MarketSession`, `TradingStatus` |
| `norm` | Rolling min-max and z-score normalizers for streaming observations | `MinMaxNormalizer`, `ZScoreNormalizer` |
| `lorentz` | Lorentz spacetime transforms for feature engineering on price-time coordinates | `LorentzTransform`, `SpacetimePoint` |
| `error` | Unified typed error hierarchy covering every pipeline failure mode | `StreamError` |

## Design Principles

1. **Never panic on valid production inputs.** Every fallible operation returns
   `Result<_, StreamError>`. The only intentional panic is `MinMaxNormalizer::new(0)`,
   which is an API misuse guard documented in the function-level doc comment.
2. **Zero heap allocation on the hot path.** `SpscRing<T, N>` is a const-generic
   array; `push`/`pop` never call `malloc`. `NormalizedTick` is stack-allocated.
3. **Exact decimal arithmetic for prices.** All price and quantity fields use
   `rust_decimal::Decimal`, never `f64`. `f64` is used only for the dimensionless
   `beta`/`gamma` Lorentz parameters and the `f64` normalizer observations.
4. **Thread-safety where needed.** `HealthMonitor` uses `DashMap` for concurrent
   feed updates. `OrderBook` is `Send + Sync`. `SpscRing` splits into producer/consumer
   halves that are individually `Send`.
5. **No unsafe code.** `#![forbid(unsafe_code)]` is active in `lib.rs`. The SPSC
   ring buffer uses `UnsafeCell` with a documented safety invariant, gated behind a
   safe public API.

## Architecture

```
Tick Source (WebSocket / simulation)
            |
            v
   [ WsManager ]           -- connection lifecycle, exponential-backoff reconnect
            |
            v
   [ TickNormalizer ]      -- raw JSON payload -> NormalizedTick (all exchanges)
            |
            v
   [ SPSC Ring Buffer ]    -- lock-free O(1) push/pop, zero allocation hot path
            |
            v
   [ OHLCV Aggregator ]    -- streaming bar construction at any timeframe
            |
            v
   [ MinMax / ZScore Normalizer ]  -- rolling-window coordinate normalization
            |
            +---> [ Lorentz Transform ]   -- relativistic spacetime boost for features
            |
            v
   Downstream (ML model | trade signal engine | order management)

   Parallel paths:
   [ OrderBook ]           -- delta streaming, snapshot reset, crossed-book guard
   [ HealthMonitor ]       -- per-feed staleness detection, circuit-breaker
   [ SessionAwareness ]    -- Open / Extended / Closed classification
```

## Mathematical Definitions

### Min-Max Normalization

Given a rolling window of `W` observations `x_1, ..., x_W` with minimum `m` and
maximum `M`, the normalized value of a new sample `x` is:

```
x_norm = (x - m) / (M - m)    when M != m
x_norm = 0.0                  when M == m  (degenerate; all window values identical)
```

The result is clamped to `[0.0, 1.0]`. This ensures that observations falling
outside the current window range are mapped to the boundary rather than outside it.

### Z-Score Normalization

Given a rolling window of `W` observations with mean `μ` and standard deviation `σ`:

```
z = (x - μ) / σ    when σ != 0
z = 0.0            when σ == 0  (degenerate; all window values identical)
```

`ZScoreNormalizer` also provides IQR, percentile rank, variance, EMA of z-scores,
and rolling mean change across the window.

### Lorentz Transform

The `LorentzTransform` applies the special-relativistic boost with velocity
parameter `beta = v/c` (speed of light normalized to `c = 1`):

```
t' = gamma * (t - beta * x)
x' = gamma * (x - beta * t)

where  beta  = v/c            (0 <= beta < 1, dimensionless drift velocity)
       gamma = 1 / sqrt(1 - beta^2)   (Lorentz factor, always >= 1)
```

The inverse transform is:

```
t  = gamma * (t' + beta * x')
x  = gamma * (x' + beta * t')
```

The spacetime interval `s^2 = t^2 - x^2` is invariant under the transform.
`beta = 0` gives the identity (`gamma = 1`). `beta >= 1` is invalid (gamma is
undefined) and is rejected at construction time with `StreamError::LorentzConfigError`.

**Financial interpretation.** `t` is elapsed time normalized to a convenient
scale. `x` is a normalized log-price or price coordinate. The boost maps the
price-time plane along Lorentz hyperbolas. Certain microstructure signals that
appear curved in the untransformed frame can appear as straight lines in a
suitably boosted frame, simplifying downstream linear models.

### OHLCV Invariants

Every completed `OhlcvBar` satisfies:

| Invariant | Expression |
|---|---|
| High is largest | `high >= max(open, close)` |
| Low is smallest | `low <= min(open, close)` |
| Valid ordering | `high >= low` |
| Volume non-negative | `volume >= 0` |

### Order Book Guarantees

| Property | Guarantee |
|---|---|
| No crossed book | Any delta that would produce `best_bid >= best_ask` is rejected with `StreamError::BookCrossed`; the book is not mutated |
| Sequence gap detection | If a delta carries a sequence number that is not exactly `last_sequence + 1`, the apply returns `StreamError::BookReconstructionFailed` |
| Zero quantity removes level | A delta with `quantity = 0` removes the price level entirely |

### Reconnect Backoff

`ReconnectPolicy::backoff_for_attempt(n)` returns:

```
backoff(n) = min(initial_backoff * multiplier^n, max_backoff)
```

`multiplier` must be `>= 1.0` and `max_attempts` must be `> 0`; both are validated
at construction time.

## Performance Characteristics

| Metric | Value |
|---|---|
| SPSC push/pop latency | O(1), single cache-line access |
| SPSC throughput | >100 K ticks/second (zero allocation) |
| OHLCV feed per tick | O(1) |
| Normalization update | O(1) amortized; O(W) after window eviction |
| Lorentz transform | O(1), two multiplications per coordinate |
| Ring buffer memory | N * sizeof(T) bytes (N is const generic) |

## Quickstart

### Normalize a Binance tick and aggregate OHLCV

```rust
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use serde_json::json;

fn main() -> Result<(), fin_stream::StreamError> {
    let normalizer = TickNormalizer::new();
    let mut agg = OhlcvAggregator::new("BTCUSDT", Timeframe::Minutes(1));

    let raw = RawTick::new(
        Exchange::Binance,
        "BTCUSDT",
        json!({ "p": "65000.50", "q": "0.002", "m": false, "t": 1u64, "T": 1_700_000_000_000u64 }),
    );
    let tick = normalizer.normalize(raw)?;
    let completed_bars = agg.feed(&tick)?;

    for bar in completed_bars {
        println!("{}: close={}", bar.bar_start_ms, bar.close);
    }
    Ok(())
}
```

### SPSC ring buffer pipeline

```rust
use fin_stream::ring::SpscRing;
use fin_stream::tick::{Exchange, RawTick, TickNormalizer, NormalizedTick};
use serde_json::json;

fn main() -> Result<(), fin_stream::StreamError> {
    let ring: SpscRing<NormalizedTick, 1024> = SpscRing::new();
    let (prod, cons) = ring.split();

    // Producer thread
    let normalizer = TickNormalizer::new();
    let raw = RawTick::new(
        Exchange::Coinbase,
        "BTC-USD",
        json!({ "price": "65001.00", "size": "0.01", "side": "buy", "trade_id": "abc" }),
    );
    let tick = normalizer.normalize(raw)?;
    prod.push(tick)?;

    // Consumer thread
    while let Ok(t) = cons.pop() {
        println!("received tick: {} @ {}", t.symbol, t.price);
    }
    Ok(())
}
```

### Min-max normalization of closing prices

```rust
use fin_stream::norm::MinMaxNormalizer;

fn main() -> Result<(), fin_stream::StreamError> {
    let mut norm = MinMaxNormalizer::new(20);

    let closes = vec![100.0, 102.0, 98.0, 105.0, 103.0];
    for &c in &closes {
        norm.update(c);
    }

    let v = norm.normalize(103.0)?;
    println!("normalized: {v:.4}");  // a value in [0.0, 1.0]
    Ok(())
}
```

### Z-score normalization with analytics

```rust
use fin_stream::norm::ZScoreNormalizer;

fn main() -> Result<(), fin_stream::StreamError> {
    let mut z = ZScoreNormalizer::new(30);

    for v in [100.0, 102.5, 99.0, 103.0, 101.5] {
        z.update(v);
    }

    let score = z.normalize(104.0)?;
    println!("z-score: {score:.4}");
    println!("positive z count: {}", z.count_positive_z_scores());
    println!("mean stable: {}", z.is_mean_stable(0.5));
    Ok(())
}
```

### Lorentz feature engineering

```rust
use fin_stream::lorentz::{LorentzTransform, SpacetimePoint};

fn main() -> Result<(), fin_stream::StreamError> {
    let lt = LorentzTransform::new(0.3)?; // beta = 0.3
    let p = SpacetimePoint::new(1.0, 0.5);
    let boosted = lt.transform(p);
    println!("t'={:.4} x'={:.4}", boosted.t, boosted.x);

    // Round-trip
    let recovered = lt.inverse_transform(boosted);
    assert!((recovered.t - p.t).abs() < 1e-10);
    Ok(())
}
```

### Order book delta streaming

```rust
use fin_stream::book::{BookDelta, BookSide, OrderBook};
use rust_decimal_macros::dec;

fn main() -> Result<(), fin_stream::StreamError> {
    let mut book = OrderBook::new("BTC-USD");
    book.apply(BookDelta::new("BTC-USD", BookSide::Bid, dec!(50000), dec!(1)).with_sequence(1))?;
    book.apply(BookDelta::new("BTC-USD", BookSide::Ask, dec!(50001), dec!(2)).with_sequence(2))?;

    println!("mid: {}", book.mid_price().unwrap());
    println!("spread: {}", book.spread().unwrap());
    println!("notional: {}", book.total_notional_both_sides());
    Ok(())
}
```

### Feed health monitoring with circuit breaker

```rust
use fin_stream::health::HealthMonitor;

fn main() -> Result<(), fin_stream::StreamError> {
    let monitor = HealthMonitor::new(5_000)          // 5 s stale threshold
        .with_circuit_breaker_threshold(3);           // open after 3 consecutive stale checks

    monitor.register("BTC-USD", None);
    monitor.heartbeat("BTC-USD", 1_000_000)?;

    let stale_errors = monitor.check_all(1_010_000); // 10 s later — stale
    for e in stale_errors {
        eprintln!("stale: {e}");
    }

    println!("circuit open: {}", monitor.is_circuit_open("BTC-USD"));
    println!("ratio healthy: {:.2}", monitor.ratio_healthy());
    Ok(())
}
```

### Session classification

```rust
use fin_stream::session::{MarketSession, SessionAwareness};

fn main() -> Result<(), fin_stream::StreamError> {
    let sa = SessionAwareness::new(MarketSession::UsEquity);
    let status = sa.status(1_700_000_000_000)?; // some UTC ms timestamp
    println!("US equity status: {status:?}");
    println!("session name: {}", sa.session_name());
    Ok(())
}
```

## API Reference

### `tick` module

```rust
// Parse an exchange identifier string.
Exchange::from_str("binance") -> Result<Exchange, StreamError>
Exchange::Display              // "Binance" / "Coinbase" / "Alpaca" / "Polygon"

// Construct a raw tick (system clock stamp applied automatically).
RawTick::new(exchange: Exchange, symbol: impl Into<String>, payload: serde_json::Value) -> RawTick

// Normalize a raw tick into a canonical representation.
TickNormalizer::new() -> TickNormalizer
TickNormalizer::normalize(&self, raw: RawTick) -> Result<NormalizedTick, StreamError>

// NormalizedTick query methods
NormalizedTick::is_above_price(&self, reference: Decimal) -> bool
NormalizedTick::is_below_price(&self, reference: Decimal) -> bool
NormalizedTick::is_at_price(&self, target: Decimal) -> bool
NormalizedTick::price_change_from(&self, reference: Decimal) -> Decimal
NormalizedTick::quantity_above(&self, threshold: Decimal) -> bool
NormalizedTick::is_round_number(&self, step: Decimal) -> bool
NormalizedTick::is_market_open_tick(&self, session_start_ms: u64, session_end_ms: u64) -> bool
NormalizedTick::signed_quantity(&self) -> Decimal   // +qty Buy, -qty Sell, 0 Unknown
NormalizedTick::as_price_level(&self) -> (Decimal, Decimal)  // (price, quantity)
NormalizedTick::is_buy(&self) -> bool
NormalizedTick::is_sell(&self) -> bool
NormalizedTick::age_ms(&self, now_ms: u64) -> u64
NormalizedTick::has_exchange_ts(&self) -> bool
NormalizedTick::exchange_latency_ms(&self, now_ms: u64) -> Option<u64>
NormalizedTick::price_change_pct(&self, reference: Decimal) -> Option<f64>
NormalizedTick::is_same_symbol_as(&self, other: &NormalizedTick) -> bool
NormalizedTick::side_str(&self) -> &'static str
NormalizedTick::is_large_tick(&self, threshold: Decimal) -> bool
NormalizedTick::is_zero_quantity(&self) -> bool
NormalizedTick::dollar_value(&self) -> Decimal      // price * quantity
NormalizedTick::vwap(&self, total_volume: Decimal, cumulative_pv: Decimal) -> Option<Decimal>
```

### `ring` module

```rust
// Create a const-generic SPSC ring buffer.
SpscRing::<T, N>::new() -> SpscRing<T, N>          // N slots, zero allocation

// Split into thread-safe producer/consumer halves.
SpscRing::split(self) -> (SpscProducer<T, N>, SpscConsumer<T, N>)

SpscProducer::push(&self, value: T) -> Result<(), StreamError>  // StreamError::RingBufferFull on overflow
SpscConsumer::pop(&self) -> Result<T, StreamError>              // StreamError::RingBufferEmpty on underflow
SpscRing::len(&self) -> usize                                   // items currently queued
SpscRing::is_empty(&self) -> bool
SpscRing::capacity(&self) -> usize                              // always N

// Analytics on a populated ring (clone-based reads on initialized slots)
SpscRing::sum_cloned(&self) -> T                where T: Clone + Sum + Default
SpscRing::average_cloned(&self) -> Option<f64>  where T: Clone + Into<f64>
SpscRing::peek_nth(&self, n: usize) -> Option<T> where T: Clone   // 0 = oldest
SpscRing::contains_cloned(&self, value: &T) -> bool where T: Clone + PartialEq
SpscRing::max_cloned_by<F, K>(&self, key: F) -> Option<T>  where F: Fn(&T) -> K, K: Ord
SpscRing::min_cloned_by<F, K>(&self, key: F) -> Option<T>  where F: Fn(&T) -> K, K: Ord
SpscRing::to_vec_sorted(&self) -> Vec<T>        where T: Clone + Ord
SpscRing::to_vec_cloned(&self) -> Vec<T>        where T: Clone
SpscRing::first(&self) -> Option<T>             where T: Clone   // oldest item
SpscRing::drain_into(&self, dest: &mut Vec<T>)  where T: Clone
```

### `book` module

```rust
// Construct a delta (sequence number optional).
BookDelta::new(symbol, side: BookSide, price: Decimal, quantity: Decimal) -> BookDelta
BookDelta::with_sequence(self, seq: u64) -> BookDelta

// Apply deltas and query the book.
OrderBook::new(symbol: impl Into<String>) -> OrderBook
OrderBook::apply(&mut self, delta: BookDelta) -> Result<(), StreamError>
OrderBook::reset(&mut self, bids: Vec<PriceLevel>, asks: Vec<PriceLevel>)
OrderBook::best_bid(&self) -> Option<Decimal>
OrderBook::best_ask(&self) -> Option<Decimal>
OrderBook::mid_price(&self) -> Option<Decimal>
OrderBook::spread(&self) -> Option<Decimal>
OrderBook::spread_bps(&self) -> Option<Decimal>
OrderBook::top_bids(&self, n: usize) -> Vec<PriceLevel>
OrderBook::top_asks(&self, n: usize) -> Vec<PriceLevel>
OrderBook::total_bid_volume(&self) -> Decimal
OrderBook::total_ask_volume(&self) -> Decimal
OrderBook::bid_ask_volume_ratio(&self) -> Option<f64>
OrderBook::depth_imbalance(&self) -> Option<f64>
OrderBook::weighted_mid_price(&self) -> Option<Decimal>
OrderBook::bid_levels_above(&self, price: Decimal) -> usize
OrderBook::ask_levels_below(&self, price: Decimal) -> usize
OrderBook::bid_volume_at_price(&self, price: Decimal) -> Option<Decimal>
OrderBook::ask_volume_at_price(&self, price: Decimal) -> Option<Decimal>
OrderBook::cumulative_bid_volume(&self, n: usize) -> Decimal
OrderBook::cumulative_ask_volume(&self, n: usize) -> Decimal
OrderBook::is_within_spread(&self, price: Decimal) -> bool
OrderBook::bid_wall(&self, threshold: Decimal) -> Option<Decimal>
OrderBook::ask_wall(&self, threshold: Decimal) -> Option<Decimal>

// Extended book analytics (added rounds 36–40)
OrderBook::total_value_at_level(&self, side: BookSide, price: Decimal) -> Option<Decimal>
OrderBook::ask_volume_above(&self, price: Decimal) -> Decimal
OrderBook::bid_volume_below(&self, price: Decimal) -> Decimal
OrderBook::total_notional_both_sides(&self) -> Decimal
OrderBook::price_level_exists(&self, side: BookSide, price: Decimal) -> bool
OrderBook::level_count_both_sides(&self) -> usize
OrderBook::ask_price_at_rank(&self, n: usize) -> Option<Decimal>  // 0 = best ask
OrderBook::bid_price_at_rank(&self, n: usize) -> Option<Decimal>  // 0 = best bid
```

### `ohlcv` module

```rust
// Construct an aggregator.
OhlcvAggregator::new(symbol: impl Into<String>, timeframe: Timeframe) -> OhlcvAggregator
OhlcvAggregator::with_emit_empty_bars(self, emit: bool) -> OhlcvAggregator

// Feed ticks; returns completed bars (may be empty or multiple on gaps).
OhlcvAggregator::feed(&mut self, tick: &NormalizedTick) -> Result<Vec<OhlcvBar>, StreamError>

// Bar boundary alignment.
Timeframe::duration_ms(self) -> u64
Timeframe::bar_start_ms(self, ts_ms: u64) -> u64

// OhlcvBar computed properties
OhlcvBar::true_range(&self, prev_close: Decimal) -> Decimal
OhlcvBar::body_ratio(&self) -> Option<f64>          // body / range
OhlcvBar::upper_shadow(&self) -> Decimal
OhlcvBar::lower_shadow(&self) -> Decimal
OhlcvBar::hlc3(&self) -> Decimal                    // (high + low + close) / 3
OhlcvBar::ohlc4(&self) -> Decimal                   // (open + high + low + close) / 4
OhlcvBar::typical_price(&self) -> Decimal           // hlc3 alias
OhlcvBar::weighted_close(&self) -> Decimal          // (high + low + 2*close) / 4
OhlcvBar::close_location_value(&self) -> Option<f64>  // (close - low) / (high - low)
OhlcvBar::is_bullish(&self) -> bool
OhlcvBar::is_bearish(&self) -> bool
OhlcvBar::is_doji(&self, threshold: Decimal) -> bool
OhlcvBar::is_marubozu(&self, wick_threshold: Decimal) -> bool
OhlcvBar::is_spinning_top(&self, body_threshold: Decimal) -> bool
OhlcvBar::is_shooting_star(&self) -> bool
OhlcvBar::is_inside_bar(&self, prev: &OhlcvBar) -> bool
OhlcvBar::is_outside_bar(&self, prev: &OhlcvBar) -> bool
OhlcvBar::is_harami(&self, prev: &OhlcvBar) -> bool
OhlcvBar::is_engulfing(&self, prev: &OhlcvBar) -> bool
OhlcvBar::has_upper_wick(&self) -> bool
OhlcvBar::has_lower_wick(&self) -> bool
OhlcvBar::volume_notional(&self) -> Decimal         // volume * close
OhlcvBar::range_pct(&self) -> Option<f64>           // (high - low) / open
OhlcvBar::price_change_pct(&self, prev: &OhlcvBar) -> Option<f64>
OhlcvBar::body_size(&self) -> Decimal               // |close - open|

// Analytics added in rounds 35–41
OhlcvBar::mean_volume(bars: &[OhlcvBar]) -> Option<Decimal>   // static
OhlcvBar::vwap_deviation(&self) -> Option<f64>
OhlcvBar::relative_volume(&self, avg_volume: Decimal) -> Option<f64>
OhlcvBar::intraday_reversal(&self, prev: &OhlcvBar) -> bool
OhlcvBar::high_close_ratio(&self) -> Option<f64>
OhlcvBar::lower_shadow_pct(&self) -> Option<f64>
OhlcvBar::open_close_ratio(&self) -> Option<f64>
OhlcvBar::is_wide_range_bar(&self, threshold: Decimal) -> bool
OhlcvBar::close_to_low_ratio(&self) -> Option<f64>   // (close - low) / (high - low)
OhlcvBar::volume_per_trade(&self) -> Option<Decimal>
OhlcvBar::price_range_overlap(&self, other: &OhlcvBar) -> bool
OhlcvBar::bar_height_pct(&self) -> Option<f64>        // (high - low) / open
OhlcvBar::is_bullish_engulfing(&self, prev: &OhlcvBar) -> bool
OhlcvBar::close_gap(&self, prev: &OhlcvBar) -> Decimal  // open - prev.close
OhlcvBar::close_above_midpoint(&self) -> bool
OhlcvBar::close_momentum(&self, prev: &OhlcvBar) -> Decimal  // close - prev.close
OhlcvBar::bar_range(&self) -> Decimal                 // high - low
```

### `norm` module

```rust
// Min-max rolling normalizer
MinMaxNormalizer::new(window_size: usize) -> MinMaxNormalizer  // panics if window_size == 0
MinMaxNormalizer::update(&mut self, value: f64)                // O(1) amortized
MinMaxNormalizer::normalize(&mut self, value: f64) -> Result<f64, StreamError>  // [0.0, 1.0]
MinMaxNormalizer::min_max(&mut self) -> Option<(f64, f64)>
MinMaxNormalizer::reset(&mut self)
MinMaxNormalizer::len(&self) -> usize
MinMaxNormalizer::is_empty(&self) -> bool
MinMaxNormalizer::window_size(&self) -> usize
MinMaxNormalizer::count_above(&self, threshold: f64) -> usize
MinMaxNormalizer::normalized_range(&mut self) -> Option<f64>   // (max - min) / max
MinMaxNormalizer::fraction_above_mid(&mut self) -> Option<f64> // fraction of window above midpoint

// Z-score rolling normalizer
ZScoreNormalizer::new(window_size: usize) -> ZScoreNormalizer
ZScoreNormalizer::update(&mut self, value: f64)
ZScoreNormalizer::normalize(&mut self, value: f64) -> Result<f64, StreamError>
ZScoreNormalizer::mean(&self) -> Option<f64>
ZScoreNormalizer::std_dev(&self) -> Option<f64>
ZScoreNormalizer::variance_f64(&self) -> Option<f64>
ZScoreNormalizer::len(&self) -> usize
ZScoreNormalizer::is_empty(&self) -> bool
ZScoreNormalizer::window_size(&self) -> usize
ZScoreNormalizer::interquartile_range(&self) -> Option<f64>
ZScoreNormalizer::percentile_rank(&self, value: f64) -> Option<f64>
ZScoreNormalizer::ema_of_z_scores(&self, alpha: f64) -> Option<f64>
ZScoreNormalizer::trim_outliers(&self, z_threshold: f64) -> Vec<f64>
ZScoreNormalizer::is_outlier(&self, value: f64, z_threshold: f64) -> bool
ZScoreNormalizer::clamp_to_window(&self, value: f64) -> f64
ZScoreNormalizer::rolling_mean_change(&self) -> Option<f64>   // second_half_mean - first_half_mean
ZScoreNormalizer::count_positive_z_scores(&self) -> usize
ZScoreNormalizer::above_threshold_count(&self, z_threshold: f64) -> usize
ZScoreNormalizer::window_span_f64(&self) -> Option<f64>       // max - min over window
ZScoreNormalizer::is_mean_stable(&self, threshold: f64) -> bool
```

### `lorentz` module

```rust
LorentzTransform::new(beta: f64) -> Result<LorentzTransform, StreamError>  // beta in [0, 1)
LorentzTransform::beta(&self) -> f64
LorentzTransform::gamma(&self) -> f64
LorentzTransform::transform(&self, p: SpacetimePoint) -> SpacetimePoint
LorentzTransform::inverse_transform(&self, p: SpacetimePoint) -> SpacetimePoint
LorentzTransform::transform_batch(&self, points: &[SpacetimePoint]) -> Vec<SpacetimePoint>
LorentzTransform::dilate_time(&self, t: f64) -> f64        // t' = gamma * t (x = 0)
LorentzTransform::contract_length(&self, x: f64) -> f64   // x' = x / gamma (t = 0)
LorentzTransform::spacetime_interval(&self, p: SpacetimePoint) -> f64  // t^2 - x^2
LorentzTransform::rapidity(&self) -> f64                   // atanh(beta)
LorentzTransform::relativistic_momentum(&self, mass: f64) -> f64  // gamma * mass * beta
LorentzTransform::four_momentum(&self, mass: f64) -> (f64, f64)   // (E, p)
LorentzTransform::velocity_addition(&self, other_beta: f64) -> Result<f64, StreamError>
LorentzTransform::proper_acceleration(&self, coordinate_accel: f64) -> f64
LorentzTransform::proper_length(&self, coordinate_length: f64) -> f64
LorentzTransform::time_dilation_ms(&self, coordinate_time_ms: f64) -> f64
LorentzTransform::boost_composition(&self, other: &LorentzTransform) -> Result<LorentzTransform, StreamError>
LorentzTransform::beta_times_gamma(&self) -> f64           // β·γ
LorentzTransform::energy_momentum_invariant(&self, mass: f64) -> f64  // E² - p² = m²

SpacetimePoint::new(t: f64, x: f64) -> SpacetimePoint
SpacetimePoint { t: f64, x: f64 }  // public fields
```

### `health` module

```rust
HealthMonitor::new(default_stale_threshold_ms: u64) -> HealthMonitor
HealthMonitor::with_circuit_breaker_threshold(self, threshold: u32) -> HealthMonitor
HealthMonitor::register(&self, feed_id: impl Into<String>, stale_threshold_ms: Option<u64>)
HealthMonitor::register_many(&self, feed_ids: &[impl AsRef<str>])
HealthMonitor::register_batch(&self, feeds: &[(impl AsRef<str>, u64)])  // per-feed custom thresholds
HealthMonitor::heartbeat(&self, feed_id: &str, ts_ms: u64) -> Result<(), StreamError>
HealthMonitor::check_all(&self, now_ms: u64) -> Vec<StreamError>
HealthMonitor::is_circuit_open(&self, feed_id: &str) -> bool
HealthMonitor::get(&self, feed_id: &str) -> Option<FeedHealth>
HealthMonitor::all_feeds(&self) -> Vec<FeedHealth>
HealthMonitor::feed_count(&self) -> usize
HealthMonitor::healthy_count(&self) -> usize
HealthMonitor::stale_count(&self) -> usize
HealthMonitor::degraded_count(&self) -> usize
HealthMonitor::healthy_feed_ids(&self) -> Vec<String>
HealthMonitor::unknown_feed_ids(&self) -> Vec<String>      // feeds with no heartbeat yet
HealthMonitor::feeds_needing_check(&self) -> Vec<String>   // sorted non-Healthy feed IDs
HealthMonitor::ratio_healthy(&self) -> f64                 // healthy / total
HealthMonitor::total_tick_count(&self) -> u64
HealthMonitor::last_updated_feed_id(&self) -> Option<String>
HealthMonitor::is_any_stale(&self) -> bool
HealthMonitor::min_healthy_age_ms(&self, now_ms: u64) -> Option<u64>

FeedHealth::elapsed_ms(&self, now_ms: u64) -> Option<u64>
```

### `session` module

```rust
SessionAwareness::new(session: MarketSession) -> SessionAwareness
SessionAwareness::status(&self, utc_ms: u64) -> Result<TradingStatus, StreamError>
SessionAwareness::is_active(&self, utc_ms: u64) -> bool          // Open or Extended
SessionAwareness::remaining_ms(&self, utc_ms: u64) -> Option<u64>
SessionAwareness::time_until_close(&self, utc_ms: u64) -> Option<u64>
SessionAwareness::minutes_until_close(&self, utc_ms: u64) -> Option<f64>
SessionAwareness::session_duration(&self) -> u64
SessionAwareness::is_pre_market(&self, utc_ms: u64) -> bool
SessionAwareness::is_after_hours(&self, utc_ms: u64) -> bool
SessionAwareness::session_label(&self) -> &'static str
SessionAwareness::session_name(&self) -> &'static str
SessionAwareness::seconds_until_open(&self, utc_ms: u64) -> f64
SessionAwareness::is_closing_bell_minute(&self, utc_ms: u64) -> bool
SessionAwareness::is_expiry_week(&self, date: chrono::NaiveDate) -> bool
SessionAwareness::is_fomc_blackout_window(&self, date: chrono::NaiveDate) -> bool
SessionAwareness::is_market_holiday_adjacent(&self, date: chrono::NaiveDate) -> bool
SessionAwareness::day_of_week_name(&self, date: chrono::NaiveDate) -> &'static str

is_tradeable(session: MarketSession, utc_ms: u64) -> Result<bool, StreamError>
```

### `ws` module

```rust
ReconnectPolicy::new(
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    multiplier: f64,
) -> Result<ReconnectPolicy, StreamError>
ReconnectPolicy::default() -> ReconnectPolicy   // 10 attempts, 500ms initial, 30s cap, 2x multiplier
ReconnectPolicy::backoff_for_attempt(&self, attempt: u32) -> Duration

ConnectionConfig::new(url: impl Into<String>, channel_capacity: usize) -> Result<ConnectionConfig, StreamError>
ConnectionConfig::with_reconnect_policy(self, policy: ReconnectPolicy) -> ConnectionConfig
ConnectionConfig::with_ping_interval(self, interval: Duration) -> ConnectionConfig

WsManager::new(config: ConnectionConfig) -> WsManager
WsManager::connect(&mut self) -> Result<(), StreamError>
WsManager::disconnect(&mut self)
WsManager::is_connected(&self) -> bool
WsManager::next_reconnect_backoff(&mut self) -> Result<Duration, StreamError>
```

## Supported Exchanges

| Exchange | Adapter | Status | Wire-format fields used |
|---|---|---|---|
| Binance | `Exchange::Binance` | Stable | `p` (price), `q` (qty), `m` (maker/taker), `t` (trade id), `T` (exchange ts) |
| Coinbase | `Exchange::Coinbase` | Stable | `price`, `size`, `side`, `trade_id` |
| Alpaca | `Exchange::Alpaca` | Stable | `p` (price), `s` (size), `i` (trade id) |
| Polygon | `Exchange::Polygon` | Stable | `p` (price), `s` (size), `i` (trade id), `t` (exchange ts) |

All four adapters are covered by unit and integration tests. To add a new exchange,
see the **Contributing** section below.

## Precision and Accuracy Notes

- **Price and quantity fields** use `rust_decimal::Decimal` — a 96-bit integer
  mantissa with a power-of-10 exponent. This guarantees exact representation of
  any finite decimal number with up to 28 significant digits. There is no
  floating-point rounding error on price arithmetic.
- **Normalization (`f64`)** uses IEEE 754 double precision. The error bound on
  `normalize(x)` is roughly `2 * machine_epsilon * |x|` in the worst case.
  For typical price ranges this is well below any practical threshold.
- **Lorentz parameters (`f64`)** use `f64` throughout. The round-trip error of
  `inverse_transform(transform(p))` is bounded by `4 * gamma^2 * machine_epsilon`.
  For `beta <= 0.9`, `gamma <= ~2.3` and the round-trip error is `< 1e-13`.
- **Bar aggregation** accumulates volume with `Decimal` addition. OHLC fields
  carry the exact decimal values from normalized ticks with no intermediate rounding.

## Error Handling

All fallible operations return `Result<_, StreamError>`. `StreamError` variants:

| Variant | Subsystem | When emitted |
|---|---|---|
| `ConnectionFailed` | ws | WebSocket connection attempt rejected |
| `Disconnected` | ws | Live connection dropped unexpectedly |
| `ReconnectExhausted` | ws | All reconnect attempts consumed |
| `Backpressure` | ws / ring | Downstream channel or ring buffer is full |
| `ParseError` | tick | Tick deserialization failed (missing field, invalid decimal) |
| `UnknownExchange` | tick | Exchange identifier string not recognized |
| `InvalidTick` | tick | Tick failed validation (negative price, zero quantity) |
| `BookReconstructionFailed` | book | Delta applied to wrong symbol, or sequence gap |
| `BookCrossed` | book | Order book bid >= ask after applying a delta |
| `StaleFeed` | health | Feed has not produced data within staleness threshold |
| `AggregationError` | ohlcv | Wrong symbol or zero-duration timeframe |
| `NormalizationError` | norm | `normalize()` called before any observations fed |
| `RingBufferFull` | ring | SPSC ring buffer has no free slots |
| `RingBufferEmpty` | ring | SPSC ring buffer has no pending items |
| `LorentzConfigError` | lorentz | `beta >= 1` or `beta < 0` or `beta = NaN` |
| `Io` | all | Underlying I/O error |
| `WebSocket` | ws | WebSocket protocol-level error |

## Custom Pipeline Extensions

### Implementing a custom tick normalizer

```rust
use fin_stream::tick::{NormalizedTick, RawTick, TradeSide};
use fin_stream::error::StreamError;

struct MyNormalizer;

impl MyNormalizer {
    fn normalize(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let price = raw.payload["price"]
            .as_str()
            .ok_or_else(|| StreamError::ParseError { reason: "missing price".into() })?
            .parse()
            .map_err(|e: rust_decimal::Error| StreamError::ParseError { reason: e.to_string() })?;
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol.clone(),
            price,
            quantity: rust_decimal::Decimal::ONE,
            side: TradeSide::Buy,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: raw.received_at_ms,
        })
    }
}
```

### Implementing a custom downstream consumer

```rust
use fin_stream::ohlcv::OhlcvBar;

fn process_bar(bar: &OhlcvBar) {
    // Access typed fields: bar.open, bar.high, bar.low, bar.close, bar.volume
    let range = bar.bar_range();
    let body = bar.body_size();
    println!("range={range} body={body} trades={}", bar.trade_count);
    println!("close_location={:.4}", bar.close_location_value().unwrap_or(0.0));
}
```

## Running Tests and Benchmarks

```bash
cargo test                          # unit and integration tests
cargo test --release                # release-mode correctness check
PROPTEST_CASES=1000 cargo test      # extended property-based test coverage
cargo clippy --all-features -- -D warnings
cargo fmt --all -- --check
cargo doc --no-deps --all-features --open
cargo bench                         # Criterion microbenchmarks
cargo audit                         # security vulnerability scan
```

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a full version-by-version history.

## Contributing

### General workflow

1. Fork the repository and create a feature branch.
2. Add or update tests for any changed behaviour. The CI gate requires all tests
   to pass and Clippy to report no warnings.
3. Run `cargo fmt` before opening a pull request.
4. Keep public APIs documented with `///` doc comments; `#![deny(missing_docs)]`
   is active in `lib.rs` — undocumented public items cause a build failure.
5. Open a pull request against `main`. The CI pipeline (fmt, clippy, test
   on three platforms, bench, doc, deny, coverage) must be green before merge.

### Adding a new exchange adapter

1. Add the variant to `Exchange` in `src/tick/mod.rs` with a `///` doc comment.
2. Implement `Display` and `FromStr` for the new variant in the same file.
3. Add a `normalize_<exchange>` method following the pattern of `normalize_binance`.
4. Wire the method into `TickNormalizer::normalize` via the match arm.
5. Add unit tests covering: happy-path, each required missing field returning
   `StreamError::ParseError`, and an invalid decimal string.
6. Update the README "Supported Exchanges" table and `CHANGELOG.md` `[Unreleased]`.

## License

MIT. See [LICENSE](LICENSE) for details.
