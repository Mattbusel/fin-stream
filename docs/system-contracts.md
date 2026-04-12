# fin-stream System Contracts & Integration Guide

**Version:** 2.11.0 | **Status:** Stable

---

## 1. Module Interdependencies

### 1.1 Crate Dependency Graph

```
External Crates
│
├─ tokio (async runtime)
├─ rust_decimal (exact arithmetic)
├─ dashmap (lock-free maps)
├─ serde (serialization)
├─ chrono (date/time)
├─ thiserror (error derive)
└─ tonic (gRPC)

Internal Crate Graph
│
fin-stream-core (foundational)
│   ├─ error (StreamError type)
│   ├─ protocol (raw message codecs)
│   ├─ ring (SPSC buffer)
│   ├─ tick (NormalizedTick, TickNormalizer)
│   ├─ ohlcv (OhlcvBar, OhlcvAggregator)
│   ├─ book (OrderBook management)
│   ├─ health (HealthMonitor)
│   ├─ session (MarketSession awareness)
│   ├─ persistence (Tick recording/replay)
│   └─ prelude (common exports)
       │
       ▼
fin-stream-net (I/O layer)
│   ├─ ws (WebSocket managers)
│   ├─ fix (FIX 4.2 adapter)
│   ├─ grpc (gRPC streaming)
│   ├─ portfolio_feed (multi-asset aggregator)
│   └─ circuit_breaker (connection resilience)
       │
       ├──────────────────────────┐
       │                          │
       ▼                          ▼
fin-stream-analytics          fin-stream-trading
│                             │
├─ norm (Z-score norm)        ├─ backtest (replay engine)
├─ correlation                ├─ portfolio_optimizer
├─ ofi (order flow imbalance) ├─ position_manager
├─ toxicity (PIN, VPIN)       ├─ risk_engine
├─ microstructure            ├─ execution (slippage)
├─ regime                     ├─ market_maker (simulation)
├─ anomaly                    ├─ statarb (pairs trader)
├─ quality (feed scoring)     └─ order_router (venue selection)
└─ circuit (symbol breaker)
       │
       ├──────────────────────────┐
       │                          │
       ▼                          ▼
fin-stream-experimental      fin-stream-simulation
│                            │
├─ neural networks           ├─ lob_sim (order book)
├─ graph algorithms          ├─ market_simulator
├─ reinforcement learning    ├─ replay (tick replay)
└─ advanced features         └─ snapshot (binary storage)
```

**Dependency Rules:**
1. **No circular dependencies** (guaranteed by crate boundaries)
2. **Core has no dependencies** on other internal crates (except error)
3. **Net depends on Core only** (no analytics or trading)
4. **Analytics & Trading depend on Core + Net** (symmetric)
5. **Experimental & Simulation depend on Core** (optional features)

---

### 1.2 Module Import Patterns

#### Safe Imports (Always OK)
```rust
use fin_stream::core::tick::NormalizedTick;
use fin_stream::core::ohlcv::OhlcvBar;
use fin_stream::core::error::StreamError;
use fin_stream::core::ring::SpscRing;

use fin_stream::net::ws::WsManager;
use fin_stream::analytics::norm::ZScoreNormalizer;
use fin_stream::trading::position_manager::PositionManager;
```

#### Unsafe Imports (Avoid in Production Code)
```rust
// Internal modules (no stability guarantees)
use fin_stream::core::protocol::RawBinanceMessage;  // ✗ Breaks on minor version
use fin_stream::analytics::window::internal::RingBuffer;  // ✗ Implementation detail
```

#### Recommended Prelude
```rust
use fin_stream::prelude::*;
// Exports: NormalizedTick, OhlcvBar, OrderBook, SpscRing, StreamError, etc.
```

---

## 2. Public API Contracts

### 2.1 Type Contracts

#### `NormalizedTick` — Immutable Public Data

```rust
pub struct NormalizedTick {
    pub exchange: Exchange,          // Invariant: one of 4 exchanges
    pub symbol: String,              // Invariant: non-empty
    pub price: Decimal,              // Invariant: > 0
    pub quantity: Decimal,           // Invariant: > 0
    pub side: Option<TradeSide>,     // Invariant: Buy | Sell
    pub trade_id: Option<String>,    // Invariant: non-empty if Some
    pub exchange_ts_ms: Option<u64>, // Invariant: milliseconds since epoch
    pub received_at_ms: u64,         // Invariant: milliseconds since epoch
}
```

