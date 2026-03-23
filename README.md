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

**v2.4.0** — 40 K+ lines of production Rust. Includes an extensive analytics
suite: 200+ static analytics on `NormalizedTick`, 200+ on `OhlcvBar`, and 80+
rolling-window analytics on `MinMaxNormalizer` and `ZScoreNormalizer` (rounds 1–88).

## What Is Included

| Module | Purpose | Key types |
|---|---|---|
| `ws` | WebSocket connection lifecycle with exponential-backoff reconnect and backpressure | `WsManager`, `ConnectionConfig`, `ReconnectPolicy` |
| `tick` | Convert raw exchange payloads (Binance/Coinbase/Alpaca/Polygon) into a single canonical form; 200+ batch analytics on tick slices | `RawTick`, `NormalizedTick`, `Exchange`, `TradeSide`, `TickNormalizer` |
| `ring` | Lock-free SPSC ring buffer: zero-allocation hot path between normalizer and consumers | `SpscRing<T, N>`, `SpscProducer`, `SpscConsumer` |
| `book` | Incremental order book delta streaming with snapshot reset and crossed-book detection | `OrderBook`, `BookDelta`, `BookSide`, `PriceLevel` |
| `ohlcv` | Bar construction at any `Seconds / Minutes / Hours` timeframe with optional gap-fill bars; 200+ batch analytics on bar slices | `OhlcvAggregator`, `OhlcvBar`, `Timeframe` |
| `health` | Per-feed staleness detection with configurable thresholds and a circuit-breaker | `HealthMonitor`, `FeedHealth`, `HealthStatus` |
| `session` | Trading-status classification (Open / Extended / Closed) for US Equity, Crypto, Forex | `SessionAwareness`, `MarketSession`, `TradingStatus` |
| `norm` | Rolling min-max and z-score normalizers for streaming observations; 80+ analytics each (moments, percentiles, entropy, trend, etc.) | `MinMaxNormalizer`, `ZScoreNormalizer` |
| `lorentz` | Lorentz spacetime transforms for feature engineering on price-time coordinates | `LorentzTransform`, `SpacetimePoint` |
| `correlation` | Streaming NxN Pearson correlation matrix; O(N) update via Welford's algorithm; DashMap-backed for concurrent feed updates | `StreamingCorrelationMatrix`, `CorrelationPair` |
| `fix` | FIX 4.2 session adapter: parse/serialize frames, validate checksum (tag 10), Logon, MarketDataRequest, Snapshot/Refresh → NormalizedTick | `FixSession`, `FixParser`, `FixMessage`, `FixError` |
| `portfolio_feed` | Multi-asset parallel WebSocket feed; JoinSet-managed per-asset WsManager tasks; exponential-backoff restart; merged tick channel | `PortfolioFeed`, `AssetFeedConfig`, `AssetFeedStats` |
| `mev` | MEV detection scaffold: sandwich, frontrun, and backrun heuristics on tick slices; no Flashbots API required | `MevDetector`, `MevCandidate`, `MevPattern` |
| `toxicity` | Order flow toxicity: PIN, VPIN, Kyle λ, Amihud illiquidity — four-metric smart-money detection | `OrderFlowToxicityAnalyzer`, `ToxicityMetrics`, `VpinCalculator` |
| `ofi` | Order flow imbalance: per-tick OFI from top-of-book delta, rolling accumulator, z-score standardization, VPIN | `OrderFlowImbalance`, `OfiAccumulator`, `OfiMetricsComputer`, `ToxicityEstimator`, `VpinResult` |
| `microstructure` | Market microstructure analytics: Amihud illiquidity, Kyle's lambda, Roll spread, bid-ask bounce, streaming monitor | `MicrostructureMonitor`, `AmihudIlliquidity`, `KyleImpact`, `RollSpread`, `BidAskBounce`, `MicrostructureReport` |
| `regime` | Real-time market regime classification: Trending / MeanReverting / HighVol / LowVol via Hurst + ADX + realised vol | `RegimeDetector`, `MarketRegime` |
| `synthetic` | Stochastic market data generator: GBM, jump-diffusion, OU, Heston — deterministic seeded output | `SyntheticMarketGenerator`, `GeometricBrownianMotion`, `HestonModel` |
| `multi_exchange` | NBBO-style multi-exchange aggregation; per-exchange latency divergence tracking; arbitrage opportunity detection | `MultiExchangeAggregator`, `Nbbo`, `ArbitrageOpportunity`, `AggregatorConfig` |
| `circuit_breaker` | WebSocket circuit breaker: exponential-backoff reconnect + degraded-mode synthetic tick emission after 5 failures | `WsCircuitBreaker`, `CircuitBreakerConfig`, `CircuitState` |
| `anomaly` | Streaming tick anomaly detection: price spikes (z-score), volume spikes, sequence gaps, timestamp inversions | `TickAnomalyDetector`, `AnomalyEvent`, `AnomalyKind`, `AnomalyDetectorConfig` |
| `snapshot` | Binary tick recorder and N-speed replayer for backtesting with real captured tick data | `TickRecorder`, `TickReplayer` |
| `grpc` | gRPC streaming endpoint (`grpc` feature): expose tick stream over gRPC via tonic with per-symbol/exchange filtering | `TickStreamServer` (feature-gated) |
| `quality` | Feed quality scoring: rolling latency percentiles, gap detection, duplicate detection, 0–100 composite score | `QualityScorer`, `FeedQualityMetrics`, `FeedGapDetector`, `TickDeduplicator`, `QualityReport` |
| `circuit` | Per-symbol circuit breakers: halt on price spikes or volume surges; Normal/Halted/Recovering FSM; hub manages one breaker per symbol | `SymbolCircuitBreaker`, `CircuitBreakerHub`, `HaltConfig`, `HaltReason`, `CircuitDecision`, `CircuitStats` |
| `error` | Unified typed error hierarchy covering every pipeline failure mode | `StreamError` |

## Feed Quality Scoring

`QualityScorer` tracks per-symbol latency, gap rate, and duplicate rate over a configurable
rolling window, then computes a composite score in `[0, 100]`.

**Score formula:**
```
score = 100 × (1 − gap_rate) × (1 − duplicate_rate) × exp(−latency_p99_ms / 1000)
```

