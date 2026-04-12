# fin-stream Requirements Analysis

**Version:** 2.11.0 | **Status:** Production | **MSRV:** 1.75

---

## 1. Functional Requirements

### 1.1 Core Data Ingestion & Normalization

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **Multi-exchange support** | Normalize ticks from Binance, Coinbase, Alpaca, Polygon into canonical form | Critical | ✓ |
| **Canonical tick format** | Single `NormalizedTick` struct; exact decimal arithmetic for prices/quantities | Critical | ✓ |
| **Zero-allocation hot path** | Tick normalization must add <1μs overhead; no heap allocations per tick | Critical | ✓ |
| **Thread-safe normalization** | `TickNormalizer` is `Send + Sync`; can be shared across async tasks | High | ✓ |
| **Exchange metadata** | Track source exchange, symbol, trade direction (Buy/Sell), trade ID | High | ✓ |

### 1.2 Real-Time Order Book Management

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **L2 order book reconstruction** | Build and maintain top-of-book snapshots from incremental deltas | Critical | ✓ |
| **BTreeMap-backed storage** | Fast range queries (depth slices); O(log n) insert/delete per level | High | ✓ |
| **Crossed-book detection** | Reject updates when bid >= ask (data quality guard) | High | ✓ |
| **Sequence gap detection** | Track sequence numbers; alert on missing updates | Medium | ✓ |
| **Multi-symbol concurrency** | Per-symbol `OrderBook` in `DashMap`; concurrent updates without global lock | High | ✓ |

### 1.3 Bar Aggregation (OHLCV)

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **Multiple bar types** | Time-based, tick-count, volume, dollar-volume aggregation | High | ✓ |
| **VWAP computation** | Online VWAP updates with cumulative volume tracking | High | ✓ |
| **Multi-symbol streams** | Route ticks to per-symbol builders; emit bars independently | High | ✓ |
| **Gap-fill bars** | Optional synthetic bars between real bars for visualization | Medium | ✓ |
| **200+ analytics on bars** | Static slice-based analytics (mean, std, skew, percentiles, etc.) | High | ✓ |

### 1.4 Risk & Market Microstructure Analytics

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **Rolling risk metrics** | Volatility (annualized), VaR (95%), max drawdown, Sharpe ratio | High | ✓ |
| **Real-time regime detection** | Classify: Trending / MeanReverting / HighVol / LowVol via Hurst + ADX + vol | Medium | ✓ |
| **Toxicity indicators** | PIN, VPIN, Kyle λ, Amihud illiquidity via tick window | High | ✓ |
| **Order flow imbalance** | Per-tick OFI from top-of-book deltas; accumulate + normalize | High | ✓ |
| **Microstructure monitors** | Roll spread, bid-ask bounce, Kyle impact estimators | Medium | ✓ |
| **Anomaly detection** | Flag price spikes (z-score), volume spikes, sequence gaps, timestamp inversions | High | ✓ |

### 1.5 Trading & Strategy Execution

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **Statistical arbitrage** | Detect cointegrated pairs; emit spread-based signals (Long/Short/Exit) | High | ✓ |
| **Portfolio risk management** | Kelly-like position sizing, stop-loss/take-profit monitoring | High | ✓ |
| **Market-maker simulation** | Order book simulation; mark-to-market P&L | Medium | ✓ |
| **Backtesting engine** | Replay historical tick data at configurable speed (1x, 10x, 100x, etc.) | High | ✓ |
| **Multi-venue order routing** | Rank venues by execution quality; route orders for best fill | Medium | ✓ |

### 1.6 Feed Reliability & Health Monitoring

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **Stale feed detection** | Monitor tick arrival intervals; flag feeds silent for >N ms | High | ✓ |
| **Circuit breaker pattern** | WebSocket circuit breaker: fail-open after 5 consecutive failures | High | ✓ |
| **Feed quality scoring** | Composite score [0–100]: latency, gap rate, duplicate rate | Medium | ✓ |
| **Per-symbol circuit breakers** | Halt trading on price spikes or volume surges; auto-recover | Medium | ✓ |
| **Exponential-backoff reconnect** | Auto-reconnect with configurable backoff (500ms → 60s) | High | ✓ |

