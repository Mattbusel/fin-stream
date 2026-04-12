# fin-stream Technical Specifications

**Version:** 2.11.0 | **Edition:** 2021 | **MSRV:** 1.75

---

## 1. Module Interfaces & Contracts

### 1.1 Core Data Types

#### `NormalizedTick` — Canonical Tick Format

```rust
pub struct NormalizedTick {
    pub exchange: Exchange,          // Source: Binance | Coinbase | Alpaca | Polygon
    pub symbol: String,              // e.g., "BTC-USDT"
    pub price: Decimal,              // Exact decimal; no f64
    pub quantity: Decimal,           // Volume executed
    pub side: Option<TradeSide>,     // Buy | Sell
    pub trade_id: Option<String>,    // Exchange-assigned ID
    pub exchange_ts_ms: Option<u64>, // Exchange-side timestamp
    pub received_at_ms: u64,         // Local system-clock timestamp
}

pub enum TradeSide { Buy, Sell }
pub enum Exchange { Binance, Coinbase, Alpaca, Polygon }
```

**Invariants:**
- `price > 0` and `quantity > 0` (always)
- `received_at_ms` is monotonically increasing per pipeline
- `exchange_ts_ms` may have gaps or be None (exchange-dependent)
- Prices and quantities never truncated; full Decimal precision preserved

**Serialization:** Serde JSON with `serde-with-str` for Decimal → string round-trip

**Copy-ability:** Stack-allocated, `Send + Sync`, designed for zero-copy hot path

---

#### `OhlcvBar` — OHLCV Bar

```rust
pub struct OhlcvBar {
    pub symbol: String,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub vwap: Decimal,              // Volume-weighted average price
    pub tick_count: u64,            // Ticks aggregated into this bar
    pub start_ms: u64,              // Bar open time
    pub end_ms: u64,                // Bar close time
}

pub enum Timeframe {
    Minutes(u32),
    Hours(u32),
    Seconds(u32),
}
```

**VWAP Update (Online):**
```
vwap = (vwap * cumulative_volume + price * quantity) / (cumulative_volume + quantity)
```

**High/Low Tracking:** Maintained across all ticks in the bar (not just consecutive extremes)

---

#### `OrderBook` — L2 Order Book

```rust
pub struct OrderBook {
    pub symbol: String,
    pub bids: BTreeMap<OrdF64, f64>,   // price → quantity
    pub asks: BTreeMap<OrdF64, f64>,   // price → quantity
    pub sequence: u64,                 // Incremental sequence counter
    pub last_updated_ms: u64,          // Timestamp of last update
}

pub struct BookUpdate {
    pub symbol: String,
    pub sequence: u64,
    pub bids: Vec<(f64, f64)>,    // [(price, quantity)]
    pub asks: Vec<(f64, f64)>,    // [(price, quantity)]
}
```

**Methods:**
| Method | Complexity | Returns |
|---|---|---|
| `best_bid()` | O(1) | `Option<PriceLevel>` |
| `best_ask()` | O(1) | `Option<PriceLevel>` |
| `spread()` | O(1) | `Option<f64>` |
| `mid_price()` | O(1) | `Option<f64>` |
| `depth(n)` | O(n log n) | `(Vec<PriceLevel>, Vec<PriceLevel>)` |
| `imbalance()` | O(n) | `f64` (top-5 ratio) |
| `apply_update()` | O(k log n) | `Result<(), BookError>` where k = # levels |

**Constraints:**
- `qty == 0` removes level (delta encoding)
- Sequence gaps rejected with `BookError::SequenceGap`
- Crossed books (bid >= ask) rejected with `BookError::CrossedBook`

---

### 1.2 Ring Buffer (SPSC)

#### `SpscRing<T, N>` — Lock-Free SPSC Buffer

```rust
pub struct SpscRing<T, const N: usize> {
    buf: Box<[UnsafeCell<MaybeUninit<T>>; N]>,
    head: AtomicUsize,  // Consumer read pointer
    tail: AtomicUsize,  // Producer write pointer
}

impl<T, const N: usize> SpscRing<T, N> {
    pub fn new() -> Self                           // O(N) allocation
    pub fn push(&self, item: T) -> Result<(), T>   // O(1), Acquire semantics
    pub fn pop(&self) -> Option<T>                 // O(1), Release semantics
    pub fn is_empty(&self) -> bool                 // O(1)
    pub fn is_full(&self) -> bool                  // O(1)
    pub fn len(&self) -> usize                     // O(1)
    pub fn capacity(&self) -> usize                // O(1)
    pub fn split(self: Arc<Self>) -> (Producer<T, N>, Consumer<T, N>)
}
```

**Invariants:**
- Usable capacity = N - 1 (one slot reserved for full/empty sentinel)
- `head == tail` ⟹ empty
- `tail - head == N - 1` (wrapping) ⟹ full
- Only producer writes `tail`; only consumer writes `head`

**Performance:** 150M push/pop pairs/sec on Zen 3 @ 3.6 GHz; zero allocations

**Thread Safety:**
- `SpscRing` is `Send` but **not** `Sync`
- Must call `split()` to get `(SpscProducer<Send>, SpscConsumer<Send>)` for thread sharing
- Each half can be sent to a different thread

---

### 1.3 Feed Aggregation

#### `FeedAggregator` — Multi-Feed Merge

```rust
pub enum MergeStrategy {
    BestBid,                // Highest price across feeds
    BestAsk,                // Lowest price across feeds
    VwapWeighted,           // VWAP of all buffered ticks
    PrimaryWithFallback,    // Primary feed with BestBid fallback when stale
}

pub struct FeedHandle {
    feed_id: String,
    latency_offset_ms: u64, // Subtract from received_at_ms before merge
}

pub struct FeedAggregator {
    strategy: MergeStrategy,
    feed_buffer_capacity: usize,
    merge_window: usize,
}

impl FeedAggregator {
    pub fn new(config: AggregatorConfig) -> Self
    pub fn add_feed(&mut self, handle: FeedHandle) -> Result<mpsc::Sender<NormalizedTick>, StreamError>
    pub fn next_tick(&mut self) -> Option<NormalizedTick>
}
```

**Latency Compensation:**
```
merge_timestamp = received_at_ms - latency_offset_ms
```
Allows aligning feeds that arrive out-of-order due to network jitter.