```rust
use fin_stream::quality::{QualityScorer, FeedGapDetector, TickDeduplicator, DeduplicatorConfig, HashFields};

// 1. Quality scorer with 500-tick rolling window
let scorer = QualityScorer::new(500);
let mut gap_det = FeedGapDetector::new(2000);     // flag gaps > 2 s
let mut dedup   = TickDeduplicator::new(DeduplicatorConfig::new(5000, HashFields::PriceQtyTimestamp), 256);

// On each received tick:
let is_gap = gap_det.process("BTC-USD", tick.received_at_ms);
let is_dup = dedup.check(&tick) == fin_stream::quality::DedupDecision::Duplicate;
scorer.record_tick("BTC-USD", tick.received_at_ms, prev_ts, is_gap, is_dup);

// Get per-symbol metrics
if let Some(m) = scorer.metrics("BTC-USD") {
    println!("Score: {:.1}  p50: {:.1}ms  p99: {:.1}ms  gaps: {:.2}%",
        m.score, m.latency_p50_ms, m.latency_p99_ms, m.gap_rate * 100.0);
}

// Aggregated report across all symbols
let report = scorer.report();
println!("System health: {:.1}", report.system_health_score);
println!("Worst feed: {:?}", report.worst_feed);
```

## Symbol Circuit Breakers

`SymbolCircuitBreaker` monitors each symbol independently and halts processing when
price moves or volume surges exceed configured thresholds.

**State machine:** `Normal → Halted { until, reason } → Recovering → Normal`

```rust
use fin_stream::circuit::{CircuitBreakerHub, HaltConfig, CircuitDecision};
use std::time::Duration;

let config = HaltConfig::new(
    0.05,                       // 5 % price move threshold
    5.0,                        // 5× volume surge threshold
    20,                         // 20-tick rolling window
    Duration::from_secs(60),   // 60 s halt duration
);
let hub = CircuitBreakerHub::new(config);

// On each tick — per-symbol breaker created automatically:
match hub.process_tick(&tick) {
    CircuitDecision::Allow              => { /* forward tick downstream */ }
    CircuitDecision::Halt(reason)       => { tracing::warn!(?reason, "circuit halted"); }
    CircuitDecision::Recover            => { tracing::info!("circuit recovered"); }
}

// Stats across all symbols
let stats = hub.stats();
println!("Total halts: {}  Currently halted: {:?}", stats.total_halts, stats.current_halted_symbols);
```

## Streaming Correlation Matrix

`StreamingCorrelationMatrix` maintains rolling Pearson r for every pair of registered
assets. It uses Welford's online algorithm for numerically stable mean and variance, and
stores pairwise cross-covariance in a `DashMap` so multiple WebSocket feed tasks can call
`update` concurrently without a global lock.

```rust
use fin_stream::correlation::StreamingCorrelationMatrix;

let matrix = StreamingCorrelationMatrix::new(100); // 100-tick rolling window

// Feed ticks from any number of concurrent WebSocket tasks:
matrix.update("BTC-USD", 30_000.0, timestamp_ms);
matrix.update("ETH-USD",  2_000.0, timestamp_ms);

// O(1) lookup:
if let Some(r) = matrix.correlation("BTC-USD", "ETH-USD") {
    println!("Pearson r = {r:.4}");
}

// Full NxN snapshot:
let full = matrix.full_matrix(); // HashMap<(String, String), f64>

// Diversification helpers:
let hedges    = matrix.top_decorrelated("BTC-USD", 5); // most negative r
let followers = matrix.top_correlated("BTC-USD", 5);   // most positive r
```

## FIX 4.2 Adapter

`FixParser` is a stateless, `Send + Sync` FIX 4.2 frame codec. `FixSession` wraps an
async TCP connection and handles Logon, Heartbeat, and MarketData message flows.

```rust
use fin_stream::fix::{FixParser, FixSession};

// Stateless parse/serialize — share across tasks via Arc:
let parser = FixParser::new();
let msg = parser.parse(&raw_bytes)?;
let wire = parser.serialize(&msg);

// Full session:
let mut session = FixSession::connect("127.0.0.1:9878").await?;
session.logon("MY_FIRM", "BROKER").await?;
session.subscribe_market_data(&["AAPL", "MSFT"]).await?;

while let Some(fix_msg) = session.read_message().await? {
    if let Some(tick) = session.on_message(fix_msg) {
        // tick is a NormalizedTick ready for the pipeline
    }
}
```

Supported message types: `A` (Logon), `0` (Heartbeat), `V` (MarketDataRequest),
`W` (MarketDataSnapshot), `X` (MarketDataIncrementalRefresh).

## Multi-Asset Portfolio Feed

`PortfolioFeed` manages one `WsManager` per registered asset in a `JoinSet`, merges
all tick streams into a single `mpsc` channel, and provides a lock-free latest-tick
snapshot via `DashMap`.

```rust
use fin_stream::portfolio_feed::PortfolioFeed;
use fin_stream::tick::Exchange;

let mut feed = PortfolioFeed::new(256);
feed.add_asset("BTC-USD", Exchange::Coinbase).await;
feed.add_asset("ETH-USD", Exchange::Coinbase).await;
feed.add_asset("AAPL",    Exchange::Alpaca).await;

let mut rx = feed.tick_stream(); // mpsc::Receiver<(String, NormalizedTick)>
while let Some((symbol, tick)) = rx.recv().await {
    println!("{symbol}: {}", tick.price);
}

// At any time, get the latest price for every asset:
let snapshot = feed.portfolio_snapshot(); // HashMap<String, NormalizedTick>
```

Auto-reconnect with exponential backoff is applied at the portfolio level: if any
asset feed task exits due to a network error, it is restarted after a delay starting
at 500 ms and doubling up to 30 s.

## MEV Detection

`MevDetector` applies three heuristic passes to a slice of `NormalizedTick`s and
returns a `Vec<MevCandidate>` describing any patterns found.

```rust
use fin_stream::mev::{MevDetector, MevPattern};

// 0.5% price impact threshold, 20-tick lookahead window:
let detector = MevDetector::with_window(0.005, 20);
let candidates = detector.analyze_block(&block_ticks);

for c in &candidates {
    println!(
        "[{:?}] tx={} profit≈${:.2} confidence={:.0}%",
        c.detected_pattern,
        c.tx_hash,
        c.estimated_profit_usd,
        c.confidence * 100.0,
    );
}
```

| Pattern | Heuristic |
|---|---|
| `Sandwich` | Large buy → 1+ victim ticks at elevated price → large sell, both legs exceed `price_impact_threshold` |
| `Frontrun` | Small tick immediately precedes a 3× larger tick at same price (within 0.1%) |
| `Backrun` | Sell within 2 ticks of a large upward move, at an elevated but slightly lower price |

`estimated_profit_usd` is a coarse order-of-magnitude estimate (price impact × quantity);
`confidence` is in [0, 1] and saturates at 1.0 when the impact is 10× the threshold.

## Order Flow Toxicity

`OrderFlowToxicityAnalyzer` computes four complementary toxicity metrics in a single
rolling-window pass over `NormalizedTick`s, identifying when smart-money (informed)
traders are active.