### 1.7 Data Persistence & Replay

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **Binary tick recording** | Compact wire format: `[u32 len][JSON]` for append-safe storage | High | ✓ |
| **NDJSON tick replay** | Load historical ticks from newline-delimited JSON; replay at N-speed | High | ✓ |
| **Looping replay** | Optional auto-restart for continuous backtesting | Medium | ✓ |
| **Snapshot format** | Binary encoding preserves all `NormalizedTick` fields exactly | High | ✓ |

### 1.8 gRPC Streaming & IPC

| Requirement | Description | Priority | Status |
|---|---|---|---|
| **gRPC streaming endpoint** | Expose tick stream over gRPC via tonic (optional feature) | Medium | ✓ |
| **Per-symbol/exchange filtering** | Clients filter ticks by symbol and/or exchange | Medium | ✓ |
| **Backpressure handling** | Slow subscribers skip buffered ticks (no blocking of hot path) | Medium | ✓ |

---

## 2. Non-Functional Requirements

### 2.1 Performance Targets

| Metric | Target | Evidence | Status |
|---|---|---|---|
| **Tick throughput** | ≥100 K ticks/sec | SPSC ring buffer bench: 150M pairs/sec on 3.6 GHz Zen 3 | ✓ |
| **Normalization latency** | <1 μs per tick (hot path) | Stack-allocated `NormalizedTick`; zero allocations | ✓ |
| **L2 book operations** | O(log n) per delta | BTreeMap-backed; depth slices O(n) for top-k | ✓ |
| **Rolling analytics** | O(1) per tick (amortized) | Welford's algorithm; fixed window overhead | ✓ |
| **Ring buffer overhead** | <5% | Atomic ops only; `MaybeUninit` eliminates discriminant | ✓ |

### 2.2 Scalability

| Dimension | Limit | Approach | Status |
|---|---|---|---|
| **Concurrent symbols** | Arbitrary | Per-symbol `OrderBook` in `DashMap`; no global lock | ✓ |
| **Feeds per portfolio** | 10K+ | `PortfolioFeed` with `JoinSet` per asset; exponential backoff | ✓ |
| **Bars per symbol** | Arbitrary | Per-symbol `BarBuilder` in router; independent close logic | ✓ |
| **Tick window for analytics** | 1M+ | Welford accumulator; O(1) memory per metric | ✓ |

### 2.3 Reliability

| Aspect | Guarantee | Mechanism | Status |
|---|---|---|---|
| **No panics on valid input** | Never panic on production tick data | All fallible ops return `Result`; typed error hierarchy | ✓ |
| **Reconnect liveness** | Auto-recover from transient connection loss | Exponential backoff + circuit breaker + synthetic ticks | ✓ |
| **Order book consistency** | Detect and reject crossed books | Bid-ask invariant checked on every update | ✓ |
| **Data integrity** | Exact decimal arithmetic for prices | `rust_decimal::Decimal` throughout; never `f64` | ✓ |
| **Memory safety** | No unsafe code in public API | `#![forbid(unsafe_code)]` in lib.rs | ✓ |

### 2.4 Concurrency Model

| Pattern | Usage | Guarantees |
|---|---|---|
| **SPSC ring buffer** | Tick inter-thread communication | Lock-free; `Send` producer/consumer; no blocking |
| **DashMap** | Multi-symbol order books, feed states | Lock-free per-entry; concurrent reads/writes |
| **Tokio async runtime** | WebSocket I/O, backtest loop | Non-blocking event-driven; scales to 10K+ concurrent streams |
| **JoinSet** | Portfolio feed task management | Structured concurrency; graceful shutdown of all tasks |

---

## 3. External Constraints

### 3.1 Exchange & Protocol Support