**Merge Strategies:**

| Strategy | Behavior | Use Case |
|---|---|---|
| `BestBid` | Emit tick with highest price | Replicating top-of-book asks |
| `BestAsk` | Emit tick with lowest price | Replicating top-of-book bids |
| `VwapWeighted` | Synthesize tick at VWAP of buffer | Execution slippage modeling |
| `PrimaryWithFallback` | Primary feed; BestBid on staleness | Deterministic primary with hedge |

---

### 1.4 Health Monitoring

#### `HealthMonitor` — Feed Staleness Detection

```rust
pub struct HealthMonitor {
    staleness_threshold_ms: u64,
    feeds: DashMap<String, FeedHealth>,
}

pub struct FeedHealth {
    pub feed_id: String,
    pub status: HealthStatus,       // Healthy | Stale | Unknown
    pub last_tick_ms: u64,
    pub tick_count: u64,
    pub health_score: f64,          // [0.0, 1.0]
}

pub enum HealthStatus {
    Healthy,
    Stale,
    Unknown,
}

impl HealthMonitor {
    pub fn new(staleness_threshold_ms: u64) -> Self
    pub fn record_tick(&self, feed_id: &str, received_ms: u64)
    pub fn status(&self, feed_id: &str) -> Result<HealthStatus, StreamError>
    pub fn health_snapshot(&self) -> HashMap<String, FeedHealth>
}
```

**Health Score Calculation:**
```
score = 1.0 if (now_ms - last_tick_ms) < threshold_ms else 0.0
```

**DashMap Usage:** Per-feed entry lock; concurrent reads/writes without global lock

---

### 1.5 Tick Normalization

#### `TickNormalizer` — Raw → Normalized

```rust
pub struct TickNormalizer;

impl TickNormalizer {
    pub fn normalize(
        exchange: Exchange,
        symbol: &str,
        payload: &serde_json::Value,
    ) -> Result<NormalizedTick, StreamError>
}
```

**Exchange-Specific Parsing:**

| Exchange | Payload Format | Key Fields | Notes |
|---|---|---|---|
| **Binance** | JSON `@aggTrade` stream | `p`, `q`, `T`, `m` | No trade side; infer from sequence |
| **Coinbase** | JSON WebSocket frame | `price`, `size`, `side`, `trade_id` | Full trade side available |
| **Alpaca** | Msgpack/JSON mixed | `price`, `size`, `trade_id` | No inherent trade side |
| **Polygon** | JSON `{A: ...}` | `x`, `v`, `vw`, `n` | Volume-weighted aggregates |

**Error Handling:**
- Missing required fields: `ParseError`
- Type coercion failures: `ParseError`
- Invalid decimals: `ParseError`

**Properties:**
- Deterministic: same raw payload → same NormalizedTick
- `Send + Sync`: safe to share across tasks
- Stateless: no mutable state; no `&mut self`

---

## 2. Data Structures & Schemas

### 2.1 Risk & Analytics Types

#### `RiskSnapshot` — Point-in-Time Risk Metrics

```rust
pub struct RiskSnapshot {
    pub volatility: Option<f64>,    // Annualized, e.g., 0.25 = 25%
    pub var_95: Option<f64>,        // 95% confidence VaR
    pub max_drawdown: Option<f64>,  // Max (peak - trough) / peak
    pub sharpe: Option<f64>,        // Sharpe ratio @ 252 trading days/year
    pub last_price: Decimal,
}
```

#### `CointegrationResult` — Pair Cointegration Test

```rust
pub struct CointegrationResult {
    pub spread_mean: f64,
    pub spread_std: f64,
    pub half_life: f64,             // Mean reversion half-life (bars)
    pub is_cointegrated: bool,      // ADF t-stat < -2.86?
    pub adf_statistic: f64,
    pub p_value: f64,
}
```

#### `MarketRegime` — Regime Classification

```rust
pub enum MarketRegime {
    Trending { direction: i8 },     // +1 = up, -1 = down
    MeanReverting,
    HighVolatility,
    LowVolatility,
    Microstructure,                 // Insufficient warm-up bars
}

pub struct RegimeSnapshot {
    pub current_regime: MarketRegime,
    pub confidence: f64,            // [0, 1]
    pub hurst: Option<f64>,         // Hurst exponent
    pub adx: Option<f64>,           // Average Directional Index
}
```

---

### 2.2 Error Hierarchy

```rust
pub enum StreamError {
    // Connection errors
    ConnectionFailed { url: String, reason: String },
    Disconnected { url: String },
    ReconnectExhausted { url: String, attempts: u32 },

    // Tick parsing
    ParseError { exchange: String, reason: String },

    // Feed health
    StaleFeed { feed_id: String, elapsed_ms: u64, threshold_ms: u64 },
    UnknownFeed { feed_id: String },

    // Configuration
    ConfigError { reason: String },

    // Order book
    BookReconstructionFailed { symbol: String, reason: String },
    BookCrossed { symbol: String, bid: Decimal, ask: Decimal },
    SequenceGap { symbol: String, expected: u64, received: u64 },

    // Ring buffer
    RingFull,
    RingEmpty,

    // Generic
    InvalidState { reason: String },
    Timeout,
    IOError(String),
}
```

**All variants are `Send + Sync` and serialize to machine-parseable Display strings.**

---

## 3. API Boundaries

### 3.1 Public API Surface

#### Core Traits (Public)
- `TickSource` — Async tick provider (live or replay)
- `TickFilter` — Tick predicate for filtering
- `TickTransform` — Tick mutation function
- `TickRouter` — Per-symbol tick routing logic

#### Core Types (Public)
- `NormalizedTick`, `RawTick`, `OhlcvBar`, `OrderBook`
- `HealthMonitor`, `HealthStatus`
- `FeedAggregator`, `MergeStrategy`
- `StreamError` (all variants)
- Ring buffer: `SpscRing<T, N>`, `SpscProducer<T, N>`, `SpscConsumer<T, N>`

#### Constructors (Public)
- `SpscRing::new()` → `SpscRing<T, N>`
- `HealthMonitor::new(threshold_ms)` → `HealthMonitor`
- `TickNormalizer::normalize(...)` → `Result<NormalizedTick, StreamError>`
- `OrderBook::new(symbol)` → `OrderBook`