| Metric | Formula | Interpretation |
|---|---|---|
| **PIN** | `α·μ / (α·μ + 2·ε)` | Fraction of order flow from informed traders (0–1) |
| **VPIN** | `mean(|V_B − V_S| / V_bucket)` over last N buckets | High-frequency toxicity proxy (0–1) |
| **Kyle λ** | `cov(ΔP, x) / var(x)` via OLS | Price impact per unit of signed flow |
| **Amihud** | `mean(|r_t| / vol_t)` | Illiquidity: price sensitivity to volume |

```rust
use fin_stream::toxicity::OrderFlowToxicityAnalyzer;

let mut analyzer = OrderFlowToxicityAnalyzer::new(
    200,  // tick window for PIN / Kyle λ / Amihud
    50,   // VPIN bucket size (volume units)
    50,   // VPIN rolling window (number of buckets)
);

// … feed NormalizedTicks …
let m = analyzer.metrics();
println!("PIN={:.3}  VPIN={:.3}  Kyle λ={:.6}  Amihud={:.8}",
         m.pin, m.vpin, m.kyle_lambda, m.amihud_illiquidity);
println!("All metrics valid: {}", m.is_valid());
```

The legacy `VpinCalculator` (single VPIN metric, tick-test classification) is
retained for backward compatibility; new code should use `OrderFlowToxicityAnalyzer`.

## Order Flow Imbalance (OFI)

`OrderFlowImbalance` computes a signed measure of buying vs. selling pressure from
top-of-book quotes. Unlike tick-test heuristics, OFI uses the full bid and ask queue
to determine who is the aggressor at each update.

### Formula

```text
OFI_t = ΔBid_Volume − ΔAsk_Volume

ΔBid_Volume:
  if bid_price improved or unchanged → Δ = bid_qty_t − bid_qty_{t-1}
  if bid_price worsened              → Δ = −bid_qty_{t-1}  (full queue disappeared)

ΔAsk_Volume:
  if ask_price improved or unchanged → Δ = ask_qty_t − ask_qty_{t-1}
  if ask_price worsened              → Δ = +ask_qty_{t-1}  (full queue disappeared)

OFI > 0  → net buying pressure (bid queue growing faster than ask queue)
OFI < 0  → net selling pressure
```

### Components

| Type | Description |
|---|---|
| `OrderFlowImbalance` | Stateful per-tick OFI calculator; tracks previous top-of-book snapshot |
| `OfiAccumulator` | Rolling window of raw OFI values; normalises via `tanh` and returns `OfiSignal` |
| `OfiMetricsComputer` | Welford online mean/variance + percentile rank for z-score standardisation |
| `ToxicityEstimator` | Volume-bucket VPIN: `|V_buy − V_sell| / V_total` per bucket |
| `TopOfBook` | Snapshot of best bid/ask (`price`, `qty`, `timestamp`); `mid_price()`, `spread()` |
| `OfiSignal` | Processed signal: `value`, `direction` (Buy/Sell/Neutral), `strength` in [0,1] |
| `VpinResult` | Bucket result: `toxicity`, `buy_vol`, `sell_vol`, `imbalance`, `is_toxic(thr)` |

```rust
use fin_stream::{OfiAccumulator, OfiMetricsComputer, OrderFlowImbalance, TopOfBook};
use rust_decimal_macros::dec;

fn main() -> Result<(), fin_stream::StreamError> {
    let mut raw   = OrderFlowImbalance::new();
    let mut accum = OfiAccumulator::new(50)?;   // 50-tick rolling window
    let mut stats = OfiMetricsComputer::new(200); // 200-tick stats window

    // Simulate top-of-book updates arriving from a WebSocket feed:
    let snap1 = TopOfBook { bid_price: dec!(50000), bid_qty: dec!(1.5),
                            ask_price: dec!(50001), ask_qty: dec!(2.0),
                            timestamp: 0 };
    let snap2 = TopOfBook { bid_price: dec!(50000), bid_qty: dec!(2.0), // bid grew
                            ask_price: dec!(50001), ask_qty: dec!(1.8),
                            timestamp: 1 };

    let ofi_raw  = raw.update(snap1);
    let ofi_raw2 = raw.update(snap2);           // = +0.5 − (−0.2) = +0.7 (net buy)

    let signal   = accum.update(ofi_raw2);      // OfiSignal { direction: Buy, strength: … }
    let metrics  = stats.update(ofi_raw2);      // OfiMetrics { zscore, percentile_rank, … }

    println!("Direction: {:?}", signal.direction);
    println!("Strength:  {:.3}", signal.strength);
    println!("Z-score:   {:.3}", metrics.zscore);
    println!("Significant (|z| > 2): {}", metrics.is_significant(2.0));
    Ok(())
}
```

### VPIN (Volume-Synchronized Probability of Informed Trading)

`ToxicityEstimator` accumulates volume into fixed-size buckets and computes VPIN
per bucket. A bucket is closed when total volume reaches `bucket_size`; VPIN is the
rolling mean of `|V_buy − V_sell| / V_total` over the last `n_buckets`.

```text
VPIN = (1/N) · Σ_b [ |V_buy_b − V_sell_b| / (V_buy_b + V_sell_b) ]

V_buy  = volume attributed to buyer-initiated trades in bucket b
V_sell = volume attributed to seller-initiated trades
N      = number of complete buckets in the rolling window

VPIN ∈ [0, 1];  VPIN > 0.5 → elevated toxicity (informed trading)
```

```rust
use fin_stream::ToxicityEstimator;

let mut vpin = ToxicityEstimator::new(1_000.0, 50); // 1 000 vol/bucket, 50-bucket window

// Feed (ofi_value, volume) pairs from your tick pipeline:
// if let Some(result) = vpin.update(0.7, 200.0) {
//     if result.is_toxic(0.5) {
//         println!("Elevated toxicity: VPIN={:.3}", result.toxicity);
//     }
// }
```

### API reference — `ofi` module

```rust
OrderFlowImbalance::new() -> OrderFlowImbalance
OrderFlowImbalance::update(&mut self, snap: TopOfBook) -> f64
OrderFlowImbalance::reset(&mut self)
OrderFlowImbalance::tick_count(&self) -> u64

OfiAccumulator::new(window_size: usize) -> Result<Self, StreamError>
OfiAccumulator::update(&mut self, raw_ofi: f64) -> OfiSignal

OfiMetricsComputer::new(lookback: usize) -> OfiMetricsComputer
OfiMetricsComputer::update(&mut self, raw_ofi: f64) -> OfiMetrics
OfiMetrics::is_significant(&self, z_threshold: f64) -> bool

ToxicityEstimator::new(bucket_size: f64, n_buckets: usize) -> ToxicityEstimator
ToxicityEstimator::update(&mut self, ofi: f64, volume: f64) -> Option<VpinResult>
VpinResult::is_toxic(&self, threshold: f64) -> bool

TopOfBook::mid_price(&self) -> Decimal
TopOfBook::spread(&self) -> Decimal
```