| Exchange | Protocols | Notes | Status |
|---|---|---|---|
| **Binance** | WebSocket (spot + futures) | Binary+JSON mixed; `@aggTrade` stream | ✓ |
| **Coinbase** | WebSocket | JSON frames; `match`, `done`, `open` message types | ✓ |
| **Alpaca** | WebSocket | Alpaca data API v2; SIP/CTA feeds | ✓ |
| **Polygon** | WebSocket | Stocks/options; `A` (aggregate), `Q` (quote), `T` (trade) | ✓ |
| **FIX 4.2** | TCP (async) | Logon → MarketDataRequest → Snapshot/Refresh | ✓ |

### 3.2 Data Type Constraints

| Domain | Constraint | Rationale | Status |
|---|---|---|---|
| **Prices** | `rust_decimal::Decimal` only | Exact arithmetic; no floating-point rounding errors | ✓ |
| **Timestamps** | `u64` milliseconds | Monotonic; consistent across exchanges | ✓ |
| **Normalizers (f64)** | Dimensionless metrics only | Beta, gamma, correlation, z-scores | ✓ |
| **Symbols** | `String` | Variable length; exchange-specific formats | ✓ |

### 3.3 Deployment Environment

| Constraint | Requirement | Status |
|---|---|---|
| **Network** | Low-latency, high-throughput internet (100 Mbps+) | Assumed |
| **CPU** | Modern x86-64 or ARM; ≥2 cores | Optimized for single hot core; scales linearly |
| **Memory** | 512 MB - 2 GB (typical trading node) | Per-symbol order books; rolling window analytics |
| **OS** | Linux, macOS, Windows (via WSL2) | Tokio cross-platform; no platform-specific I/O |
| **Rust version** | 1.75+ (MSRV enforced) | Const generics; async/await; `#![forbid(...)]` |

---

## 4. Business Requirements & Use Cases

### 4.1 Primary Use Cases

#### UC1: Real-Time Market Data Pipeline
**Actors:** Quantitative trader, risk manager  
**Goal:** Ingest tick data from multiple exchanges, aggregate into a unified canonical stream  
**Scope:** Live WebSocket feeds → normalization → ring buffer → analytics  
**Success Criteria:**
- Zero dropped ticks under load
- <10 ms end-to-end latency (feed → buffer → analytics)
- Sub-microsecond overhead per tick

#### UC2: Statistical Arbitrage Signal Generation
**Actors:** Quant strategist  
**Goal:** Detect cointegrated price pairs and emit trading signals  
**Scope:** Pair cointegration testing → rolling spread monitoring → z-score thresholding  
**Success Criteria:**
- Half-life estimation accurate to ±5% on known pairs
- Signals generated in real-time with <50ms latency

#### UC3: Backtesting with Historical Ticks
**Actors:** Backtesting engine operator  
**Goal:** Replay recorded tick data at controllable speed; execute strategy on same code path as live  
**Scope:** Snapshot files → TickReplayer → strategy pipeline  
**Success Criteria:**
- Deterministic output given same seed
- Support 10x–1000x replay speed without gaps
- Memory usage scales with window size, not total tick count

#### UC4: Risk Monitoring & Portfolio Health
**Actors:** Risk officer  
**Goal:** Monitor real-time portfolio Greeks, liquidity metrics, anomalies  
**Scope:** Tick aggregation → rolling risk metrics → health dashboard  
**Success Criteria:**
- Volatility accurate to ±1% vs. closed-form formula
- Anomaly alerts within 1 tick of occurrence
- Circuit breakers halt positions within 100ms of trigger

#### UC5: Market Microstructure Analysis
**Actors:** Microstructure researcher  
**Goal:** Quantify bid-ask spreads, order flow toxicity, regime persistence  
**Scope:** Tick data → microstructure estimators → signal generation  
**Success Criteria:**
- Roll spread estimates match trade-weighted half-spreads
- PIN estimates calibrate to known informed-trading scenarios

---

## 5. Roadmap & Future Enhancements

### 5.1 Planned Features

| Feature | Rationale | Effort | Priority |
|---|---|---|---|
| **Options Greeks analytics** | Risk mgmt for derivatives portfolios | High | Medium |
| **Machine learning feature export** | Pipeline → Arrow IPC → Python ML | Medium | Medium |
| **Persistent event sourcing** | Audit trail + replay-from-any-point | High | Low |
| **Multi-tenant isolation** | Per-portfolio sandboxing; resource quotas | High | Low |
| **Websocket multiplexing** | One connection per exchange; multi-symbol fan-out | Medium | Medium |

