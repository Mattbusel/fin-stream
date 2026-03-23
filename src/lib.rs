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

/// Real-time portfolio risk tracking: positions, P&L, VaR, drawdown, Sharpe, and volatility.
pub mod portfolio_risk;
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

/// Statistical arbitrage detector: cointegration testing (simplified ADF), spread z-score
/// monitoring, and real-time Long/Short/Exit/Neutral signal generation for symbol pairs.
pub mod statarb;

/// Composable tick normalization pipeline: `TickFilter` and `TickTransform` traits,
/// built-in filters (price range, volume, symbol, staleness) and transforms
/// (price rounding, volume normalization, timestamp alignment).
pub mod pipeline;

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

/// Real-time risk metrics: rolling volatility, historical VaR, max drawdown,
/// Sharpe ratio, and concurrent multi-symbol risk monitoring with portfolio VaR.
pub mod risk;

/// Trade classifier: Lee-Ready algorithm for buyer/seller-initiated classification,
/// tick-test fallback, and rolling trade flow metrics accumulation.
pub mod classifier;

/// Bid-ask spread estimation from trade data:
/// Roll (1984) serial-covariance model, Corwin-Schultz (2012) high-low estimator,
/// and a rolling `SpreadAnalyzer` with effective and realized spread computations.
pub mod spread;

/// Order book depth imbalance signals: raw and weighted imbalance, market-impact
/// (slippage) estimation, bid-ask spread, and a DashMap-backed per-symbol tracker.
pub mod depth;

/// Perpetual-futures funding rate tracker and cash-and-carry arbitrage detector.
/// Concurrent per-symbol rolling history with trend analysis and high-funding filters.
pub mod funding;

/// Real-time candlestick pattern detection on streaming bars.
/// PatternDetector (5-bar window), PatternSignal, StreamingPatternMonitor (DashMap, multi-symbol).
pub mod pattern;

/// News sentiment feed integration: NewsStore, SentimentScore, NewsSource trait, MockNewsSource,
/// SentimentAlert, and NewsMonitor with recency-decayed aggregation and alert generation.
pub mod news;

/// Trading signal aggregator with confidence weighting and exponential time-decay.
/// Provides SignalType, Signal, WeightedSignal, SignalAggregator, AggregatedSignal, SignalFilter.
pub mod signal;

/// Real-time position and P&L tracking: Position, Trade, PositionTracker, PositionRiskMetrics.
/// Supports open, average-up, partial close, full close, opposite-side reduce/flip.
pub mod position_tracker;

/// Smart order routing venue selector: composite venue scoring (fill rate, fees, latency),
/// best-venue selection, large-order splitting, and exponential-decay performance tracking.
pub mod venue_selector;

/// Multi-stage tick data quality filter: size, staleness, duplicate, and outlier filters
/// with per-stage rejection statistics and a composable TickFilter pipeline.
pub mod tick_filter;

/// High-frequency trading signals: order flow imbalance, micro-price estimation,
/// toxic flow detection, trade sign aggregation, and quote/trade speed ratios.
pub mod hft;

/// Cross-asset correlation streaming: online Pearson correlation for asset pairs,
/// NxN correlation matrix, and statistical correlation breakdown detection.
pub mod cross_asset;

/// Market hours checker: TradingSession, MarketCalendar, MarketHoursChecker with
/// is_open, next_open, time_until_close, and session_for queries.
pub mod market_hours;

/// Market event detector: breakout, volume spike, reversal, and gap-open detection
/// from streaming ticks, with per-symbol state management.
pub mod event_detector;

/// Trading session analysis: intraday volume/return bucket profiles, day-of-week
/// seasonality effects, session pattern classification (U/L/J/Uniform), and
/// best-trading-hours detection by relative volume threshold.
pub mod session_analysis;

/// Execution quality analytics: slippage, implementation shortfall, VWAP deviation,
/// price improvement, fill rate, venue comparison, and formatted TCA reports.
pub mod trade_analytics;

/// OHLCV candle builder for multiple timeframes: `Candle`, `Timeframe`, `CandleBuilder`,
/// `MultiTimeframeCandler`, and stateless `CandleIndicators` (typical price, true range,
/// body pct, doji, engulfing pattern).
pub mod candle;

/// Price and volume alert system: `AlertCondition` (PriceAbove/Below, PriceChangePct,
/// VolumeSpike, BollingerBreakout, CrossOver), `AlertSeverity`, `Alert`, and
/// `AlertManager` with per-symbol tick state evaluation.
pub mod alert;