---

## Market Microstructure Analytics

`MicrostructureMonitor` runs four complementary illiquidity and spread estimators
on a single stream of `MicroTick`s — no separate data feeds required.

### Estimators

| Estimator | Formula | Interpretation |
|---|---|---|
| **Amihud (2002)** | `mean(|ln(P_t/P_{t-1})| / V_t)` | Price sensitivity per unit of volume; higher = more illiquid |
| **Kyle's lambda** | OLS slope of `ΔP ~ Q` (signed volume) | Price impact coefficient; higher = thinner book |
| **Roll (1984) spread** | `2·√(−Cov(r_t, r_{t-1}))` | Bid-ask spread proxy from serial covariance of returns |
| **Bid-Ask Bounce** | `−Cov(r_t, r_{t-1}) / Var(r_t)` clamped to [0,1] | Fraction of return variance explained by microstructure noise |

```text
Amihud illiquidity:
  ILL_t = |r_t| / V_t          where r_t = ln(P_t / P_{t-1})
  ILL   = (1/T) Σ ILL_t        rolling mean over window

Kyle's lambda (OLS):
  ΔP_i = λ · Q_i + ε_i
  λ     = Cov(ΔP, Q) / Var(Q)  computed over a rolling window of ticks

Roll spread:
  γ = Cov(r_t, r_{t-1})
  s = 2·√(−γ)    if γ < 0,  else 0

Bid-Ask Bounce:
  B = −Cov(r_t, r_{t-1}) / Var(r_t),  clamped to [0, 1]
```

```rust
use fin_stream::{MicrostructureMonitor, MicroTick};

fn main() -> Result<(), fin_stream::StreamError> {
    let mut monitor = MicrostructureMonitor::new(100)?; // 100-tick window

    // Construct ticks from your normalised feed:
    let tick = MicroTick::new(
        50_000.0,   // price
        1.5,        // volume
        1.5,        // signed volume (+buy / -sell)
        1_700_000_000_000_000_000_i64, // nanosecond timestamp
    )?;

    let report = monitor.update(&tick)?;

    if report.is_complete() {
        println!("Amihud:      {:.8}", report.amihud.unwrap());
        println!("Kyle lambda: {:.8}", report.kyle_lambda.unwrap());
        println!("Roll spread: {:.6}", report.roll_spread.unwrap());
        println!("Bounce frac: {:.4}", report.bounce.unwrap_or(0.0));
    } else {
        println!("{} / 4 estimators ready", report.available_count());
    }
    Ok(())
}
```

### API reference — `microstructure` module

```rust
MicroTick::new(price: f64, volume: f64, signed_volume: f64, timestamp_ns: i64)
    -> Result<MicroTick, StreamError>   // validates price > 0 and volume > 0

MicrostructureMonitor::new(window_size: usize) -> Result<Self, StreamError>
MicrostructureMonitor::update(&mut self, tick: &MicroTick) -> Result<MicrostructureReport, StreamError>
MicrostructureMonitor::reset(&mut self)
MicrostructureMonitor::amihud(&self)   -> &AmihudIlliquidity
MicrostructureMonitor::kyle(&self)     -> &KyleImpact
MicrostructureMonitor::roll(&self)     -> &RollSpread
MicrostructureMonitor::bounce(&self)   -> &BidAskBounce

AmihudIlliquidity::new(window_size: usize) -> AmihudIlliquidity
AmihudIlliquidity::update(&mut self, tick: &MicroTick) -> Option<f64>

KyleImpact::new(window_size: usize) -> KyleImpact
KyleImpact::update(&mut self, tick: &MicroTick) -> Option<f64>

RollSpread::new(window_size: usize) -> RollSpread
RollSpread::update(&mut self, tick: &MicroTick) -> Option<f64>

BidAskBounce::new(window_size: usize) -> BidAskBounce
BidAskBounce::update(&mut self, tick: &MicroTick) -> Option<f64>

MicrostructureReport::is_complete(&self) -> bool      // all four estimators have values
MicrostructureReport::available_count(&self) -> usize // 0–4
```

---

## Real-Time Regime Detection

`RegimeDetector` classifies an incoming stream of OHLCV bars into one of five
market regimes using a rolling 100-bar window and three fused indicators.

| Regime | Condition |
|---|---|
| `Trending { direction: 1 }` | ADX > 25 and/or Hurst > 0.58, +DI > -DI |
| `Trending { direction: -1 }` | ADX > 25 and/or Hurst > 0.58, -DI > +DI |
| `MeanReverting` | ADX < 20 and/or Hurst < 0.42 |
| `HighVolatility` | Realised vol > `HIGH_VOL_THRESHOLD` |
| `LowVolatility` | Realised vol < `LOW_VOL_THRESHOLD` |
| `Microstructure` | Fewer than 30 warm-up bars seen |

```rust
use fin_stream::regime::RegimeDetector;

let mut detector = RegimeDetector::new(100); // 100-bar rolling window

// … feed OhlcvBars …
println!("Regime:     {}", detector.current_regime());
println!("Confidence: {:.1}%", detector.regime_confidence() * 100.0);

// Static helpers (operate on slices):
let hurst = RegimeDetector::hurst_exponent(&closes);  // R/S analysis
let adx   = RegimeDetector::adx(&bars);               // Wilder ADX
```

### Hurst Exponent (R/S Analysis)

```text
H = log(R/S) / log(n)

R  = max(Y) − min(Y)           (range of cumulative deviations)
S  = std(log-returns)           (standard deviation)
Y_t = Σ(r_i − mean_r)         (cumulative deviation series)

H > 0.5 → trending / persistent (long memory)
H ≈ 0.5 → random walk
H < 0.5 → mean-reverting / anti-persistent
```

## Synthetic Market Data Generator

`SyntheticMarketGenerator` drives four stochastic price models to produce
`NormalizedTick` and `OhlcvBar` sequences for testing and simulation.

| Model | Dynamics | Use case |
|---|---|---|
| `GeometricBrownianMotion` | `dS = μS dt + σS dW` | Baseline equity / crypto prices |
| `JumpDiffusion` | GBM + Poisson(λ) jumps (Merton 1976) | Flash crashes, earnings surprises |
| `OrnsteinUhlenbeck` | `dX = θ(μ−X)dt + σ dW` | Mean-reverting spread / basis |
| `HestonModel` | GBM + CIR variance process, corr ρ | Stochastic-vol smile dynamics |