### 5.2 Performance Roadmap

| Target | Current | Gap | Timeline |
|---|---|---|---|
| **1M ticks/sec** | 100K+ (demo) | 10x | 6–12 months |
| **Sub-100ns latency per tick** | <1 μs | 10x | 12+ months |
| **GPU acceleration for analytics** | CPU only | Research phase | 12+ months |

---

## 6. Success Metrics

### 6.1 Operational Metrics

| Metric | Target | Measurement |
|---|---|---|
| **Uptime** | 99.95% | Exclude planned maintenance |
| **Feed availability** | 99.9% per exchange | Daily circuit-breaker stats |
| **Tick loss rate** | 0% | Sequence gap monitoring |
| **Mean latency (p50)** | <500 μs | Per-symbol latency tracking |
| **Tail latency (p99)** | <5 ms | Rolling latency histogram |

### 6.2 Quality Metrics

| Metric | Target | Measurement |
|---|---|---|
| **Code coverage** | >85% | CI/CD gate |
| **MSRV stability** | 1.75 | Enforced in CI |
| **Dependency audit** | Zero critical | `cargo audit` before release |
| **Documentation coverage** | 100% public API | Enforced in CI |

---

## 7. Refactoring & Evolution Requirements

### 7.1 Architectural Improvements (Phase 1-4)

| Requirement | Current State | Target State | Priority |
|---|---|---|---|
| **Trait-based design** | 50+ concrete types | Analyzer trait hierarchy | Critical |
| **Testable dependencies** | Direct DashMap (50 sites) | Storage abstraction trait | Critical |
| **Modular analytics** | 37 modules in 1 crate | 4 focused crates | High |
| **Code duplication** | 50+ repetitions | <5 via centralized validators | High |
| **SOLID adherence** | ~60% | 90%+ | High |
| **Public API surface** | 141 types exposed | Facade with deprecation warnings | High |
| **Configuration complexity** | 23+ Config structs | Unified, injectable Config trait | Medium |
| **Documentation completeness** | Partial | 100% with examples | Medium |

### 7.2 Quality Gate Requirements

| Requirement | Success Criteria | Verification |
|---|---|---|
| **Backward compatibility** | Zero breaking changes, deprecation warnings | Integration test suite |
| **Performance regression** | <2% regression on refactored hot paths | Benchmark suite (criterion) |
| **Test coverage** | >85% maintained | CI gate with coverage thresholds |
| **Compiler warnings** | Zero warnings | CI gate, deny warnings in Cargo.toml |
| **Documentation** | 100% of public API items documented | CI gate, cargo doc --document-private-items |
| **Code review** | Architecture + correctness + performance | Multi-agent review system |

### 7.3 Extensibility Requirements

| Requirement | Description | Benefit |
|---|---|---|
| **Plugin architecture** | Analyzers implementable as trait objects | Third-party analyzer ecosystem |
| **Storage backends** | Swappable storage (DashMap, testing, Redis-backed) | Testing, different deployment profiles |
| **Configuration injection** | Config passed to analyzers, not global | Per-analyzer tuning, testing |
| **Exchange parsers as traits** | Generic ExchangeParser trait | Add exchanges without core changes |
| **Independent crate usage** | Each analytics crate usable standalone | Reuse in other projects |

### 7.4 Evolution Requirements (Post-Phase-4)

| Requirement | Rationale | Timeline |
|---|---|---|
| **Observable hot paths** | Tracing instrumentation with zero overhead | Phase 4 + Post-release |
| **Metrics emission** | Prometheus-compatible metrics per analyzer | Phase 4 + Post-release |
| **Distributed tracing** | OpenTelemetry integration for multi-process systems | Post-release (Q3 2026) |
| **Async/await readiness** | Support for Tokio-based pipelines | Post-release (Q3 2026) |
| **GPU acceleration research** | Feasibility study for analytics compute | Post-release (Q4 2026) |

---

## 8. Engineering Standards & Research Findings