### 3.2 Internal API Surface

#### Private Modules
- `protocol` — Raw exchange message codecs
- `persistence` — File I/O internals
- `data_pipeline` — Composable filters (implementation)

#### Semi-Private (Crate-level)
- `ring::producer_consumer_pair()` — Helper for `split()`
- `book::delta_apply()` — BTreeMap merge algorithm
- `agg::merge_strategies` — Strategy implementations

---

## 4. Protocol Specifications

### 4.1 WebSocket Protocols

#### Binance WebSocket Stream

**Endpoint:** `wss://stream.binance.com:9443/ws/{symbol}@aggTrade`

**Message Format (JSON):**
```json
{
  "e": "aggTrade",
  "E": 1700000000123,
  "s": "BTCUSDT",
  "a": 12345678,
  "p": "50000.50",
  "q": "0.01",
  "f": 100000000,
  "l": 100000001,
  "T": 1700000000000,
  "m": true
}
```

| Field | Mapping | Notes |
|---|---|---|
| `p` | price | First maker in aggregate |
| `q` | quantity | Aggregate quantity |
| `T` | exchange_ts_ms | Trade initiation time |
| `m` | side | `true` = seller-initiated (BUY), `false` = buyer-initiated (SELL) |

---

#### Coinbase WebSocket Stream

**Endpoint:** `wss://ws-feed.exchange.coinbase.com`

**Subscribe Message:**
```json
{
  "type": "subscribe",
  "product_ids": ["BTC-USD", "ETH-USD"],
  "channels": ["matches"]
}
```

**Tick Message (matches):**
```json
{
  "type": "match",
  "product_id": "BTC-USD",
  "price": "50000.50",
  "size": "0.01",
  "side": "buy",
  "trade_id": 123456789,
  "time": "2024-11-15T10:30:00.000000Z"
}
```

| Field | Mapping | Notes |
|---|---|---|
| `price` | price | Direct mapping |
| `size` | quantity | Direct mapping |
| `side` | side | "buy" or "sell" |
| `trade_id` | trade_id | String |
| `time` | exchange_ts_ms | RFC3339 → milliseconds |

---

#### FIX 4.2 Protocol

**Message Types Supported:**

| Type | Name | Purpose | Required Fields |
|---|---|---|---|
| `A` | Logon | Establish session | SenderCompID, TargetCompID, Heartbeat |
| `0` | Heartbeat | Keep-alive | (none) |
| `V` | MarketDataRequest | Subscribe to tickers | MDReqID, SubscriptionRequestType, Symbols |
| `W` | MarketDataSnapshot | Initial snapshot | Symbol, BidPrice, BidSize, AskPrice, AskSize |
| `X` | MarketDataIncrementalRefresh | Quote update | Symbol, BidPrice, BidSize, AskPrice, AskSize |

**Checksum Validation:**
- Tag 10: CRC32-style checksum of header + body (3 digits, zero-padded)
- Mismatch → `ParseError`

---

### 4.2 gRPC Streaming

**Proto Definition (`proto/tick_stream.proto`):**

```protobuf
service TickStreamService {
  rpc SubscribeTicks(SubscribeTicksRequest) returns (stream Tick);
}

message SubscribeTicksRequest {
  string symbol = 1;      // "" = all symbols
  string exchange = 2;    // "" = all exchanges
}

message Tick {
  string exchange = 1;
  string symbol = 2;
  string price = 3;
  string quantity = 4;
  string side = 5;
  int64 exchange_ts_ms = 6;
  int64 received_at_ms = 7;
}
```

**Backpressure:** Slow subscribers skip buffered ticks (broadcast buffer size = 1024 by default)

---

## 5. Performance Characteristics

### 5.1 Algorithmic Complexity

| Operation | Time Complexity | Space | Notes |
|---|---|---|---|
| Tick normalization | O(1) | O(1) | Single-pass parsing; fixed output size |
| Ring buffer push/pop | O(1) | O(N) | N-sized array; amortized constant |
| Order book delta apply | O(k log n) | O(n) | k levels; n total levels in book |
| OHLCV bar aggregation | O(1) per tick | O(S) | S = # active symbols |
| Rolling volatility update | O(1) | O(W) | W = window size; Welford's algorithm |
| Regime detection | O(b log b) | O(b) | b = bar history (100 bars typical) |
| Cointegration test | O(T²) | O(T) | T = tick window for regression |

### 5.2 Throughput Benchmarks

**Platform:** 3.6 GHz AMD Zen 3 (Ryzen 5950X)  
**Setup:** Single-threaded microbenchmarks; no allocations on hot path

| Component | Throughput | Latency (p50) | Latency (p99) |
|---|---|---|---|
| SPSC ring push/pop | 150M pairs/sec | ~7 ns | ~20 ns |
| Tick normalization | >10M ticks/sec | ~100 ns | ~500 ns |
| Order book delta apply | 5M updates/sec | ~200 ns | ~1000 ns |
| OHLCV bar close | >20M bars/sec | ~50 ns | ~200 ns |
| Rolling volatility (W=100) | >5M updates/sec | ~200 ns | ~500 ns |

**Design Target:** 100K+ ticks/sec end-to-end (achieved >1M in isolation)

---

## 6. Constraints & Invariants

### 6.1 Type-Level Invariants

| Invariant | Checked | Enforcement |
|---|---|---|
| `price > 0` | Runtime | On parse; `ParseError` if violated |
| `quantity > 0` | Runtime | On parse; `ParseError` if violated |
| `NormalizedTick` uniqueness | Semantic | No guarantees; application-level dedup required |
| `OrderBook.bid < OrderBook.ask` | Runtime | On `apply_update()`; `BookCrossed` if violated |
| SPSC ring capacity | Compile-time | `const N >= 2` assertion |
| Window size > 0 | Runtime | `ConfigError` if violated |

### 6.2 Numeric Precision

| Type | Precision | Strategy |
|---|---|---|
| Prices/quantities | Full Decimal | No truncation; full significant digits preserved |
| Timestamps | Milliseconds | u64; overflows ~285 million years |
| Ratios/metrics | f64 (double) | 53-bit mantissa; sufficient for risk metrics |
| Volatility | % annualized | e.g., 0.25 = 25% |
| Correlation | [-1, 1] | Pearson r; Welford's algorithm for stability |