```rust
use fin_stream::synthetic::{
    GeometricBrownianMotion, HestonModel, JumpDiffusion,
    OrnsteinUhlenbeck, SyntheticMarketGenerator,
};
use fin_stream::ohlcv::Timeframe;

// GBM: 5% drift, 20% vol, starting at 100
let mut gbm = GeometricBrownianMotion::new(0.05, 0.20, 100.0);
let mut gen = SyntheticMarketGenerator::new(42); // seed for reproducibility

let ticks = gen.generate_ticks(1_000, &mut gbm);
let bars  = gen.generate_ohlcv(50, &mut gbm, Timeframe::Minutes(1));

// Heston: stochastic vol with mean-reverting variance and correlation
let mut heston = HestonModel::new(
    0.05,   // drift
    100.0,  // initial price
    2.0,    // kappa (mean-reversion speed of variance)
    0.04,   // theta (long-run variance = 20% vol)
    0.3,    // xi (vol of vol)
    -0.7,   // rho (price-vol correlation, typically negative)
    0.04,   // initial variance
);
let heston_ticks = gen.generate_ticks(500, &mut heston);
```

Both `generate_ticks` and `generate_ohlcv` are deterministic given the same seed.
The generator uses a pure-Rust xorshift64 PRNG — no `unsafe` code, no external
random-number dependencies.

## Multi-Feed Aggregator

`FeedAggregator` subscribes to N independent tick feeds simultaneously, applies
configurable latency compensation per feed, and merges them into a single
chronologically-ordered output stream using one of four merge strategies.

### Merge strategies

| Strategy | Description |
|---|---|
| `BestBid` | Emit the tick with the highest price across all feeds (best resting bid) |
| `BestAsk` | Emit the tick with the lowest price across all feeds (best resting ask) |
| `VwapWeighted` | Emit a synthetic tick whose price is the VWAP of all buffered ticks; quantity = total volume |
| `PrimaryWithFallback` | Use a designated primary feed; fall back to `BestBid` when the primary is stale |

### Latency compensation

Every feed has an optional `latency_offset_ms` that is subtracted from
`received_at_ms` before merge-ordering. Faster feeds naturally arrive first;
slower venues are time-shifted backward so the merged stream reflects the
order events actually occurred at the exchange.

```rust
use fin_stream::agg::{FeedAggregator, FeedHandle, AggregatorConfig, MergeStrategy};

let mut agg = FeedAggregator::new(AggregatorConfig {
    strategy: MergeStrategy::VwapWeighted,
    feed_buffer_capacity: 2_048,
    merge_window: 32,
});

// Binance arrives ~5 ms earlier than Coinbase on this network.
let tx_binance = agg.add_feed(
    FeedHandle::new("binance-btc-usdt").with_latency_offset(5)
).expect("add feed");

let tx_coinbase = agg.add_feed(
    FeedHandle::new("coinbase-btc-usd").with_latency_offset(18)
).expect("add feed");

// Push ticks from each exchange into their senders; the aggregator
// merges them in compensated-timestamp order.
// let merged_tick = agg.next_tick(); // → Option<NormalizedTick>
```

### Arbitrage detection

`ArbDetector` maintains the latest tick per feed and checks every feed pair
for price discrepancies. When the gross spread exceeds the threshold (in basis
points) an `ArbOpportunity` is emitted.

```rust
use fin_stream::agg::{ArbDetector, ArbOpportunity};

let mut detector = ArbDetector::new(10.0); // flag spreads > 10 bps

// Ingest ticks as they arrive from the aggregator.
detector.ingest("binance", &binance_tick);
detector.ingest("coinbase", &coinbase_tick);

let opportunities: Vec<ArbOpportunity> = detector.check();
for opp in &opportunities {
    println!(
        "Buy on {} @ {}, sell on {} @ {} — {:.1} bps gross spread",
        opp.buy_feed, opp.buy_price,
        opp.sell_feed, opp.sell_price,
        opp.spread_bps,
    );
}
```

`ArbOpportunity` carries: `symbol`, `buy_feed`, `sell_feed`, `buy_price`,
`sell_price`, `spread_bps`, and `detected_at_ms`.

---

## Replay Engine

`TickReplayer` reads NDJSON tick files and replays them at configurable speed
through the `TickSource` trait — the same interface used by live WebSocket feeds.
Strategy and analytics code has no awareness of whether it is running against live
or historical data.

### Speed control

| `speed_multiplier` | Effect |
|---|---|
| `1.0` | Real-time: honours original inter-tick gaps |
| `10.0` | 10× faster than real-time |
| `100.0` | 100× faster than real-time |
| `0.0` | Emit all ticks as fast as possible (no delay) |

### File format

Input files are newline-delimited JSON (NDJSON). Each line deserialises into a
`NormalizedTick`. Lines beginning with `#` are treated as comments and skipped.

```json
{"exchange":"Binance","symbol":"BTC-USDT","price":"50000","quantity":"0.01","side":null,"trade_id":null,"exchange_ts_ms":1700000000000,"received_at_ms":1700000000001}
{"exchange":"Coinbase","symbol":"BTC-USD","price":"50005","quantity":"0.005","side":"buy","trade_id":"T2","exchange_ts_ms":1700000000100,"received_at_ms":1700000000102}
```

### Example

```rust
use fin_stream::replay::{TickReplayer, ReplaySession, TickSource};

# async fn run() -> Result<(), fin_stream::StreamError> {
let session = ReplaySession::new("data/btc_ticks.ndjson")
    .with_speed(10.0)          // replay at 10× real time
    .with_start_offset(5_000)  // skip first 5 seconds of the recording
    .with_max_ticks(100_000);  // stop after 100 K ticks

let mut replayer = TickReplayer::with_session(session);

let (tx, mut rx) = tokio::sync::mpsc::channel(1_024);

tokio::spawn(async move {
    replayer.run(tx).await.ok();
});

while let Some(tick) = rx.recv().await {
    // identical code path as live data
    println!("{} @ {}", tick.symbol, tick.price);
}
# Ok(())
# }
```

### Looping replay

Set `.looping()` on the session to restart from the beginning automatically —
useful for continuous strategy back-tests without manually reloading the file.

### ReplayStats

`TickReplayer::stats()` returns a `ReplayStats` snapshot:

| Field | Description |
|---|---|
| `ticks_replayed` | Total ticks emitted |
| `duration_ms` | Wall-clock elapsed time |
| `lag_ms` | Mean delay between scheduled and actual emit time |
| `parse_errors` | Lines that could not be deserialised (skipped) |
| `skipped_ticks` | Ticks skipped by `start_offset_ms` |