### 8.1 Rust Best Practices (Research Synthesis)

| Pattern | Recommendation | Performance | Use Case |
|---|---|---|---|
| **Static dispatch** | Use generics + monomorphization | 0% overhead | Hot paths |
| **Dynamic dispatch** | Trait objects for plugins | 1-2% overhead | Pluggable analyzers |
| **Zero-cost abstractions** | Generics with trait bounds | 0% (compiles away) | RollingWindow<T> |
| **DashMap** | Replace RwLock<HashMap> | 15-20x throughput | Concurrent state |
| **Tokio async** | For I/O-bounded operations | Minimal | WebSocket feeds |
| **Rayon** | Batch parallelism >1000 items | Linear scaling | Batch analytics |
| **#[inline]** | Hot path small functions | Eliminates call overhead | Validators |

**Key Findings from RUST_BEST_PRACTICES.md**:
- Use static dispatch in hot paths (0% overhead vs 1-2% for trait objects)
- Replace RwLock<HashMap> with DashMap for 15-20x throughput improvement
- DashMap already in dependency tree - no new external dependencies
- Trait objects safe for analyzer plugins where pluggability needed
- Implement StorageBackend trait for testability instead of direct DashMap coupling

### 8.2 Financial Engineering Standards

| Metric | Standard Formula | Industry Benchmark |
|---|---|---|
| **Sharpe Ratio** | (Return - RiskFree) / StdDev | >1.0 desirable |
| **Sortino Ratio** | (Return - RiskFree) / DownsideDev | >1.5 desirable |
| **Maximum Drawdown** | Peak-to-trough decline | <20% acceptable |
| **VaR (95%)** | 1.65 × σ (parametric) | Per position limits |
| **PIN** | Prob(Informed Trading) | <0.50 low toxicity |
| **VPIN** | Volume-weighted PIN | <0.50 low toxicity |
| **Kyle's Lambda** | Price impact / volume | <0.01 low impact |

**Exchange Protocol Standards**:
- Binance: WebSocket with mixed binary+JSON, @aggTrade stream
- Coinbase: JSON frames with match/done/open message types
- Alpaca: SIP/CTA feed v2 format
- Polygon: A (aggregate), Q (quote), T (trade) message types
- FIX 4.2: TCP with Logon → MarketDataRequest → Snapshot

**Risk Calculation Standards** (from GREEKS_RESEARCH.md):
- Delta: ∂V/∂S (first derivative of option price to underlying)
- Gamma: ∂²V/∂S² (second derivative, convexity)
- Vega: ∂V/∂σ (sensitivity to volatility)
- Theta: ∂V/∂t (time decay, daily decay)
- Rho: ∂V/∂r (interest rate sensitivity)

### 8.3 Research Synthesis Requirements

| Source Document | Key Finding | Integration |
|---|---|---|
| RUST_BEST_PRACTICES.md | Static dispatch 0% overhead | Analyzer traits use generics |
| RUST_BEST_PRACTICES.md | DashMap 15-20x faster | Replace RwLock patterns |
| ARCHITECTURALANALYSIS.md | 50+ DashMap coupling sites | Storage trait abstraction |
| ARCHITECTURALANALYSIS.md | 5 window implementations | Generic RollingWindow<T> |
| QUANTITATIVE_FINANCE_REFERENCE.md | Industry risk standards | Standardize metrics |
| GREEKS_RESEARCH.md | Greeks formulas verified | Use standard implementations |

---

## 9. Current State Analysis (Verified via Codebase Exploration)

### 9.1 Architecture Overview (6 Crates, 117 Files, ~141K LOC)

**Crate Structure:**

| Crate | Purpose | Modules | Estimated LOC |
|-------|---------|---------|---------------|
| **fin-stream-core** | Foundational data primitives | tick, ohlcv, aggregator, book, orderbook, health, session, latency, ring, persistence | ~35K |
| **fin-stream-net** | Network I/O, protocols, WebSocket | ws, fix, grpc, portfolio_feed, circuit_breaker | ~20K |
| **fin-stream-trading** | Trading strategies, risk, execution | risk_engine, position_manager, backtest, market_maker, statarb, pairs_trader | ~25K |
| **fin-stream-analytics** | Analytics, features, signals | norm, microstructure, volatility_forecast, alpha_engine, anomaly, circuit, liquidity | ~45K |
| **fin-stream-simulation** | Market simulation, order book replay | market_simulator, replay, lob_sim, snapshot | ~10K |
| **fin-stream-experimental** | Research, experimental features | synthetic, mev, lorentz, news, compression | ~6K |