---

## 7. Versioning & Stability

### 7.1 Semantic Versioning

- **Major (2.x):** Breaking API changes (rare after 2.0)
- **Minor (x.11):** New features, backward compatible
- **Patch (2.11.x):** Bug fixes, internal refactoring

### 7.2 Stability Guarantees

| API | Stability | Notes |
|---|---|---|
| `NormalizedTick` fields | Stable (JSON serialization) | Fields won't change; new fields append-only |
| `OrderBook` interface | Stable | Core methods frozen; new methods additive |
| `StreamError` variants | Stable | Variants won't be removed; new ones may be added |
| Ring buffer API | Stable | Core methods won't change |
| Exchange support | Stable | Won't remove existing exchanges; new ones additive |

### 7.3 MSRV Policy

- **MSRV:** 1.75 (enforced in CI)
- **Bump policy:** Only on major version increments; advance notice in changelog
- **Rationale:** Const generics and async/await require 1.75+

---

## 8. Refactoring Technical Specifications

### 8.1 Trait-Based Architecture

#### Analyzer Trait (Core Abstraction)
```rust
pub trait Analyzer: Send + Sync {
    /// Process a single normalized tick
    fn process(&mut self, tick: &NormalizedTick) -> Result<AnalysisResult, AnalysisError>;
    
    /// Reset internal state (for backtesting/replay)
    fn reset(&mut self);
    
    /// Query current metrics
    fn metrics(&self) -> AnalysisMetrics;
    
    /// Display name for logging/debugging
    fn name(&self) -> &'static str;
}

pub trait AnalyzerComposer {
    /// Compose multiple analyzers into a pipeline
    fn compose(analyzers: Vec<Box<dyn Analyzer>>) -> Self;
}

pub trait AnalyzerMetrics {
    /// Retrieve all emitted metrics
    fn all_metrics(&self) -> Vec<MetricSnapshot>;
}
```

**Rationale**: Trait objects enable:
- Generic analyzer pipelines (no trait specifics per analyzer)
- Easy mocking for tests
- Dynamic analyzer loading (runtime composition)
- Third-party analyzer ecosystem

#### Storage Backend Trait (Testability)
```rust
pub trait StorageBackend: Send + Sync {
    /// Insert or update value for key
    fn insert(&self, key: String, value: Vec<u8>) -> Result<(), StorageError>;
    
    /// Retrieve value for key
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    
    /// Check if key exists
    fn contains_key(&self, key: &str) -> bool;
}

pub struct DashMapBackend { /* ... */ }  // Production: High-performance concurrent
pub struct MutexBackend { /* ... */ }    // Testing: Deterministic, mockable
pub struct MockBackend { /* ... */ }     // Testing: Complete control
```

**Rationale**: Decouples storage implementation from business logic:
- Easy testing (mock backend)
- Different deployment profiles (DashMap for prod, mock for tests)
- Future: Redis-backed, distributed storage

#### Aggregator Trait Segregation (ISP)
```rust
// Before (14-method trait)
pub trait BarAggregator {
    fn push_tick(&mut self, tick: &NormalizedTick) -> Option<OhlcvBar>;
    fn flush(&mut self) -> Vec<OhlcvBar>;
    fn reset(&mut self);
    fn metrics(&self) -> AggregatorMetrics;
    fn current_bar(&self) -> Option<&OhlcvBar>;
    // ... 9 more methods
}

// After (3 focused traits)
pub trait BarAggregation {
    fn push_tick(&mut self, tick: &NormalizedTick) -> Option<OhlcvBar>;
}

pub trait BarHistory {
    fn current_bar(&self) -> Option<&OhlcvBar>;
    fn last_n_bars(&self, n: usize) -> Vec<OhlcvBar>;
}

pub trait BarMetrics {
    fn metrics(&self) -> AggregatorMetrics;
}
```

**Rationale**: ISP compliance - clients use only needed methods

### 8.2 Modularization Structure

#### New Crate: fin_stream_analytics_signal
**Modules**: Momentum, MeanReversion, Volatility, CointegrationPairs  
**Public**: `Analyzer` impls, configuration builders  
**Dependencies**: fin-stream-core  
**Purpose**: Standalone signal generation; reusable in other projects

#### New Crate: fin_stream_analytics_microstructure
**Modules**: Spread, Liquidity, OrderFlowImbalance, Toxicity  
**Public**: `Analyzer` impls, configuration builders  
**Dependencies**: fin-stream-core  
**Purpose**: Market microstructure analysis; independent usage

#### New Crate: fin_stream_analytics_regime
**Modules**: RegimeDetector, TrendFollowing, MeanReversion  
**Public**: `Analyzer` impls, configuration builders  
**Dependencies**: fin-stream-core  
**Purpose**: Market regime classification; reusable detection

#### New Crate: fin_stream_analytics_risk
**Modules**: GreeksComputation, PortfolioRisk, StressTest  
**Public**: `Analyzer` impls, Greeks structs, Risk traits  
**Dependencies**: fin-stream-core, (optional) fin-stream-trading  
**Purpose**: Risk management; derivatives portfolio analytics

#### Backward Compatibility Facade (fin_stream_analytics)
```rust
// Re-export all public types
pub use fin_stream_analytics_signal::*;
pub use fin_stream_analytics_microstructure::*;
pub use fin_stream_analytics_regime::*;
pub use fin_stream_analytics_risk::*;

#[deprecated(since = "2.10.0", note = "Use fin_stream_analytics_signal::MomentumAlpha")]
pub use fin_stream_analytics_signal::MomentumAlpha;
```

### 8.3 Code Consolidation Patterns

#### Validators Module (DRY)
```rust
// Before: 50+ repetitions of validation
if price <= Decimal::ZERO {
    return Err(ParseError::InvalidPrice);
}

// After: Centralized
validators::validate_positive_price(price)?;
```

#### Generic RollingWindow<T> (DRY)
```rust
// Before: 5 implementations (WindowI32, WindowF64, WindowDecimal, etc.)
// After: Single generic
pub struct RollingWindow<T> { /* ... */ }

// Specializations zero-cost via monomorphization
let window_i32 = RollingWindow::<i32>::new(100);
let window_f64 = RollingWindow::<f64>::new(100);
```

### 8.4 Performance Specifications (Post-Refactoring)