`ReplayStats::ticks_per_second()` computes throughput from the above fields.

---

## Multi-Exchange NBBO Aggregation

`MultiExchangeAggregator` merges N per-exchange `NormalizedTick` streams into a
single consolidated best bid/ask (NBBO-style view), tracks per-exchange latency
divergence, and emits `ArbitrageOpportunity` alerts when the price spread between
any two exchanges exceeds a configurable threshold.

```rust,no_run
use fin_stream::multi_exchange::{AggregatorConfig, MultiExchangeAggregator};
use rust_decimal_macros::dec;

async fn example() -> Result<(), fin_stream::StreamError> {
    // Create aggregator for BTC-USD, flag arb when spread > $10
    let cfg = AggregatorConfig::new("BTC-USD", dec!(10))?;
    let (agg, mut nbbo_rx, mut arb_rx) = MultiExchangeAggregator::new(cfg, 64);

    // In your tick pipeline:
    // agg.ingest(tick_from_binance).await?;
    // agg.ingest(tick_from_coinbase).await?;

    // Receive consolidated NBBO:
    if let Some(nbbo) = nbbo_rx.recv().await {
        println!("Best bid: {:?}", nbbo.best_bid);
        println!("Best ask: {:?}", nbbo.best_ask);
        println!("Spread:   {:?}", nbbo.spread());
        println!("Mid:      {:?}", nbbo.mid_price());
    }

    // Receive arbitrage alerts:
    if let Some(opp) = arb_rx.recv().await {
        println!("Arb: buy @ {} on {}, sell @ {} on {}  (+{:.4}%)",
            opp.buy_price, opp.buy_exchange,
            opp.sell_price, opp.sell_exchange,
            opp.profit_pct());
    }

    // Latency divergence between fastest and slowest exchange (ms):
    let now_ms = 0u64; // replace with real clock
    println!("Latency divergence: {:?} ms", agg.max_latency_divergence_ms(now_ms));
    Ok(())
}
```

| Method | Description |
|---|---|
| `ingest(tick)` | Process one tick; updates NBBO and checks arbitrage |
| `current_nbbo()` | Snapshot the current NBBO from all exchange states |
| `latency_stats(now_ms)` | Per-exchange latency breakdown |
| `max_latency_divergence_ms(now_ms)` | Gap between fastest and slowest exchange (ms) |
| `exchange_count()` | Number of exchanges with at least one tick |

---

## WebSocket Circuit Breaker

`WsCircuitBreaker` wraps the raw message channel from a `WsManager` and counts
consecutive parse failures. After `failure_threshold` consecutive failures (default: 5)
the circuit **opens**: the breaker enters degraded mode and emits synthetic ticks
derived from the last-known price with an inflated spread. Downstream consumers
always have a price reference, even when the connection is broken. The circuit
**closes** automatically when a real tick arrives.

```rust,no_run
use fin_stream::circuit_breaker::{CircuitBreakerConfig, CircuitState, WsCircuitBreaker};
use fin_stream::tick::Exchange;

async fn example() -> Result<(), fin_stream::StreamError> {
    let cfg = CircuitBreakerConfig::new(Exchange::Binance, "BTC-USD", 5)?;
    let (breaker, mut tick_rx) = WsCircuitBreaker::new(cfg, 64);

    // Spawn alongside your WsManager raw message channel:
    // tokio::spawn(async move { breaker.run(raw_msg_rx).await });

    // tick_rx yields both real and synthetic NormalizedTick values.
    // Synthetic ticks have trade_id = Some("synthetic") and quantity = 0.
    while let Some(tick) = tick_rx.recv().await {
        let is_synthetic = tick.trade_id.as_deref() == Some("synthetic");
        println!("Price: {} (synthetic: {})", tick.price, is_synthetic);
    }

    println!("State:               {:?}", breaker.state());
    println!("Consecutive failures: {}", breaker.consecutive_failures());
    Ok(())
}
```

| Config field | Default | Description |
|---|---|---|
| `failure_threshold` | 5 | Failures before circuit opens |
| `synthetic_tick_interval` | 500 ms | Interval between synthetic ticks in degraded mode |
| `degraded_spread_pct` | 0.5% | Spread applied to last-known price for synthetic ticks |
| `initial_backoff` | 500 ms | Starting reconnect backoff |
| `max_backoff` | 60 s | Reconnect backoff cap |

---

## Tick Anomaly Detection

`TickAnomalyDetector` flags four anomaly types in a streaming tick pipeline.
Normal ticks are always forwarded unchanged; anomaly events are emitted on a
**separate channel** so they can be handled independently without blocking the
tick pipeline.

```rust,no_run
use fin_stream::anomaly::{AnomalyDetectorConfig, AnomalyKind, TickAnomalyDetector};

async fn example() {
    let cfg = AnomalyDetectorConfig::default_config()
        .with_window_size(100)              // rolling window for mean/std
        .with_price_spike_z(4.0)            // flag if |price - mean| > 4σ
        .with_volume_spike_multiplier(10.0); // flag if qty > 10× mean qty

    let (mut detector, mut anomaly_rx) = TickAnomalyDetector::new(cfg, 256);

    // In your tick pipeline (tick passes through unchanged):
    // let tick = detector.process(tick).await;

    // Handle anomaly events in a separate task:
    while let Some(event) = anomaly_rx.recv().await {
        match &event.kind {
            AnomalyKind::PriceSpike { z_score } =>
                println!("Price spike! z={z_score} on {}", event.tick.symbol),
            AnomalyKind::VolumeSpike { ratio } =>
                println!("Volume spike! ratio={ratio}x"),
            AnomalyKind::SequenceGap { last_seq, current_seq } =>
                println!("Gap: missed {} ticks", current_seq - last_seq - 1),
            AnomalyKind::TimestampInversion { previous_ms, current_ms } =>
                println!("Out-of-order: {} -> {} ms", previous_ms, current_ms),
        }
    }
}
```

| Anomaly | Trigger |
|---|---|
| `PriceSpike` | `|price - rolling_mean| / rolling_std > price_spike_z` |
| `VolumeSpike` | `quantity > rolling_mean_qty x volume_spike_multiplier` |
| `SequenceGap` | `trade_id` (integer) skips one or more sequence numbers |
| `TimestampInversion` | `tick.received_at_ms < previous tick's received_at_ms` |

---

## Snapshot-and-Replay

`TickRecorder` writes ticks to a compact binary file (length-prefixed JSON
records). `TickReplayer` reads the file back and re-emits ticks at original
timing or at N-speed — enabling backtesting with real captured market data.

### Recording

