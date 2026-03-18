[![CI](https://github.com/Mattbusel/fin-stream/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/fin-stream/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/fin-stream.svg)](https://crates.io/crates/fin-stream)
[![docs.rs](https://img.shields.io/docsrs/fin-stream)](https://docs.rs/fin-stream)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

# fin-stream

Lock-free streaming primitives for real-time financial market data. Provides a
composable ingestion pipeline from raw exchange ticks to normalized, transformed
features ready for downstream models or trade execution. Built on Tokio. Targets
100 K+ ticks/second throughput with zero heap allocation on the fast path.

## What it does

fin-stream handles the ingestion layer of a quantitative trading stack:

- Normalizes heterogeneous exchange tick formats (Binance, Coinbase, Alpaca, Polygon)
  into a single canonical `NormalizedTick` type.
- Routes ticks through a lock-free SPSC ring buffer with zero heap allocation on the
  fast path.
- Aggregates normalized ticks into OHLCV bars at arbitrary timeframes, with optional
  synthetic gap-fill bars for missing windows.
- Maintains a rolling min-max normalizer that maps price coordinates into [0, 1]
  without buffering the full history.
- Applies Lorentz spacetime transforms to the price-time plane as a feature
  engineering step for ML pipelines.
- Maintains live order books through incremental delta streaming with crossed-book
  detection.
- Monitors feed staleness per symbol and opens circuit breakers on consistently
  stale feeds.
- Classifies timestamps into market session windows (equity, crypto, forex).

All fallible operations return `Result<_, StreamError>`. The library never panics
in release builds.

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
   [ MinMax Normalizer ]   -- rolling-window coordinate normalization to [0, 1]
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

## Performance characteristics

| Metric                  | Value                                      |
|-------------------------|--------------------------------------------|
| SPSC push/pop latency   | O(1), single cache-line access             |
| SPSC throughput         | >100 K ticks/second (zero allocation)      |
| OHLCV feed per tick     | O(1)                                       |
| Normalization update    | O(1) amortized; O(W) after window eviction |
| Lorentz transform       | O(1), two multiplications per coordinate   |
| Ring buffer memory      | N * sizeof(T) bytes (N is const generic)   |

## Quickstart

```rust
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use fin_stream::ring::SpscRing;
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use fin_stream::norm::MinMaxNormalizer;
use serde_json::json;

fn main() -> Result<(), fin_stream::StreamError> {
    let normalizer = TickNormalizer::new();
    let ring: SpscRing<_, 1024> = SpscRing::new();
    let (prod, cons) = ring.split();
    let mut agg = OhlcvAggregator::new("BTCUSDT", Timeframe::Minutes(1))?;
    let mut norm = MinMaxNormalizer::new(60);

    // Producer side: normalize and enqueue.
    let raw = RawTick::new(
        Exchange::Binance,
        "BTCUSDT",
        json!({ "p": "65000.50", "q": "0.002", "m": false, "t": 1u64 }),
    );
    let tick = normalizer.normalize(raw)?;
    prod.push(tick)?;

    // Consumer side: aggregate and normalize closes.
    while let Ok(tick) = cons.pop() {
        let completed_bars = agg.feed(&tick)?;
        for bar in &completed_bars {
            let close: f64 = bar.close.to_string().parse().unwrap_or(0.0);
            norm.update(close);
            if let Ok(v) = norm.normalize(close) {
                println!("normalized close: {v:.4}");
            }
        }
    }
    Ok(())
}
```

## Supported Exchanges

| Exchange  | Adapter module       | Status     | Wire-format fields used            |
|-----------|----------------------|------------|------------------------------------|
| Binance   | `tick::Exchange::Binance`   | Stable | `p` (price), `q` (qty), `m` (maker/taker), `t` (trade id), `T` (exchange ts) |
| Coinbase  | `tick::Exchange::Coinbase`  | Stable | `price`, `size`, `side`, `trade_id` |
| Alpaca    | `tick::Exchange::Alpaca`    | Stable | `p` (price), `s` (size), `i` (trade id) |
| Polygon   | `tick::Exchange::Polygon`   | Stable | `p` (price), `s` (size), `i` (trade id), `t` (exchange ts) |

All four adapters are covered by unit and integration tests.
To add a new exchange, see the **Contributing** section below.

## Lorentz transform -- mathematical notes

The `LorentzTransform` applies the special-relativistic boost to a (time, price)
coordinate pair:

```
t' = gamma * (t - beta * x)
x' = gamma * (x - beta * t)

where  beta  = v/c    (0 <= beta < 1, dimensionless drift velocity parameter)
       gamma = 1 / sqrt(1 - beta^2)   (Lorentz factor, >= 1)
```

In the financial interpretation, `t` is elapsed time normalized to a convenient
scale and `x` is a normalized log-price or price coordinate. The boost maps the
price-time plane along Lorentz hyperbolas. The spacetime interval
`s^2 = t^2 - x^2` is preserved under the transform. Setting `beta = 0` gives
the identity (gamma = 1). `beta >= 1` is invalid (division by zero in gamma) and
is rejected at construction time with `StreamError::LorentzConfigError`.

The motivation for applying this transform as a feature engineering step is that
certain microstructure signals (momentum, mean-reversion) that appear curved in
the untransformed frame can appear as straight lines in a suitably boosted frame,
simplifying downstream linear models.

## Integration with fin-primitives

fin-stream is the ingestion and transformation layer designed to sit above the
`fin-primitives` crate, which provides the canonical `Tick`, `OrderBook`,
`OhlcvBar`, and signal types. fin-stream imports `fin-primitives` for shared type
definitions and re-exports the types needed by downstream consumers.

## Module map

| Module    | Public types |
|-----------|--------------|
| `tick`    | `RawTick`, `NormalizedTick`, `Exchange`, `TradeSide`, `TickNormalizer` |
| `ring`    | `SpscRing`, `SpscProducer`, `SpscConsumer` |
| `ohlcv`   | `OhlcvAggregator`, `OhlcvBar`, `Timeframe` |
| `norm`    | `MinMaxNormalizer` |
| `lorentz` | `LorentzTransform`, `SpacetimePoint` |
| `book`    | `OrderBook`, `BookDelta`, `BookSide`, `PriceLevel` |
| `health`  | `HealthMonitor`, `FeedHealth`, `HealthStatus` |
| `session` | `SessionAwareness`, `MarketSession`, `TradingStatus` |
| `ws`      | `WsManager`, `ConnectionConfig`, `ReconnectPolicy` |
| `error`   | `StreamError` |

## Running tests and benchmarks

```bash
cargo test                          # unit and integration tests
cargo test --release                # release-mode correctness check
cargo clippy --all-features -- -D warnings
cargo fmt --all -- --check
cargo doc --no-deps --all-features --open
cargo bench                         # criterion microbenchmarks
```

## Contributing

### General workflow

1. Fork the repository and create a feature branch.
2. Add or update tests for any changed behaviour. The CI gate requires all tests
   to pass and Clippy to report no warnings.
3. Run `cargo fmt` before opening a pull request.
4. Keep public APIs documented with `///` doc comments; `#![deny(missing_docs)]`
   is active — undocumented public items will cause a build failure.
5. Open a pull request against `main`. The CI pipeline (fmt, clippy, test, release
   test, doc) must be green before merge.

### Adding a new exchange adapter

Follow these steps to wire in a new exchange (example: `Kraken`).

1. **Add the variant** to `Exchange` in `src/tick/mod.rs`:

   ```rust
   pub enum Exchange {
       // ...existing variants...
       /// Kraken WebSocket v2 feed.
       Kraken,
   }
   ```

2. **Implement `Display` and `FromStr`** for the new variant in the same file.

3. **Add a normalization method** `normalize_kraken` following the pattern of
   `normalize_binance`. Extract the price and quantity fields from the raw JSON
   payload using `parse_decimal_field`, populate optional fields where the exchange
   provides them (`side`, `trade_id`, `exchange_ts_ms`), and return a
   `NormalizedTick`.

4. **Wire the method** into `TickNormalizer::normalize`:

   ```rust
   Exchange::Kraken => self.normalize_kraken(raw),
   ```

5. **Add unit tests** in the `#[cfg(test)]` block of `src/tick/mod.rs`. Include
   at minimum: a happy-path normalization test, a test for each required missing
   field returning `StreamError::ParseError`, and a test for an invalid decimal
   string.

6. **Update the README** "Supported Exchanges" table and the `CHANGELOG.md`
   `[Unreleased]` section.

All contributions are licensed under MIT.