| Metric | Pre-Refactoring | Post-Refactoring | Target |
|--------|-----------------|------------------|--------|
| Trait method overhead | N/A | <1% (vtable) | <1% |
| Storage abstraction cost | 0% | <2% | <2% |
| Generic specialization | 0% (no generics) | 0% (monomorphization) | 0% |
| Modularization penalty | N/A | <1% | <1% |
| **Total regression** | Baseline | <2% | <2% |
| **Target improvement** | 100K ticks/sec | +10-15% | 110-115K |

**Verification**: Criterion benchmarks comparing old vs. new

### 8.5 Testing Specifications

#### Unit Tests (Trait-Based)
```rust
#[test]
fn test_analyzer_with_mock_backend() {
    let backend = MockBackend::new();
    let analyzer = MomentumAlpha::with_backend(backend);
    // ... test without real DashMap
}
```

#### Integration Tests
```rust
#[test]
fn test_analyzer_composition() {
    let analyzers: Vec<Box<dyn Analyzer>> = vec![
        Box::new(MomentumAlpha::default()),
        Box::new(VolatilityAnalyzer::default()),
    ];
    let composer = PipelineComposer::compose(analyzers);
    // ... test composition
}
```

#### Performance Tests
```rust
#[bench]
fn bench_refactored_vs_old(b: &mut Bencher) {
    // Compare with criterion
    // Ensure <2% regression
}
```

### 8.6 Migration Guide (User-Facing)

#### Before (Old API)
```rust
use fin_stream_analytics::{MomentumAlpha, MicrostructureMonitor};

let signal = MomentumAlpha::default();
let microstructure = MicrostructureMonitor::default();
```

#### After (New API - Trait-Based)
```rust
use fin_stream_analytics::Analyzer;
use fin_stream_analytics_signal::MomentumAlpha;
use fin_stream_analytics_microstructure::MicrostructureMonitor;

let mut signal: Box<dyn Analyzer> = Box::new(MomentumAlpha::default());
let mut microstructure: Box<dyn Analyzer> = Box::new(MicrostructureMonitor::default());

// Compose into pipeline
let mut analyzers = vec![signal, microstructure];
```

#### Compatibility Layer (v2.10.0 - v3.0.0)
```rust
// Old code still works with deprecation warnings
#[deprecated(since = "2.10.0", note = "Import from fin_stream_analytics_signal")]
pub use fin_stream_analytics_signal::MomentumAlpha;
```

---

## 9. Research-Based Implementation Patterns

### 9.1 DashMap Performance Optimization

Per RUST_BEST_PRACTICES.md research findings:

**Current Pattern (Replace)**:
```rust
// 15-20x SLOWER than DashMap
use std::sync::RwLock;
use std::collections::HashMap;

let state = RwLock::new(HashMap::new());
state.write().unwrap().insert(symbol, order_book);
```

**Optimized Pattern (Use)**:
```rust
// Already in dependency tree
use dashmap::DashMap;

let state = DashMap::new();
state.insert(symbol, order_book); // Concurrent, lock-free
```

**Verification**: Replace all 50+ RwLock<HashMap> sites with DashMap for 15-20x throughput improvement

### 9.2 Static vs Dynamic Dispatch Strategy

**Hot Path - Static Dispatch (0% overhead)**:
```rust
pub trait Analyzer: Send + Sync {
    fn analyze(&mut self, tick: &NormalizedTick) -> Signal;
}

pub struct MomentumAnalyzer<N: Copy> { window: RollingWindow<N> }

impl Analyzer for MomentumAnalyzer<f64> {
    #[inline]
    fn analyze(&mut self, tick: &NormalizedTick) -> Signal {
        // Inlined, zero-cost
    }
}
```

**Pluggable Path - Dynamic Dispatch (1-2% overhead)**:
```rust
pub fn create_analyzer(name: &str) -> Box<dyn Analyzer> {
    match name {
        "momentum" => Box::new(MomentumAnalyzer::default()),
        _ => panic!("Unknown analyzer"),
    }
}
```

### 9.3 Generic RollingWindow Zero-Cost Abstraction

Per RUST_BEST_PRACTICES.md - generics compile away via monomorphization:
```rust
pub struct RollingWindow<T: Copy + Default> {
    data: VecDeque<T>,
    sum: f64,
    sum_sq: f64,
}

impl RollingWindow<f64> {
    pub fn new(capacity: usize) -> Self { /* ... */ }
}

impl RollingWindow<Decimal> {
    pub fn new(capacity: usize) -> Self { /* ... */ }
}

// Monomorphization: generates separate optimized code per type
let window_f64 = RollingWindow::<f64>::new(100);     // Fast f64 path
let window_dec = RollingWindow::<Decimal>::new(100); // Precision Decimal path
```

### 9.4 Storage Abstraction for Testability

```rust
pub trait StorageBackend: Send + Sync {
    fn get(&self, key: &str) -> Option<OrderBook>;
    fn insert(&self, key: str, value: OrderBook);
    fn remove(&self, key: &str);
    fn contains(&self, key: &str) -> bool;
}

// Production: DashMap-backed
pub struct DashMapBackend { map: DashMap<String, OrderBook> }

impl StorageBackend for DashMapBackend {
    fn get(&self, key: &str) -> Option<OrderBook> {
        self.map.get(key).map(|v| v.clone())
    }
    // ...
}

// Testing: In-memory
pub struct MemBackend { map: std::collections::HashMap<String, OrderBook> }

impl StorageBackend for MemBackend {
    fn get(&self, key: &str) -> Option<OrderBook> {
        self.map.get(key).cloned()
    }
    // ...
}
```

### 9.5 Validator Consolidation (DRY)

```rust
// Centralized validators module
pub mod validators {
    pub fn validate_price(p: Decimal) -> Result<Decimal, ParseError> {
        if p <= Decimal::ZERO {
            Err(ParseError::InvalidPrice(p))
        } else {
            Ok(p)
        }
    }

    pub fn validate_qty(q: Decimal) -> Result<Decimal, ParseError> {
        if q <= Decimal::ZERO {
            Err(ParseError::InvalidQuantity(q))
        } else {
            Ok(q)
        }
    }

    pub fn validate_timestamp(ts: u64, now: u64) -> Result<u64, ParseError> {
        if ts > now + 60_000 { // 60s tolerance
            Err(ParseError::FutureTimestamp(ts))
        } else {
            Ok(ts)
        }
    }

    #[inline]
    pubfn validate_symbol(s: &str) -> Result<&str, ParseError> {
        if s.is_empty() || s.len() > 12 {
            Err(ParseError::InvalidSymbol(s.to_string()))
        } else {
            Ok(s)
        }
    }
}

// Usage: replace 50+ validation sites
validators::validate_price(price)?;
validators::validate_qty(qty)?;
validators::validate_timestamp(ts, now)?;
```