```rust,no_run
use fin_stream::snapshot::TickRecorder;

fn example() -> Result<(), fin_stream::StreamError> {
    let mut recorder = TickRecorder::open("/data/btc_20260322.bin")?;

    // In your tick pipeline:
    // recorder.record(&tick)?;

    recorder.flush()?; // always flush before dropping
    println!("Wrote {} ticks ({} bytes)",
        recorder.ticks_written(), recorder.bytes_written());
    Ok(())
}
```

### Replay

```rust,no_run
use fin_stream::snapshot::TickReplayer;

async fn example() -> Result<(), fin_stream::StreamError> {
    // Load and replay at 10x speed:
    let replayer = TickReplayer::open("/data/btc_20260322.bin", 10.0)?;
    println!("Loaded {} ticks", replayer.tick_count());

    let (_handle, mut tick_rx) = replayer.start(256);
    while let Some(tick) = tick_rx.recv().await {
        // Process exactly as in live mode
        let _ = tick;
    }
    Ok(())
}
```

| Speed multiplier | Effect |
|---|---|
| `0.0` | No delay — as fast as possible |
| `1.0` | Real-time replay |
| `10.0` | 10x faster than real-time |
| `0.5` | Half speed (slower than real-time) |

Wire format: `[4 bytes LE u32 payload_length][JSON bytes]`. Human-inspectable and
append-safe via `TickRecorder::open_append`.

---

## gRPC Streaming Endpoint

