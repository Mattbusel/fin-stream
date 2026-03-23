// SPDX-License-Identifier: MIT
#![deny(missing_docs)]
//! # fin-stream
//!
//! Lock-free streaming primitives for real-time financial market data.
//!
//! ## Architecture
//!
//! ```text
//! Tick Source (live WebSocket or TickReplayer)
//!     |
//!     v
//! SPSC Ring Buffer  (lock-free, zero-allocation hot path)
//!     |
//!     v
//! FeedAggregator    (merge N feeds, VWAP / BestBid / BestAsk / Fallback)
//!     |
//!     +---> ArbDetector  (cross-feed arbitrage opportunity detection)
//!     |
//!     v
//! OHLCV Aggregator  (streaming bar construction at any timeframe)
//!     |
//!     v
//! MinMax Normalizer (rolling-window coordinate normalization)
//!     |
//!     +---> Lorentz Transform  (spacetime boost for feature engineering)
//!     |
//!     v
//! Downstream (ML model, trade signal engine, order management)
//! ```
//!
//! ## Performance
//!
//! The SPSC ring buffer sustains 100 K+ ticks/second with no heap allocation
//! on the fast path. All error paths return `Result<_, StreamError>` — the
//! library never panics on the hot path. Construction functions validate their
//! arguments and panic on misuse with a clear message (e.g. ring capacity of 0,
//! normalizer window size of 0).
//!
//! ## Modules
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! [`agg`] | Cross-feed aggregation, merge strategies, and arb detection |
//! [`aggregator`] | Bar aggregator: time/tick/volume-based OHLCV aggregation from trades |
//! [`anomaly`] | Tick anomaly detection: price spikes, volume spikes, sequence gaps, timestamp inversions |
//! [`book`] | Order book delta streaming and crossed-book detection |
//! [`circuit_breaker`] | WebSocket circuit breaker with degraded-mode synthetic tick emission |
//! [`correlation`] | Streaming NxN Pearson correlation matrix (Welford, DashMap) |
//! [`error`] | Typed error hierarchy (`StreamError`) |
//! [`fix`] | FIX 4.2 session adapter — parse, serialize, logon, market data |
//! [`grpc`] | gRPC streaming endpoint (`grpc` feature) — expose tick stream via tonic |
//! [`health`] | Feed staleness detection and circuit-breaker |
//! [`lorentz`] | Lorentz spacetime transforms for time-series features |
//! [`mev`] | MEV detection scaffold — sandwich, frontrun, backrun heuristics |
//! [`multi_exchange`] | NBBO-style multi-exchange aggregation and arbitrage detection |
//! [`norm`] | Rolling min-max coordinate normalization |
//! [`ohlcv`] | OHLCV bar aggregation at arbitrary timeframes |
//! [`portfolio_feed`] | Multi-asset parallel WebSocket feed with merged tick stream |
//! [`protocol`] | Unified streaming protocol: `MarketEvent` enum, `JsonStreamAdapter`, `EventStream` |
//! [`replay`] | Historical NDJSON tick replay with speed control |
//! [`ring`] | SPSC lock-free ring buffer |
//! [`session`] | Market session and trading-hours classification |
//! [`snapshot`] | Snapshot-and-replay: binary tick recording and N-speed replay |
//! [`tick`] | Raw-to-normalized tick conversion for all exchanges |
//! [`ws`] | WebSocket connection management and reconnect policy |
//! [`predictive_book`] | Online logistic regression predicting next-tick direction from L2 imbalance, spread, depth |
//! [`execution`] | Execution quality monitor: implementation shortfall, market impact, slippage decomposition |
//! [`synthetic`] | Synthetic market data generator: GBM, jump-diffusion, Ornstein–Uhlenbeck, Heston stochastic-vol |
//! [`toxicity`] | Order flow toxicity: PIN, VPIN, Kyle λ, Amihud illiquidity — identifies informed (smart-money) trading |
//! [`noise`] | Microstructure noise filter: Roll spread estimator, realised kernel, de-noised efficient price |
//! [`regime`] | Real-time market regime detection: Trending, MeanReverting, High/LowVolatility (Hurst, ADX, realised vol) |
//! [`ofi`] | Order flow imbalance: `OrderFlowImbalance`, `OfiAccumulator`, `OfiMetricsComputer`, `ToxicityEstimator` (VPIN) |
//! [`microstructure`] | Market microstructure analytics: Amihud illiquidity, Kyle lambda, Roll spread, bid-ask bounce |
//! [`quality`] | Feed quality scoring: latency percentiles, gap rate, duplicate rate, composite score 0–100 |
//! [`circuit`] | Per-symbol circuit breakers: price-spike/volume-surge detection, Normal/Halted/Recovering FSM |

pub mod agg;
pub mod aggregator;