### 9.2 SOLID Principle Violations (Verified via Code Inspection)

#### Single Responsibility Principle (SRP) - 5 Critical Violations

1. **TickNormalizer** (`/crates/core/src/tick/mod.rs:11463-11564`):
   - Handles 4 exchange parsers (Binance, Coinbase, Alpaca, Polygon)
   - Handles formatting and caching
   - Fix: Extract ExchangeParser trait; separate concerns

2. **NormalizedTick** (`/crates/core/src/tick/mod.rs:100-700`):
   - Exposes 50+ public methods with overlapping functionality
   - Duplicated side-checking logic across methods
   - Fix: Break into focused interfaces (PriceTick, AgeTick, SideTick)

3. **OrderBook** (`/crates/core/src/book/mod.rs:128-195`):
   - Handles application logic, book reconstruction, sequence tracking
   - Fix: Separate concerns into OrderBookDelta, SequenceValidator

4. **RiskEngine** (`/crates/trading/src/risk_engine/mod.rs:160-180`):
   - Manages Greeks computation, price storage, portfolio aggregation
   - Uses two separate DashMaps with mixed responsibilities
   - Fix: Extract Greek computation and storage into separate modules

5. **MinMaxNormalizer** (`/crates/analytics/src/norm/mod.rs:63-300+`):
   - Manages window state, caching, recomputation, denormalization
   - Fix: Separate into StatefulNormalizer, CachingLayer

#### Open/Closed Principle (OCP) - 3 Critical Violations

1. **Exchange Parser Logic** (`/crates/core/src/tick/mod.rs:11463-11564`):
   - Requires code modification to add new exchanges
   - Fix: Implement ExchangeParser trait pattern

2. **Arbitrage Checking** (`/crates/analytics/src/multi_exchange/mod.rs:254-299`):
   - Hard-coded arbitrage types
   - Fix: Strategy pattern for arbitrage detectors

3. **Order Routing** (`/crates/trading/src/order_router/mod.rs:159-175`):
   - Venue selection requires modifying core logic
   - Fix: Strategy pattern for routing algorithms

#### Dependency Inversion Principle (DIP) - 3 Critical Violations

1. **Position Manager** (`/crates/trading/src/position_manager/mod.rs:173`):
   - Direct dependency on `DashMap<String, PositionState>`
   - Should use abstract PositionStorage trait

2. **Multi-Exchange Aggregator** (`/crates/analytics/src/multi_exchange/mod.rs:180`):
   - Direct dependency on concrete `Arc<DashMap<Exchange, ExchangeState>>`
   - Should depend on abstract storage

3. **Data Pipeline** (`/crates/core/src/data_pipeline/mod.rs:50+`):
   - Direct dependency on `VecDeque<f64>` for history
   - Should inject history storage interface

### 9.3 DRY Violations - 50+ Repetitions

#### Validation Logic (8+ instances)
- Price/quantity validation repeated in: tick_filter, quality, aggregator, book
- Stale data checks repeated in: health, data_pipeline, tick_filter
- Exchange string parsing repeated 3+ times with identical match statements
- Timestamp conversion repeated 6+ times independently

#### Parser Duplication (4 exchange parsers)
- All 4 parsers (Binance, Coinbase, Alpaca, Polygon) follow identical structure
- Extract field → Parse side → Handle timestamp → Construct NormalizedTick
- Refactoring opportunity: FieldExtractor trait, ~600 LOC duplication

#### Configuration Pattern (15+ structs)
- HaltConfig, VpinConfig, NoiseConfig, AnomalyDetectorConfig, 11 others
- All ~80% identical: validation in new(), builders, serialization
- Refactoring opportunity: Generic ConfigBuilder<T>, ~800 LOC duplication