**Contract:**
- All fields are **read-only** (no setters)
- Constructed via `TickNormalizer::normalize()` or deserialization
- Invariants checked at construction time
- If invariants violated → `StreamError::ParseError` (not panic)

**Serialization Stability:**
- Serde-based; field names never change
- New fields appended, never removed (backward compatible)
- Decimal serialized as string (no floating-point precision loss)

**Memory Stability:**
- Stack-allocated, `Copy`, `Send`, `Sync`
- Size: ~80 bytes (9 fields + padding)
- No heap allocations in construction

---

#### `OhlcvBar` — Completed Bar

```rust
pub struct OhlcvBar {
    pub symbol: String,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub vwap: Decimal,
    pub tick_count: u64,
    pub start_ms: u64,
    pub end_ms: u64,
}
```

**Contract:**
- High >= max(Open, Close, Low) (always)
- Low <= min(Open, Close, High) (always)
- Volume > 0 (always)
- VWAP in [Low, High] (usually; may differ due to pre-market)
- end_ms > start_ms (always)

**Emission Guarantee:**
- Emitted exactly once when bar boundary crossed
- Never reissued; no partial bars
- `tick_count >= 1` (at least one tick in bar)

---

#### `OrderBook` — Concurrent State

```rust
pub struct OrderBook {
    pub symbol: String,
    pub bids: BTreeMap<OrdF64, f64>,
    pub asks: BTreeMap<OrdF64, f64>,
    pub sequence: u64,
    pub last_updated_ms: u64,
}
```

**Contract:**
- Bid prices in descending order (BTreeMap maintains)
- Ask prices in ascending order (BTreeMap maintains)
- Best bid < best ask (checked on apply_update)
- sequence always increases (monotonic)
- Not Copy; must be cloned or referenced

**Concurrency:**
- Typically accessed via `OrderBookManager` (DashMap)
- Per-symbol lock; concurrent access to different symbols
- Single-threaded: direct use OK; multi-threaded: use Manager

---

### 2.2 Trait Contracts

#### `TickSource` — Async Tick Provider

```rust
#[async_trait]
pub trait TickSource: Send + Sync {
    async fn next_tick(&mut self) -> Result<Option<NormalizedTick>, StreamError>;
    async fn reset(&mut self) -> Result<(), StreamError>;
}
```

**Contract:**
- `next_tick()` returns `Some(tick)` while stream active
- Returns `None` when stream exhausted (graceful end)
- Returns `Err(_)` on unrecoverable error
- `reset()` is optional; for replay only
- Implementations must be `Send + Sync` (safe for `Arc<dyn TickSource>`)

**Implementations:**
- `WsManager` — Live WebSocket feed
- `TickReplayer` — Historical NDJSON file
- `SyntheticMarketGenerator` — Simulated ticks

**Error Handling:**
```rust
match source.next_tick().await {
    Ok(Some(tick)) => { /* process tick */ }
    Ok(None) => { /* stream ended gracefully */ }
    Err(e) => { /* log error; may retry */ }
}
```

---

#### `TickFilter` — Predicate for Filtering

```rust
pub trait TickFilter: Send + Sync {
    fn filter(&self, tick: &NormalizedTick) -> bool;
}
```

**Contract:**
- Returns `true` to keep tick; `false` to discard
- No side effects; pure function
- Thread-safe for concurrent calls

**Common Implementations:**
```rust
pub struct PriceRangeFilter { min: Decimal, max: Decimal }
pub struct VolumeFilter { min: Decimal }
pub struct SymbolFilter { symbols: HashSet<String> }
pub struct StaleFilter { max_age_ms: u64 }
```

---

#### `TickTransform` — Mutation Function

```rust
pub trait TickTransform: Send + Sync {
    fn transform(&self, tick: NormalizedTick) -> NormalizedTick;
}
```

**Contract:**
- Maps one tick to another; may modify fields
- Must not panic
- May return error (via Result variant if supported)

**Common Implementations:**
```rust
pub struct PriceRounder { decimals: u8 }
pub struct VolumeNormalizer { scale_factor: Decimal }
pub struct TimestampAligner { granularity_ms: u64 }
```

---

### 2.3 Function Contracts

#### `TickNormalizer::normalize()`

```rust
pub fn normalize(
    exchange: Exchange,
    symbol: &str,
    payload: &serde_json::Value,
) -> Result<NormalizedTick, StreamError>
```

**Contract:**
- **Input:** Raw JSON from exchange WebSocket
- **Output:** Validated `NormalizedTick` or `ParseError`
- **Side effects:** None (stateless)
- **Panics:** Never on valid input