/// Tick data persistence: binary serialization (8-byte magic, length-prefixed fields),
/// TickRecord, TickIndex, TickWriter (in-memory), TickReader with range queries.
pub mod persistence;

/// Technical analysis indicators: RSI (Wilder smoothing), MACD, Bollinger Bands,
/// ATR, Stochastic oscillator, EMA, and SMA.
pub mod technical;

/// Order flow analytics and VPIN: trade classification into volume buckets,
/// rolling VPIN toxicity score, net order flow, price impact regression, and cumulative delta.
pub mod order_flow;

/// Market making quote engine: inventory-aware spread computation, fill processing,
/// mark-to-market P&L, inventory risk, and aggregate statistics.
pub mod market_maker;

/// Real-time portfolio risk calculation: Greeks aggregation, P&L attribution, VaR,
/// stress testing, and concurrent DashMap-backed risk engine.
pub mod risk_engine;

/// Market data normalization and quality: tick normalization, OHLCV bar construction,
/// data quality flags, quality reporting, and end-to-end DataPipeline.
pub mod data_pipeline;

/// IC-based alpha signal combination: EqualWeight, ICWeight, ICSquaredWeight,
/// InformationRatio, KellyCriterion weighting; Gram-Schmidt orthogonalization;
/// pairwise correlation; diversification ratio; and performance breakdown.
pub mod signal_combiner;

/// Tick-level backtesting engine: order submission, Market/Limit/StopLoss matching,
/// fill processing, mark-to-market equity curve, and performance metrics (Sharpe, drawdown, win rate).
pub mod backtest;

/// Order book depth analytics: PriceLevel, OrderBook, VWAP, imbalance, slippage estimation,
/// depth chart data, and DepthMetrics.
pub mod liquidity;

/// Limit order book simulation with price-time priority matching: LimitOrder, Trade,
/// OrderBookSim, price_to_key, best_bid, best_ask, spread, and order book snapshots.
pub mod order_book_sim;

/// Extended market microstructure: tick rule, Lee-Ready classification, PIN estimation,
/// LOB imbalance, price impact model, Hasbrouck information share, and VPIN flow toxicity.
pub mod microstructure_v2;

/// Real-time VWAP and TWAP computation with period resets and execution scheduling.
/// Provides VwapTracker, TwapTracker, ExecutionScheduler, and ExecutionAlgo.
pub mod vwap_tracker;

/// Bid-ask spread analytics and decomposition: SpreadMonitor, SpreadDecomposition,
/// effective/realized spread, SpreadAlerts, and SpreadStats.
pub mod spread_monitor;

/// Alpha signal generation: momentum, mean-reversion, and volatility alphas
/// with a concurrent DashMap-backed AlphaEngine for multi-symbol signal management.
pub mod alpha_engine;

/// Position sizing and risk management: Kelly-like sizing, stop-loss/take-profit controls,
/// daily P&L limits, portfolio VaR, and concurrent DashMap-backed PositionManager.
pub mod position_manager;

/// Agent-based market simulator with heterogeneous agents (MarketMaker, TrendFollower,
/// MeanReverter, NoiseTrader, Informed), continuous double auction, and price formation.
pub mod market_simulator;

/// High-throughput tick processing pipeline: composable TickFilter, per-symbol TickStats,
/// VWAP, TickRouter with DashMap routing, and concurrent TickProcessor with atomics.
pub mod tick_processor;

/// Performance attribution analysis: Brinson-Fachler allocation/selection/interaction,
/// rolling attribution reports, and OLS-based factor attribution.
pub mod performance_attribution;

/// Smart order routing: venue registry, BestPrice/BestFillRate/LowestFee/SplitBestN/TWAP/VWAP
/// strategies, and a concurrent DashMap-backed SmartOrderRouter.
pub mod order_router;

/// Statistical pairs trading: cointegration testing (OLS + ADF), spread z-score,
/// half-life estimation, and real-time Long/Short/Exit/Neutral signal generation.
pub mod pairs_trader;

/// Composable signal processing pipeline: normalization (MinMax, ZScore, Robust),
/// IIR filtering, smoothing, clipping, lag, diff, log, rank, and signal combination.
pub mod signal_processor;

/// Hidden Markov model regime detector: 5-state (LowVolBull/HighVolBull/LowVolBear/HighVolBear/Crisis)
/// with EWMA feature extraction, Bayesian state-prob updates, and confidence tracking.
pub mod regime_detector;

/// Real-time bid-ask spread, order book depth, and liquidity impact cost monitoring.
/// Provides EWMA spread tracking, depth imbalance, Kyle-lambda impact estimation, and liquidity scoring.
pub mod liquidity_monitor;