### 9.6 Performance Verification Benchmarks

Per RUST_BEST_PRACTICES.md - use criterion for benchmarking:
```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_analyzer_hot_path(c: &mut Criterion) {
    let mut analyzer = MomentumAlpha::default();
    let tick = NormalizedTick::random();

    c.bench_function("analyzer_single_tick", |b| {
        b.iter(|| analyzer.analyze(black_box(&tick)))
    });
}

fn bench_storage_dashmap(c: &mut Criterion) {
    let store = DashMap::new();
    let book = OrderBook::default();

    c.bench_function("dashmap_concurrent_insert", |b| {
        b.iter(|| store.insert(black_box("BTC-USD"), black_box(book.clone())))
    });
}
```

**Performance Targets**: <2% regression on all hot paths verified with criterion

---

## 10. Current State & Verified Technical Findings

### 10.1 Codebase Structure Baseline (Verified via Exploration)

**6 Rust Crates, 117 Files, ~141K LOC**

| Crate | Purpose | Modules | LOC Estimate |
|-------|---------|---------|--------------|
| fin-stream-core | Tick normalization, OHLCV, order books, health | tick, ohlcv, aggregator, book, health, latency, session, ring | 35K |
| fin-stream-net | WebSocket, FIX protocol, gRPC, portfolio feeds | ws, fix, grpc, portfolio_feed, circuit_breaker | 20K |
| fin-stream-trading | Risk engine, position manager, backtest, market maker | risk_engine, position_manager, backtest, market_maker, statarb, pairs | 25K |
| fin-stream-analytics | Feature engineering, microstructure, anomaly detection | norm, microstructure, volatility, alpha, anomaly, circuit | 45K |
| fin-stream-simulation | Market simulator, order book replay, snapshots | market_simulator, replay, lob_sim, snapshot | 10K |
| fin-stream-experimental | Research features (synthetic, MEV, news, compression) | synthetic, mev, lorentz, news, compression | 6K |

### 10.2 SOLID Compliance Audit (Current: ~60%, Target: 90%+)

#### Single Responsibility Principle - 5 Critical Violations

| File | Lines | Issue | Fix |
|------|-------|-------|-----|
| TickNormalizer | 11463-11564 | Handles 4 exchange parsers + formatting + caching | Extract ExchangeParser trait |
| NormalizedTick | 100-700 | 50+ methods with overlapping functionality | Break into focused interfaces |
| OrderBook::apply() | 128-195 | Validation + delta application + sequence tracking | Separate validators |
| RiskEngine | 160-180 | Greeks + price storage + portfolio aggregation | Extract Greek computation |
| MinMaxNormalizer | 63-300+ | Window state + caching + recomputation | Separate layers |

**Impact**: Each violation increases code coupling and makes testing harder. Fixing will reduce average method count per module by 30-40%.

#### Open/Closed Principle - 3 Critical Violations

| Issue | File | Fix |
|-------|------|-----|
| Exchange parsers require code modification | tick/mod.rs | Strategy: ExchangeParser trait |
| Arbitrage checking hard-coded | multi_exchange/mod.rs | Strategy: ArbitrageDetector trait |
| Order routing not extensible | order_router/mod.rs | Strategy: RoutingAlgorithm trait |

**Impact**: Adding new exchanges/strategies currently requires core changes. Trait-based approach enables third-party implementations.

#### Dependency Inversion Principle - 3 Critical Violations

| Violation | File | Current | Fix |
|-----------|------|---------|-----|
| PositionManager depends on concrete DashMap | position_manager/mod.rs:173 | `DashMap<String, PositionState>` | PositionStorage trait |
| MultiExchangeAggregator uses concrete DashMap | multi_exchange/mod.rs:180 | `Arc<DashMap<Exchange, State>>` | StorageBackend trait |
| DataPipeline uses concrete VecDeque | data_pipeline/mod.rs:50+ | `VecDeque<f64>` | HistoryStorage trait |

**Impact**: Direct DashMap coupling (113 sites) makes testing expensive. StorageBackend trait enables in-memory test doubles and Redis-backed variants.

### 10.3 DRY Violations Quantified (50+ Repetitions)

#### Validation Logic Duplication (~8+ instances)
- **Price/qty validation**: tick_filter, quality, aggregator, book (4 independent implementations)
- **Stale data checks**: health, data_pipeline, tick_filter (3 independent implementations)
- **Exchange parsing**: Binance, Coinbase, Alpaca, Polygon parsers + 2 other sites (6 implementations)
- **Timestamp conversion**: 6+ independent RFC3339 parsers across modules

**Fix**: Centralized validators module eliminates ~500 LOC duplication

#### Exchange Parser Duplication (4 parsers, ~600 LOC)

All 4 parsers follow identical structure:
1. Extract decimal field from JSON
2. Parse trade side / trade ID
3. Handle exchange-specific timestamp format
4. Construct NormalizedTick

**Pattern**: 
- Binance: `normalize_binance()` at tick/mod.rs:11463-11486 (23 lines)
- Coinbase: `normalize_coinbase()` at tick/mod.rs:11488-11519 (31 lines)
- Alpaca: `normalize_alpaca()` at tick/mod.rs:11521-11542 (21 lines)
- Polygon: `normalize_polygon()` at tick/mod.rs:11544-11564 (20 lines)

**Fix**: Extract FieldExtractor trait; implement once per exchange (~10-12 lines per exchange)

#### Configuration Pattern Duplication (15+ structs, ~800 LOC)

All config structs follow identical pattern:
```
validation in new() → stored in HashMap/DashMap → with_* builders → serialization
```