**Error Cases:**
- Missing required fields → `ParseError { reason: "missing field X" }`
- Invalid Decimal format → `ParseError { reason: "invalid price" }`
- Exchange-specific validation fail → `ParseError`

**Example:**
```rust
let payload = json!({
    "p": "50000.50",
    "q": "0.01",
    "T": 1700000000000,
    "m": false
});

match TickNormalizer::normalize(Exchange::Binance, "BTCUSDT", &payload) {
    Ok(tick) => println!("✓ {:?}", tick),
    Err(e) => eprintln!("✗ Parse error: {}", e),
}
```

---

#### `OrderBook::apply_update()`

```rust
impl OrderBook {
    pub fn apply_update(&mut self, delta: &BookUpdate) -> Result<(), BookError>
}
```

**Contract:**
- Updates order book with incremental delta
- Qty=0 removes level; Qty>0 sets level
- Checks sequence (gaps rejected)
- Checks crossed book (bid < ask)
- If error: book unchanged (atomic semantics)

**Success:** `Ok(())` and book is updated  
**Errors:**
- `SequenceGap` — Lost messages
- `CrossedBook` — Bid >= Ask
- `StaleUpdate` — Sequence < last

**Example:**
```rust
let delta = BookUpdate {
    symbol: "BTC-USD".into(),
    sequence: 100,
    bids: vec![(50000.0, 5.0), (49999.0, 10.0)],
    asks: vec![(50001.0, 3.0), (50002.0, 8.0)],
};

match book.apply_update(&delta) {
    Ok(()) => println!("Book updated"),
    Err(BookError::CrossedBook { bid, ask }) => {
        eprintln!("Crossed: {} >= {}", bid, ask);
    }
    Err(e) => eprintln!("Error: {:?}", e),
}
```

---

## 3. Extension Points

### 3.1 Custom TickFilter Implementation

```rust
pub struct MyCustomFilter {
    threshold: f64,
}

impl TickFilter for MyCustomFilter {
    fn filter(&self, tick: &NormalizedTick) -> bool {
        // Keep only ticks with price > threshold
        tick.price > Decimal::from_f64_retain(self.threshold).unwrap_or_default()
    }
}

// Usage:
let filter = MyCustomFilter { threshold: 50000.0 };
let filtered = pipeline.add_filter(Box::new(filter));
```

### 3.2 Custom TickSource Implementation

```rust
pub struct DatabaseTickSource {
    db: Arc<Database>,
    symbol: String,
    cursor: u64,
}

#[async_trait]
impl TickSource for DatabaseTickSource {
    async fn next_tick(&mut self) -> Result<Option<NormalizedTick>, StreamError> {
        if let Some(row) = self.db.fetch_tick(self.symbol.clone(), self.cursor).await? {
            self.cursor += 1;
            Ok(Some(NormalizedTick {
                exchange: Exchange::Binance,
                symbol: self.symbol.clone(),
                price: row.price,
                quantity: row.quantity,
                // ... other fields
                received_at_ms: row.timestamp_ms,
            }))
        } else {
            Ok(None)  // No more ticks
        }
    }

    async fn reset(&mut self) -> Result<(), StreamError> {
        self.cursor = 0;
        Ok(())
    }
}
```

### 3.3 Custom Analytics

```rust
pub struct MyIndicator {
    window: VecDeque<f64>,
    max_size: usize,
}

impl MyIndicator {
    pub fn new(window_size: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            max_size: window_size,
        }
    }

    pub fn update(&mut self, bar: &OhlcvBar) -> f64 {
        let close_f64 = bar.close.to_f64().unwrap_or(0.0);
        self.window.push_back(close_f64);
        
        if self.window.len() > self.max_size {
            self.window.pop_front();
        }

        // Compute custom indicator
        self.window.iter().sum::<f64>() / self.window.len() as f64
    }
}
```

---

## 4. Versioning & Compatibility

### 4.1 Semantic Versioning Rules

**Major (2.0 → 3.0):** Breaking changes
- Remove public type or method
- Change signature of public function
- Change behavior of public API
- Change wire format (serialization)

**Minor (2.x → 2.y):** New features, backward compatible
- Add new public type
- Add new method (appended to trait)
- Modify private implementation
- Add new error variant (with default match arm)

**Patch (2.x.y → 2.x.z):** Bug fixes
- Fix logic error in public method
- Optimize performance without API change
- Update dependencies for security

### 4.2 Deprecation Policy

```rust
#[deprecated(
    since = "2.10.0",
    note = "use `new_method()` instead"
)]
pub fn old_method(&self) { ... }
```