Enable the `grpc` Cargo feature to expose the tick stream over gRPC using
[tonic](https://github.com/hyperium/tonic). The proto is defined in
`proto/tick_stream.proto` and compiled automatically at build time.

```toml
# Cargo.toml
fin-stream = { version = "*", features = ["grpc"] }
```

### Server

```rust,no_run
# #[cfg(feature = "grpc")]
async fn example() -> Result<(), Box<dyn std::error::Error>> {
    use fin_stream::grpc::TickStreamServer;
    use tonic::transport::Server;

    // Create the server with a 1024-tick broadcast buffer per subscriber.
    let server = TickStreamServer::new(1024);
    let svc = server.clone_service();

    // Serve in a background task:
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve("0.0.0.0:50051".parse().unwrap())
            .await
            .unwrap();
    });

    // Publish ticks from your pipeline:
    // server.publish(normalized_tick);
    println!("Active subscribers: {}", server.subscriber_count());
    Ok(())
}
```

### Proto definition (proto/tick_stream.proto)

```protobuf
service TickStreamService {
  rpc SubscribeTicks(SubscribeTicksRequest) returns (stream Tick);
}

message TickFilter {
  string symbol   = 1;  // "" = all symbols
  string exchange = 2;  // "" = all exchanges
}
```

Clients filter by symbol and/or exchange name. Slow subscribers that fall behind
the broadcast buffer receive a lagged notification and skip buffered ticks (no
blocking of the fast path).

---

## Zero-Allocation Hot Path

The hot path through `TickNormalizer → SpscRing → FeedAggregator` makes zero
heap allocations per tick:

- **`NormalizedTick`** is a plain Rust struct — stack-allocated, `Copy`-able,
  and `Send`. No `Box`, no `Arc`, no `String` clone on the hot path.
- **`SpscRing<T, N>`** uses a const-generic array (`[MaybeUninit<T>; N]`).
  `push` and `pop` are single atomic operations (`Release`/`Acquire` ordering).
  No `malloc` call is made after initial construction.
- **`FeedAggregator::poll_feeds`** drains the bounded `mpsc` channels (which
  are pre-allocated at `add_feed` time) into a `BinaryHeap` that is also
  pre-allocated. Tick merge never allocates on the happy path.
- **`ArbDetector::check`** iterates over a `HashMap` of `NormalizedTick`
  references and performs arithmetic comparisons. The only allocation is the
  `Vec<ArbOpportunity>` returned on a hit — which is empty in the common case.

Benchmarks on a 3.6 GHz Zen 3 core show the SPSC ring sustaining **150 million
push/pop pairs per second** for `u64` items, exceeding the 100 K ticks/second
design target by three orders of magnitude.

---

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
  Live Feeds                        Historical Data
  ──────────────────────            ──────────────────────
  WsManager (Binance)  ──┐          TickReplayer (NDJSON)
  WsManager (Coinbase) ──┤              │  speed_multiplier
  WsManager (Alpaca)   ──┤              │  loop_replay
                         │              │  start_offset_ms
                         ▼              ▼
                  [ FeedAggregator ]           ─── TickSource trait (live ≡ replay)
                    latency compensation
                    BestBid / BestAsk
                    VwapWeighted
                    PrimaryWithFallback
                         │
                         +──► [ ArbDetector ]  ── spread > N bps → ArbOpportunity
                         │
                         ▼
               [ TickNormalizer ]     raw JSON payload → NormalizedTick (all exchanges)
                         │
                         ▼
             [ SPSC Ring Buffer ]     lock-free O(1) push/pop, zero-allocation hot path
                         │
                         ▼
             [ OHLCV Aggregator ]     streaming bar construction at any timeframe
                         │
                         ▼
      [ MinMax / ZScore Normalizer ]  rolling-window coordinate normalization
                         │
                         +──► [ Lorentz Transform ]  relativistic spacetime boost
                         │
                         ▼
    Downstream (ML model | trade signal engine | order management)

  Parallel paths:
  [ OrderBook ]       -- delta streaming, snapshot reset, crossed-book guard
  [ HealthMonitor ]   -- per-feed staleness detection, circuit-breaker
  [ SessionAwareness ]-- Open / Extended / Closed classification
  [ MevDetector ]     -- sandwich, frontrun, backrun heuristics
  [ AnomalyDetector ] -- price spikes, volume spikes, sequence gaps
```

## Analytics Suite

Over 88 rounds of development, fin-stream has accumulated a comprehensive
analytics suite covering every layer of the pipeline.

### `NormalizedTick` Batch Analytics (200+ functions)

Static methods operating on `&[NormalizedTick]` slices for microstructure analysis:

| Category | Example functions |
|---|---|
| VWAP / price | `vwap`, `vwap_deviation_std`, `volume_weighted_mid_price`, `mid_price_drift` |
| Volume / notional | `total_volume`, `buy_volume`, `sell_volume`, `buy_notional`, `sell_notional_fraction`, `max_notional`, `min_notional`, `trade_notional_std` |
| Side / flow | `buy_count`, `sell_count`, `buy_sell_count_ratio`, `buy_sell_size_ratio`, `order_flow_imbalance`, `buy_sell_avg_qty_ratio` |
| Price movement | `price_range`, `price_mean`, `price_mad`, `price_dispersion`, `max_price_gap`, `price_range_velocity`, `max_price_drop`, `max_price_rise` |
| Tick direction | `uptick_count`, `downtick_count`, `uptick_fraction`, `tick_direction_bias`, `price_mean_crossover_count` |
| Timing / arrival | `tick_count_per_ms`, `volume_per_ms`, `inter_arrival_variance`, `inter_arrival_cv`, `notional_per_second` |
| Concentration | `quantity_concentration`, `price_level_volume`, `quantity_std`, `notional_skewness` |
| Running extremes | `running_high_count`, `running_low_count`, `max_consecutive_side_run` |
| Spread / efficiency | `spread_efficiency`, `realized_spread`, `adverse_selection_score`, `price_impact_per_unit` |

### `OhlcvBar` Batch Analytics (200+ functions)

Static methods operating on `&[OhlcvBar]` slices:

| Category | Example functions |
|---|---|
| Candle structure | `body_fraction`, `bullish_ratio`, `avg_bar_efficiency`, `avg_wick_symmetry`, `body_to_range_std` |
| Highs / lows | `peak_close`, `trough_close`, `max_high`, `min_low`, `higher_highs_count`, `lower_lows_count`, `new_high_count`, `new_low_count` |
| Volume | `mean_volume`, `up_volume_fraction`, `down_close_volume`, `up_close_volume`, `max_bar_volume`, `min_bar_volume`, `high_volume_fraction` |
| Close statistics | `mean_close`, `close_std`, `close_skewness`, `close_at_high_fraction`, `close_at_low_fraction`, `close_cluster_count` |
| Range / movement | `total_range`, `range_std_dev`, `avg_range_pct_of_open`, `volume_per_range`, `total_body_movement`, `avg_open_to_close` |
| Patterns | `continuation_bar_count`, `zero_volume_fraction`, `complete_fraction` |
| Shadow analysis | `avg_lower_shadow_ratio`, `tail_upper_fraction`, `tail_lower_fraction`, `avg_lower_wick_to_range` |
| VWAP / price | `mean_vwap`, `normalized_close`, `price_channel_position`, `candle_score` |

### Normalizer Analytics (80+ functions each)

Both `MinMaxNormalizer` and `ZScoreNormalizer` expose identical analytics suites:

| Category | Example functions |
|---|---|
| Central tendency | `mean`, `median`, `geometric_mean`, `harmonic_mean`, `exponential_weighted_mean` |
| Dispersion | `variance_f64`, `std_dev`, `interquartile_range`, `range_over_mean`, `coeff_of_variation`, `rms` |
| Shape | `skewness`, `kurtosis`, `second_moment`, `tail_variance` |
| Rank / quantile | `percentile_rank`, `quantile_range`, `value_rank`, `distance_from_median` |
| Threshold | `count_above`, `above_median_fraction`, `below_mean_fraction`, `outlier_fraction`, `zero_fraction` |
| Trend | `momentum`, `rolling_mean_change`, `is_mean_stable`, `sign_flip_count`, `new_max_count`, `new_min_count` |
| Extremes | `max_fraction`, `min_fraction`, `peak_to_trough_ratio`, `range_normalized_value` |
| Misc | `ema_of_z_scores`, `rms`, `distinct_count`, `interquartile_mean`, `latest_minus_mean`, `latest_to_mean_ratio` |

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
| OFI raw update | O(1) per top-of-book snapshot |
| OFI accumulator | O(1) amortized; rolling VecDeque eviction |
| Microstructure update | O(1) amortized per `MicroTick`; O(W) on window eviction |
| VPIN bucket | O(1) per tick; O(n_buckets) on bucket close |

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
OhlcvBar::close_to_low_ratio(&self) -> Option<f64>
OhlcvBar::volume_per_trade(&self) -> Option<Decimal>
OhlcvBar::price_range_overlap(&self, other: &OhlcvBar) -> bool
OhlcvBar::bar_height_pct(&self) -> Option<f64>
OhlcvBar::is_bullish_engulfing(&self, prev: &OhlcvBar) -> bool
OhlcvBar::close_gap(&self, prev: &OhlcvBar) -> Decimal
OhlcvBar::close_above_midpoint(&self) -> bool
OhlcvBar::close_momentum(&self, prev: &OhlcvBar) -> Decimal
OhlcvBar::bar_range(&self) -> Decimal

// Batch analytics added in rounds 42–88 (static, operate on &[OhlcvBar])
// See Analytics Suite section for the full categorized list (200+ functions).
// Representative selection:
OhlcvBar::bullish_ratio(bars: &[OhlcvBar]) -> Option<f64>
OhlcvBar::peak_close(bars: &[OhlcvBar]) -> Option<Decimal>
OhlcvBar::trough_close(bars: &[OhlcvBar]) -> Option<Decimal>
OhlcvBar::body_fraction(bars: &[OhlcvBar]) -> Option<f64>
OhlcvBar::up_volume_fraction(bars: &[OhlcvBar]) -> Option<f64>
OhlcvBar::range_std_dev(bars: &[OhlcvBar]) -> Option<f64>
OhlcvBar::higher_highs_count(bars: &[OhlcvBar]) -> usize
OhlcvBar::lower_lows_count(bars: &[OhlcvBar]) -> usize
OhlcvBar::mean_close(bars: &[OhlcvBar]) -> Option<Decimal>
OhlcvBar::close_std(bars: &[OhlcvBar]) -> Option<f64>
OhlcvBar::mean_vwap(bars: &[OhlcvBar]) -> Option<Decimal>
OhlcvBar::total_body_movement(bars: &[OhlcvBar]) -> Decimal
OhlcvBar::avg_wick_symmetry(bars: &[OhlcvBar]) -> Option<f64>
OhlcvBar::complete_fraction(bars: &[OhlcvBar]) -> Option<f64>
```

### `norm` module

Both normalizers expose 80+ analytics. Only core methods are shown here; see
the **Analytics Suite** section above for the full categorized function list.

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
MinMaxNormalizer::normalized_range(&mut self) -> Option<f64>
MinMaxNormalizer::fraction_above_mid(&mut self) -> Option<f64>
// ... 70+ additional analytics (moments, percentiles, trend, shape — see Analytics Suite)

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
ZScoreNormalizer::rolling_mean_change(&self) -> Option<f64>
ZScoreNormalizer::count_positive_z_scores(&self) -> usize
ZScoreNormalizer::above_threshold_count(&self, z_threshold: f64) -> usize
ZScoreNormalizer::window_span_f64(&self) -> Option<f64>
ZScoreNormalizer::is_mean_stable(&self, threshold: f64) -> bool
// ... 60+ additional analytics (see Analytics Suite)
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
| `ConfigError { reason }` | ofi / microstructure | Invalid constructor argument (e.g. zero window size) |
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