| Struct | File | Lines | Pattern |
|--------|------|-------|---------|
| HaltConfig | circuit/mod.rs:32-65 | 33 | validate() + builders + serialize |
| VpinConfig | toxicity/mod.rs:87-145 | 59 | validate() + builders + serialize |
| NoiseConfig | noise/mod.rs:54-85 | 31 | validate() + builders + serialize |
| AnomalyDetectorConfig | anomaly/mod.rs:96-140 | 44 | validate() + builders + serialize |
| +11 more | Various | ~30-60 ea | Same pattern |

**Fix**: Generic ConfigBuilder<T> macro or procedural macro generates ~10 lines of boilerplate per config

#### Trait Implementation Duplication (40+ Display, 30+ FromStr)

- **Display**: All 40+ follow `match self { Variant => write!(...) }` pattern
- **FromStr**: All 30+ follow `match s.to_lowercase()` pattern with identical fallback logic
- **Default**: All 25+ wrap `.new()` call

**Fix**: Derive macros (Display, FromStr via serde) eliminate ~500 LOC

### 10.4 Cyclomatic Complexity Analysis (10 functions with CC > 8)

**Priority 1 - Refactor Immediately** (CC > 12):

| Function | File | CC | Lines | Issues |
|----------|------|-----|-------|--------|
| normalize() | tick/mod.rs:11424-11462 | ~18 | 38 | 4 exchange branches + nested parsing |
| apply() | book/mod.rs:158-195 | ~12 | 37 | Symbol check + sequence + crossed-book |
| normalize_tick() | data_pipeline/mod.rs:330-380 | ~14 | 50 | Spike detection + quality + history |

**Priority 2 - Refactor Next** (CC 8-12):

| Function | File | CC | Fix |
|----------|------|-----|-----|
| process_trade() | aggregator/bars.rs:155-200 | ~10 | Extract to TradeProcessor |
| check_thresholds() | circuit/mod.rs:238-280 | ~12 | Extract ThresholdValidator |
| update_position() | position_manager/mod.rs:200-260 | ~11 | Extract PositionUpdate |

**Refactoring Strategy**: Extract to Strategy/Validator traits; reduce CC to ~4-6 per function

### 10.5 Trait Abstraction Coverage

**Current State**: 13 public traits, ~101 implementations

| Trait | Implementors | Issue |
|-------|-------------|-------|
| BarPriceAnalytics | OhlcvBar only | Over-abstraction; should be internal |
| BarShapeAnalytics | OhlcvBar only | Over-abstraction; should be internal |
| BarPatternAnalytics | OhlcvBar only | Over-abstraction; should be internal |
| BarTrendAnalytics | OhlcvBar only | Over-abstraction; should be internal |
| BarSliceAnalytics | OhlcvBar only | Over-abstraction; should be internal |
| BarVolatilityAnalytics | OhlcvBar only | Over-abstraction; should be internal |
| TickSource | FileTickSource, TestTickSource | ✓ Healthy |
| StatisticalWindow | RollingWindow | ✓ Healthy |
| PriceModel | GBMPriceModel, MeanRevertingModel | ✓ Healthy |
| NewsSource | MockNewsSource | ✓ Healthy |
| TickFilter | User closures | ✓ Healthy |
| TickTransform | User closures | ✓ Healthy |
| SecretProvider | EnvSecretProvider + custom | ✓ Healthy |

**Gap Analysis**: Missing 50+ essential traits:
- ExchangeParser (4 implementations currently embedded)
- StorageBackend (113 DashMap usages)
- ConfigProvider (15+ config structs)
- ValidatorRegistry (25+ validation functions)
- AnalyzerPlugin (37 analytics modules)
- BarAggregator (multiple aggregation modes)
- RiskCalculator (Greeks computation)
- RoutingStrategy (order routing logic)

### 10.6 Complexity Metrics

| Metric | Current | Healthy Range | Status |
|--------|---------|---|---------|
| Avg functions per file | 35-50 | 20-30 | ⚠️ High |
| Max file size | 36,076 lines (norm/mod.rs) | <3000 | 🔴 Critical |
| Methods per type | NormalizedTick: 50+, OhlcvBar: 200+ (inherited) | <20 | 🔴 Critical |
| Trait implementations | 40+ Display, 30+ FromStr | <5 variants | ⚠️ High |
| DashMap coupling | 113 sites in 22 files | <5 | 🔴 Critical |

### 10.7 KISS Principle Violations

**Over-Complex Patterns**:

1. **TickNormalizer.normalize()** (38 lines, CC 18):
   - 4 exchange-specific branches with nested JSON parsing
   - Each branch duplicates: extract field → parse side → handle timestamp → construct tick
   - Solution: ExchangeParser trait reduces to ~8-10 lines per implementation

2. **OrderBook.apply()** (37 lines, CC 12):
   - Combines symbol validation, sequence checking, delta application, crossed-book detection
   - Solution: Separate validators; core logic ~15 lines

3. **6 Bar Analytics Traits**:
   - BarPriceAnalytics, BarShapeAnalytics, BarPatternAnalytics, BarTrendAnalytics, BarSliceAnalytics, BarVolatilityAnalytics
   - All implemented by single type (OhlcvBar) - over-abstraction
   - Clients forced to implement patterns 5-6 times
   - Solution: Consolidate into OhlcvAnalytics trait with 6 sub-methods

4. **23+ Config Structs**:
   - Each 30-60 lines with identical builder pattern
   - ~80% duplication across validators, builders, serialization
   - Solution: Generic ConfigBuilder<T> or procedural macro

5. **normalize_tick()** (50 lines, CC 14):
   - Combines spike detection, quality flag assignment, history management
   - Solution: Separate QualityChecker, SpikeDetector; core ~20 lines

### 10.8 YAGNI Violations

**Potentially Unnecessary Features**:

1. **Looping Replay** (~5% estimated usage):
   - Autostart feature for backtesting
   - Recommendation: Keep but move to optional feature flag

2. **Multi-tenant Isolation** (not yet implemented):
   - Per-portfolio sandboxing; significant complexity
   - Recommendation: Defer to Phase 4 or post-release

3. **GPU Acceleration Research** (aspirational):
   - Listed in performance roadmap; no current work
   - Recommendation: Defer to post-release Q4 2026

4. **Arrow IPC Export** (high effort, medium priority):
   - ML feature export to Python
   - Recommendation: Defer to Phase 4