**Timeline:** Deprecated in minor version; removed in next major version  
**Notice:** At least 6 months advance warning

### 4.3 Binary Compatibility

**Guaranteed Stable:**
- `NormalizedTick` serialization (Serde)
- `OhlcvBar` serialization
- `StreamError` Display format (for parsing logs)

**Not Guaranteed:**
- Binary memory layout (padding, field order)
- `OrderBook` internal BTreeMap structure
- `Arc<DashMap>` pointer representation

### 4.4 Rust Version Support

**MSRV (Minimum Supported Rust Version):** 1.75

**Policy:**
- Advance MSRV only on major version bumps
- At least 3 months notice before bump
- Affected by: const generics, async/await, GAT features

**Version Timeline:**
| Version | Rust | Release | EOL |
|---|---|---|---|
| 2.0+ | 1.75+ | 2024-01 | 2027-01 |
| 3.0+ | 1.80+ (est) | 2026-01 | TBD |

---

## 5. Integration Patterns

### 5.1 Embedded in Larger Application

```rust
// Your application using fin-stream
use fin_stream::prelude::*;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create normalization ring
    let ring: SpscRing<NormalizedTick, 8192> = SpscRing::new();
    let (tx, rx) = ring.split();

    // 2. Spawn WebSocket task
    tokio::spawn(async move {
        let manager = WsManager::connect(
            "wss://stream.binance.com:9443/ws/btcusdt@aggTrade",
            256,
        ).await?;
        
        let mut source = manager;
        while let Ok(Some(raw_tick)) = source.next_tick().await {
            let normalized = TickNormalizer::normalize(
                raw_tick.exchange,
                &raw_tick.symbol,
                &raw_tick.payload,
            )?;
            tx.push(normalized)?;  // Lock-free
        }
        Ok::<_, StreamError>(())
    });

    // 3. Spawn aggregation task
    tokio::spawn(async move {
        let mut agg = OhlcvAggregator::new("BTC-USDT", Timeframe::Minutes(1));
        
        while let Some(tick) = rx.pop() {
            if let Some(bar) = agg.on_tick(&tick) {
                println!("New bar: {:?}", bar);
            }
        }
    });

    // Keep app alive
    std::future::pending().await
}
```

### 5.2 Backtesting Pipeline

```rust
use fin_stream::prelude::*;
use fin_stream::simulation::replay::TickReplayer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Create replayer from NDJSON file
    let mut replayer = TickReplayer::open(
        "data/btc_ticks.ndjson",
        10.0,  // 10x speed
    )?;

    // 2. Create channel for tick stream
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);

    // 3. Spawn replayer
    tokio::spawn(async move {
        replayer.run(tx).await.ok();
    });

    // 4. Run backtest strategy
    let mut portfolio = PortfolioState::new();
    while let Some(tick) = rx.recv().await {
        let signal = portfolio.update(&tick);
        if signal.should_trade() {
            portfolio.place_order(&signal);
        }
    }

    println!("Backtest complete: PnL = ${:.2}", portfolio.pnl());
    Ok(())
}
```

### 5.3 Multi-Exchange NBBO Aggregation

```rust
use fin_stream::multi_exchange::{MultiExchangeAggregator, AggregatorConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create NBBO aggregator for BTC-USD, flag arbitrage > $10
    let config = AggregatorConfig::new("BTC-USD", Decimal::from(10))?;
    let (agg, mut nbbo_rx, mut arb_rx) = MultiExchangeAggregator::new(config, 64);

    // Ingest from multiple feeds
    tokio::spawn(async move {
        let binance = WsManager::connect(
            "wss://stream.binance.com:9443/ws/btcusdt@aggTrade", 256
        ).await?;
        // ... feed ticks to agg
        Ok::<_, StreamError>(())
    });

    // Receive consolidated NBBO
    while let Some(nbbo) = nbbo_rx.recv().await {
        println!("NBBO bid: ${}, ask: ${}", nbbo.best_bid, nbbo.best_ask);
    }

    // Receive arbitrage alerts
    while let Some(opp) = arb_rx.recv().await {
        println!("Arb opportunity: buy @ {} sell @ {} ({:.1} bps)",
            opp.buy_price, opp.sell_price, opp.spread_bps);
    }

    Ok(())
}
```

---

## 6. Performance Contracts

### 6.1 Latency Guarantees