/// L2 Order Book Reconstruction: OrdF64-keyed BTreeMap books, delta application,
/// sequence validation, crossed-book detection, imbalance, and DashMap-backed manager.
pub mod orderbook;
pub mod anomaly;
pub mod book;
pub mod circuit_breaker;
pub mod correlation;
pub mod error;
pub mod fix;
pub mod grpc;
pub mod health;
pub mod lorentz;
pub mod mev;
pub mod multi_exchange;
pub mod norm;
pub mod ohlcv;
pub mod portfolio_feed;
pub mod protocol;
pub mod replay;
pub mod ring;
pub mod session;
pub mod snapshot;
pub mod tick;
pub mod ws;
pub mod predictive_book;
pub mod execution;
pub mod regime;
pub mod synthetic;
pub mod toxicity;
pub mod noise;

/// Order flow imbalance: OFI per tick, rolling accumulator, standardized metrics, and VPIN toxicity.
pub mod ofi;

/// Market microstructure analytics: Amihud illiquidity, Kyle's lambda, Roll spread, bid-ask bounce.
pub mod microstructure;

/// Feed quality scoring, gap detection, and tick deduplication.
/// Score = 100 * (1 - gap_rate) * (1 - dup_rate) * exp(-latency_p99 / 1000).
pub mod quality;

/// Per-symbol circuit breakers: halt on price spikes, volume surges, or both.
/// Manages one breaker per symbol with Normal/Halted/Recovering state machine.
pub mod circuit;

/// Full L3 limit order book simulator with price-time priority matching engine.
pub mod lob_sim;

/// Triangular and statistical arbitrage detectors with Kalman-filter spread estimation.
pub mod arbitrage;

/// Tick data compression: delta encoding, run-length encoding, ~8 bytes/tick binary format.
pub mod compression;

/// Price correlation graph, centrality measures, regime change detection, and contagion detection.
pub mod network;

/// Inventory-aware market maker simulator: symmetric spread quoting, skew by inventory,
/// fill processing, realised P&L accounting, and aggregate statistics.
pub mod marketmaker;

/// HDR-style latency histogram: powers-of-2 buckets with 4 sub-buckets, percentile queries,
/// and per-operation `LatencyTracker` with named histograms.
pub mod latency;

pub use agg::{AggregatorConfig, ArbDetector, ArbOpportunity, FeedAggregator, FeedHandle, MergeStrategy};
pub use aggregator::{AggregationMode, BarAggregator, BarBuilder};
pub use protocol::{
    BarEvent as ProtocolBarEvent, EventStream, FeedStatus, JsonStreamAdapter, MarketEvent,
    OrderBookEvent, QuoteEvent, StatusEvent, TradeSide, TradeEvent as ProtocolTradeEvent,
};
pub use anomaly::{AnomalyDetectorConfig, AnomalyEvent, AnomalyKind, TickAnomalyDetector};
pub use book::{BookDelta, BookSide, OrderBook, PriceLevel};
pub use circuit_breaker::{CircuitBreakerConfig, CircuitState, WsCircuitBreaker};
pub use correlation::{CorrelationPair, StreamingCorrelationMatrix};
pub use error::StreamError;
pub use fix::{FixError, FixMessage, FixParser, FixSession};
pub use health::{FeedHealth, HealthMonitor, HealthStatus};
pub use lorentz::{LorentzTransform, SpacetimePoint};
pub use mev::{MevCandidate, MevDetector, MevPattern};
pub use multi_exchange::{
    AggregatorConfig as MultiExchangeAggregatorConfig, ArbitrageOpportunity,
    ExchangeLatencyStats, MultiExchangeAggregator, Nbbo,
};
pub use norm::{MinMaxNormalizer, ZScoreNormalizer};
pub use ohlcv::{OhlcvAggregator, OhlcvBar, Timeframe};
pub use portfolio_feed::{AssetFeedConfig, AssetFeedStats, PortfolioFeed};
pub use replay::{ReplaySession, ReplayStats, TickReplayer, TickSource};
pub use ring::{SpscConsumer, SpscProducer, SpscRing};
pub use session::{MarketSession, SessionAwareness, TradingStatus};
pub use snapshot::{TickRecorder, TickReplayer as SnapshotReplayer};
pub use tick::{Exchange, NormalizedTick, RawTick, TickNormalizer};
pub use ws::{ConnectionConfig, ReconnectPolicy, WsManager};
pub use ofi::{
    NanoTimestamp as OfiNanoTimestamp, OfiAccumulator, OfiMetrics, OfiMetricsComputer,
    OfiSignal, OrderFlowImbalance, Side as OfiSide, TopOfBook, ToxicityEstimator, VpinResult,
};
pub use microstructure::{
    AmihudIlliquidity, BidAskBounce, KyleImpact, MicroTick, MicrostructureMonitor,
    MicrostructureReport, RollSpread,
};
