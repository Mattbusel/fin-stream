# fin-stream examples and recipes

Short programs for the main building blocks. Every Rust block in this file is compiled and run by `cargo test --doc` (see the doctest structs at the end of `src/lib.rs`), so they stay in step with the API. For the four runnable programs in `examples/`, see the [README](../README.md#examples).

## Other ways to add it

| How | Command |
|-----|---------|
| cargo | `cargo add fin-stream serde_json` |
| Cargo.toml | `fin-stream = "2.11"` (plus `serde_json = "1"` to build raw payloads with `json!`) |
| Latest `main` from git | `cargo add fin-stream --git https://gitlab.com/mattbusel/fin-stream` |
| Just run the examples | `git clone https://gitlab.com/mattbusel/fin-stream && cd fin-stream && cargo run --example tape` |

The README GIF is a real recording of `cargo run --example tape`, made 2026-09-25 and shown at real
speed: a feed thread paces 40 seeded trades from four venues over about five seconds, and the main
thread prints each one as it comes off the lock-free ring.

## Ticks into one-minute bars


Normalize a raw Binance trade and roll it into one-minute bars:

```rust
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
use serde_json::json;

fn main() -> Result<(), fin_stream::StreamError> {
    let normalizer = TickNormalizer::new();
    let mut agg = OhlcvAggregator::new("BTCUSDT", Timeframe::Minutes(1))?;

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


## Quick recipes

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
use rust_decimal_macros::dec;

fn main() -> Result<(), fin_stream::StreamError> {
    let mut norm = MinMaxNormalizer::new(20)?;

    for c in [dec!(100), dec!(102), dec!(98), dec!(105), dec!(103)] {
        norm.update(c);
    }

    let v = norm.normalize(dec!(103))?;
    println!("normalized: {v:.4}");  // a value in [0.0, 1.0]
    Ok(())
}
```

### Z-score normalization with analytics

```rust
use fin_stream::norm::ZScoreNormalizer;
use rust_decimal_macros::dec;

fn main() -> Result<(), fin_stream::StreamError> {
    let mut z = ZScoreNormalizer::new(30)?;

    for v in [dec!(100), dec!(102.5), dec!(99), dec!(103), dec!(101.5)] {
        z.update(v);
    }

    let score = z.normalize(dec!(104))?;
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

    let stale_errors = monitor.check_all(1_010_000); // 10 s later, stale
    for (feed, err) in stale_errors {
        eprintln!("stale: {feed}: {err}");
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

</details>