| Operation | Target | Typical | P99 | Notes |
|---|---|---|---|---|
| Tick normalization | <1 μs | 500 ns | 5 μs | Single-pass parsing |
| Ring push | <100 ns | 50 ns | 200 ns | Atomic operations only |
| Ring pop | <100 ns | 50 ns | 200 ns | Atomic operations only |
| OrderBook apply | <10 μs | 1 μs | 100 μs | BTreeMap O(log n) |
| Bar close | <10 μs | 500 ns | 50 μs | Depends on bar type |
| Volatility update | <10 μs | 1 μs | 50 μs | Welford's algorithm |

**Caveats:**
- Measured on dedicated core at 3.6 GHz
- No contention (single-threaded)
- Cold cache not included
- P99 includes page faults, context switches

---

### 6.2 Throughput Guarantees

| Path | Target | Achieved | Headroom |
|---|---|---|---|
| Tick → Ring | 100K/sec | >1M/sec | 10x |
| Ring → Bar | 100K/sec | >1M/sec | 10x |
| Bar → Features | 100K/sec | >500K/sec | 5x |
| End-to-end | 100K/sec | >100K/sec | 1x |

**Test Conditions:**
- Synthetic data (no network jitter)
- Single symbol
- Single core
- No contention

---

### 6.3 Memory Contracts

| Component | Per-Symbol | Per-100-Ticks | Scaling |
|---|---|---|---|
| OrderBook | ~50 KB | — | O(depth) |
| OHLCV Bar | ~200 bytes | — | O(1) |
| Rolling risk (100 ticks) | — | ~8 KB | O(window) |
| Correlation matrix | — | ~1 MB (100 assets) | O(N²) |
| Ring buffer (8192 slots) | — | ~650 KB | O(capacity) |

**Total for 100 symbols, 100-tick windows:**
```
OrderBooks:        100 × 50 KB = 5 MB
Aggregators:       100 × 8 KB = 800 KB
Rolling metrics:   100 × 8 KB = 800 KB
Ring buffer:                     650 KB
────────────────────────────────────
Total:                          ~8 MB
```

---

## 7. Deprecation Schedule

### 7.1 Currently Supported

| Component | Version | Stability | Notes |
|---|---|---|---|
| `NormalizedTick` | 2.0+ | Stable | Never removed |
| `OhlcvBar` | 2.0+ | Stable | Never removed |
| `TickNormalizer` | 2.0+ | Stable | Never removed |
| `OrderBook` | 2.0+ | Stable | Never removed |
| `SpscRing` | 2.0+ | Stable | Never removed |

### 7.2 Planned Deprecations

| Component | Deprecated In | Removal In | Reason |
|---|---|---|---|
| `TickReplayer` (old API) | — | — | No deprecation planned |
| `WsManager::legacy_connect` | (never existed) | N/A | N/A |

---

## 8. Troubleshooting & Support

### 8.1 Common Integration Issues

**Issue:** `ring full; producer blocked`
- **Cause:** Aggregation task slower than normalization
- **Fix:** Increase ring capacity or add more aggregation workers

**Issue:** `BookCrossed error; bid >= ask`
- **Cause:** Exchange sent crossed book snapshot
- **Fix:** Reject update; request fresh snapshot

**Issue:** Timestamps not monotonic
- **Cause:** Slow feeds mixed with fast feeds; no latency compensation
- **Fix:** Use `FeedAggregator` with latency offsets

### 8.2 Reporting Issues

File issues with:
1. **Reproducible example** (minimal code)
2. **Full error message** (including context)
3. **Rust version** (`rustc --version`)
4. **Platform** (OS, CPU)
5. **fin-stream version** (`Cargo.lock` relevant lines)

---

## Appendix: Quick Reference

### API Cheat Sheet

```rust
// Tick normalization
let tick = TickNormalizer::normalize(Exchange::Binance, "BTC-USDT", &json_payload)?;

// Ring buffer
let (tx, rx) = SpscRing::<NormalizedTick, 8192>::new().split();
tx.push(tick)?;
let tick = rx.pop();

// Order book
let mut book = OrderBook::new("BTC-USD");
book.apply_update(&delta)?;
let spread = book.spread();

// OHLCV aggregation
let mut agg = OhlcvAggregator::new("BTC-USD", Timeframe::Minutes(1));
if let Some(bar) = agg.on_tick(&tick) { ... }

// Rolling risk
let mut risk_mon = RiskMonitor::new();
risk_mon.update("BTC-USD", tick.price);
let snap = risk_mon.risk_snapshot("BTC-USD");

// Health monitoring
let health = HealthMonitor::new(5000);  // 5s threshold
health.record_tick("binance", now_ms);
let status = health.status("binance")?;
```