#### Trait Implementations (40+ Display, 30+ FromStr, 25+ Default)
- All Display/FromStr follow identical `match self { ... }` pattern
- All Default implementations wrap `.new()`
- Opportunity: Derive macros or procedural macros

### 9.4 Complex Functions (10 functions with CC > 8)

| Rank | Function | File | Complexity | Lines |
|------|----------|------|-----------|-------|
| 1 | `normalize()` | tick/mod.rs | ~18 | 38 |
| 2 | `apply()` | book/mod.rs | ~12 | 37 |
| 3 | `normalize_tick()` | data_pipeline/mod.rs | ~14 | 50 |
| 4 | `process_trade()` | aggregator/bars.rs | ~10 | 45 |
| 5 | `check_thresholds()` | circuit/mod.rs | ~12 | 42 |

All candidates for refactoring via strategy pattern or trait extraction.

### 9.5 Trait Coverage Analysis

**Current Traits**: 13 public traits, ~101 implementations
- **Single-implementor traits**: 6 (over-abstraction: BarPriceAnalytics, BarShapeAnalytics, etc.)
- **Multi-implementor traits**: 7 (healthy: TickSource, PriceModel, NewsSource)
- **Gap**: 50+ missing trait abstractions (ExchangeParser, StorageBackend, ConfigProvider, etc.)

### 9.6 DashMap & Concurrency Usage

- **DashMap instances**: 113 across 22 files (excessive coupling)
- **RwLock instances**: 1 (minimal; good practice)
- **Opportunity**: Extract StorageBackend trait, enable testing, reduce coupling

### 9.7 Git History Insights (384 commits in 30 days)

- **Rapid development**: ~55 commits/day during active sprint
- **Workspace modernization**: Current branch `feat/workspace-modernization`
- **Latest additions**: Rounds 25-32 with volatility_forecast, portfolio_optimizer, etc.
- **Refactoring pattern**: Systematic duplication elimination across rounds 49-67
- **No merge conflicts**: Clean linear history on main
- **CI/Build issues**: Recent fixes for clippy, cargo-deny (now resolved)

### 9.8 KISS (Keep It Simple, Stupid) Violations

**Over-complexity Identified:**

1. **TickNormalizer normalize()** (38 lines, CC 18):
   - 4 exchange-specific branches with nested parsing
   - Solution: Extract to ExchangeParser trait, ~8 lines per implementation

2. **OrderBook apply()** (37 lines, CC 12):
   - Combines delta application, validation, sequence checking
   - Solution: Separate validators, ~12 lines core logic

3. **normalize_tick()** (50 lines, CC 14):
   - Spike detection + quality flag + history management
   - Solution: Separate QualityChecker, ~20 lines core logic

4. **6 bar analytics traits** (overlapping, redundant):
   - Should consolidate into single OhlcvAnalytics trait
   - Clients forced to implement redundant patterns

5. **23+ Config structs** (each 30-60 lines):
   - 80% identical builder pattern + validation
   - Solution: Generic ConfigBuilder<T> or macro, ~10 lines per config

### 9.9 YAGNI (You Aren't Gonna Need It) Violations

**Potentially Unnecessary Features:**

1. **Looping replay** (medium priority, ~5% usage predicted):
   - Autostart feature for backtesting; rarely used
   - Keep but move to optional feature flag

2. **Multi-tenant isolation** (planned but not implemented):
   - Per-portfolio sandboxing; complex to implement
   - Defer to Phase 4 or post-release

3. **GPU acceleration research** (aspirational):
   - Listed in performance roadmap; no current work
   - Defer to post-release Q4 2026

4. **Arrow IPC export** (planned, high effort):
   - ML feature export; nice-to-have
   - Defer to Phase 4

5. **Experimental modules** (6 crates, ~6K LOC):
    - synthetic, mev, lorentz, news, compression
    - Keep for research but document as unstable

---

## 10. Development Tooling & Quality Automation (Prek)

### 10.1 Prek Hook Requirements