5. **Experimental Modules** (6 crates, ~6K LOC):
   - synthetic, mev, lorentz, news, compression
   - Recommendation: Keep but document as unstable/research-only; move to separate experimental workspace

### 10.9 Git History Insights

**Recent Development Pattern** (384 commits in 30 days):

- **Rapid iteration**: ~55 commits/day during active sprint (March 17-23, 2026)
- **Systematic refactoring**: Rounds 49-67 systematically eliminate duplication
- **No merge conflicts**: Clean linear history on main
- **Active development**: Workspace modernization branch with significant structural changes

**Refactoring Pattern Observed**:
- Use `map_or`, `let-else`, and accessor delegation to reduce code duplication
- Systematic consolidation of inline implementations
- Pattern: Extract → Delegate → Consolidate

**CI/Build Observations**:
- Recent fixes for clippy, cargo-deny, codecov (now stable)
- Build system robust; ready for aggressive refactoring with confidence

---

## 11. Prek Tooling Integration & Quality Automation

### 11.1 Prek Framework Architecture

**Configuration File**: `.pre-commit-config.yaml` (root directory)

**Hook Stages**:

1. **prek stage 1** (Every commit, ~8-12s)
   - Runs on staged changes only
   - Blocks commit if checks fail
   - Fast hooks prioritized: Rustfmt (1-2s), Clippy (5-10s)
   - Format MUST run before lint (formatting changes affect lints)

2. **prek stage 2** (Before push, ~10-15s)
   - Runs on unpushed commits
   - Blocks push if checks fail
   - Comprehensive hooks: Cargo Audit (0.5s), Cargo Deny (1-2s)
   - Final quality gate before remote

3. **commit-msg stage** (Optional)
   - Validates commit message format
   - Could enforce conventional commits
   - Status: Optional for Phase 1

4. **post-commit stage** (Optional)
   - Notifications or cleanup
   - Does not block workflow
   - Status: Not implemented

### 11.2 Hook Execution Order & Rationale

**Stage: prek stage 1 (fast, every commit)**

```yaml
1. File checks (< 100ms)
   - Pre-commit built-in checks
   - Catches obvious issues early

2. Rustfmt (1-2s) [CRITICAL: RUNS FIRST]
   - Formats Rust code
   - MUST run before Clippy
   - Reason: Formatting changes affect lints
   - Argument: --edition=2021

3. Taplo (0.2s) [AFTER formatting]
   - Formats TOML files
   - Dependencies: Cargo.toml, deny.toml, clippy.toml
   - Non-blocking: Warns but doesn't fail

4. Clippy (5-10s) [FINAL lint pass]
   - Runs after formatting
   - 800+ checks enabled
   - Arguments: --all-targets --all-features -- -D warnings
   - Blocks on any warning
```

**Stage: prek stage 2 (comprehensive, before push)**

```yaml
1. Cargo Audit (0.5s)
   - Scans for known security vulnerabilities
   - Database: RustSec advisory-db
   - Blocks on any vulnerability found
   - Vulnerability threshold: DENY (no warnings)

2. Cargo Deny (1-2s)
   - Enforces dependency policy
   - Checks: licenses, ban list, sources
   - Allowed licenses: MIT, Apache-2.0, BSD-*
   - Denied licenses: GPL-*, AGPL-*
   - Blocks on policy violations
```

### 11.3 Performance Characteristics

**Timing Breakdown** (cold cache):

| Check | Time | Frequency | Stage |
|-------|------|-----------|-------|
| Rustfmt | 1-2s | prek stage 1 | 100% of commits |
| Clippy | 5-10s | prek stage 1 | 100% of commits |
| Cargo Audit | 0.5s | prek stage 2 | 100% of pushes |
| Cargo Deny | 1-2s | prek stage 2 | 100% of pushes |
| Taplo | 0.2s | prek stage 1 | 100% of commits |
| Typos | 0.3s | prek stage 2 | 100% of pushes |

**Total**: 
- **prek stage 1**: ~8-12s (Rustfmt + Taplo + Clippy)
- **prek stage 2**: ~10-15s (Audit + Deny + optional Typos)
- **Cached**: ~2-3s (most files unchanged)

**Optimization Strategies**:

1. **Only check modified files**: Pre-commit checks staged files by default
2. **Parallel execution**: Pre-commit runs checks in parallel when possible
3. **Caching**: Tools cache results (Clippy incremental compile)
4. **Skip on specific branches**: Prek stage 2 skipped on documentation-only branches

### 11.4 Configuration Files

**`.pre-commit-config.yaml`** (Main prek configuration):
- Location: project root
- Format: YAML
- Specifies: repos, hooks, stages, args
- Size: ~80 lines for production setup

**`clippy.toml`** (Clippy configuration):
- Location: project root
- Format: TOML
- Specifies: lint thresholds, doc identifiers, complexity limits
- Size: ~10 lines

**`rustfmt.toml`** (Rustfmt configuration):
- Location: project root
- Format: TOML
- Specifies: edition, line width, indentation, shorthand options
- Size: ~15 lines

**`deny.toml`** (Cargo Deny configuration):
- Location: project root
- Format: TOML
- Specifies: allowed/denied licenses, advisory DB, ban list, sources
- Size: ~40 lines

### 11.5 CI/CD Integration

**GitHub Actions Workflow** (`.github/workflows/prek.yml`):

```yaml
name: prek quality gates
on: [pull_request, push]
jobs:
  prek:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: pre-commit/action@v3
```

**Strategy**:
- Runs on every PR and push
- Uses pre-commit action (official, maintained)
- Caches prek environment (< 5s on warm cache)
- Reports per-hook results
- Blocks merge on failure

**Fallback for Local Development**:
- If prek not installed: `cargo build && cargo test` must pass
- CI catches prek failures for non-local developers

### 11.6 Tool Versions & Maintenance

**Version Pinning Strategy**:
- `.pre-commit-config.yaml` pins all hook versions
- Rationale: Prevent unexpected linting changes
- Update: Monthly security reviews, quarterly feature updates
- Process: Pin to latest stable; test locally; update CI

**Example Version Pins** (as of 2026-04-12):
- Rustfmt: v1.8.0
- Clippy: stable (via rust-lang repo)
- Cargo Audit: latest
- Cargo Deny: v0.14.0
- Taplo: v0.9.3
- Typos: v1.23.0

---