| Hook | Purpose | Stage | Requirement |
|------|---------|-------|-------------|
| **Rustfmt** | Code formatting enforcement | prek stage 1 | All commits must pass |
| **Clippy** | Rust linting (800+ checks) | prek stage 1 | All commits must pass |
| **Cargo Audit** | Security vulnerability scanning | prek stage 2 | All pushes must pass |
| **Cargo Deny** | Dependency policy enforcement | prek stage 2 | All pushes must pass |
| **Taplo** | TOML formatting | prek stage 1 | Optional; nice-to-have |
| **Typos** | Spell checking | prek stage 2 | Optional; informational |

**Execution Strategy**:
- **prek stage 1**: Fast checks (~8-12s) run on every commit (Rustfmt, Clippy)
- **prek stage 2**: Comprehensive checks (~10-15s) run before pushing to remote (Cargo Audit, Deny)
- **Rationale**: Developers get instant feedback on commit; push is final quality gate

### 10.2 Code Quality Standards

**Formatting Standards**:
- Edition: Rust 2021
- Line width: 100 characters
- Hard tabs: Disabled (4-space indentation)
- Try shorthand: Enabled (`?` operator)
- Field init shorthand: Enabled

**Linting Standards**:
- Clippy: All warnings treated as errors in prek stage 1 (`-D warnings`)
- Documentation: 100% of public API documented
- Compiler warnings: Zero in all commits
- Unsafe code: Forbidden in public API (`#![forbid(unsafe_code)]`)

**Security Standards**:
- Cargo Audit: Zero vulnerabilities (advisory DB updated automatically)
- Cargo Deny: No GPL/AGPL dependencies; only approved licenses
- Dependency versions: No yanked crates; no unmaintained crates

### 10.3 Developer Machine Setup

**Prerequisites**:
```bash
# Install prek framework
pip install pre-commit
# or: brew install pre-commit (macOS)

# Install Rust tools (if not present)
rustup component add rustfmt clippy
cargo install cargo-audit cargo-deny taplo-cli typos-cli
```

**First-Time Setup**:
```bash
cd /home/tommyk/projects/quant/engines/fin-stream
pre-commit install
pre-commit install --hook-type pre-push
pre-commit run --all-files  # Verify all checks pass
```

**Typical Developer Workflow**:
```bash
# Make changes
git add file.rs
git commit -m "feat(core): implement validator consolidation"
# Prek stage 1 hooks run automatically; must pass

# Push to remote
git push origin feat/branch-name
# Prek stage 2 hooks run automatically; must pass before actual push
```

### 10.4 Continuous Integration Integration

**GitHub Actions Workflow** (`.github/workflows/pre-commit.yml`):
- Runs on every pull request
- Caches prek results for speed (< 5s on warm cache)
- Blocks merge if any checks fail
- Reports individual hook failures

**CI Success Criteria**:
- ✓ All prek hooks pass
- ✓ Cargo build succeeds
- ✓ All tests pass
- ✓ Test coverage > 85%
- ✓ Documentation builds without warnings

---

## Glossary

| Term | Definition |
|---|---|
| **NBBO** | National Best Bid and Offer (best prices across exchanges) |
| **OFI** | Order Flow Imbalance; measure of buy vs. sell pressure |
| **VPIN** | Volume-synchronized Probability of Informed Trading |
| **LEE-READY** | Algorithm for classifying trades as buyer/seller-initiated |
| **HALF-LIFE** | Time for mean-reverting process to revert halfway to equilibrium |
| **ADF** | Augmented Dickey-Fuller test for cointegration |
| **PIN** | Probability of Informed Trading (toxicity metric) |
| **SPSC** | Single-Producer, Single-Consumer (ring buffer pattern) |
| **SRP** | Single Responsibility Principle (SOLID) |
| **OCP** | Open/Closed Principle (SOLID) |
| **LSP** | Liskov Substitution Principle (SOLID) |
| **ISP** | Interface Segregation Principle (SOLID) |
| **DIP** | Dependency Inversion Principle (SOLID) |
| **DRY** | Don't Repeat Yourself principle |
| **KISS** | Keep It Simple, Stupid principle |
| **YAGNI** | You Aren't Gonna Need It principle |

