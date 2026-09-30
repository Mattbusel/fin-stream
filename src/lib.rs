// SPDX-License-Identifier: MIT
#![deny(missing_docs)]
#![doc(
    html_logo_url = "https://gitlab.com/mattbusel/fin-stream/-/raw/main/assets/logo.svg",
    html_favicon_url = "https://gitlab.com/mattbusel/fin-stream/-/raw/main/assets/logo.svg"
)]
//! # fin-stream
//!
//! Turn raw trade messages from crypto and stock exchanges into one clean,
//! exact tick format, move them between threads fast, and roll them into
//! price bars.
//!
//! ![cargo run --example tape: 40 BTC-USD trades from four venues streaming live, with 2-second bars and a summary](https://gitlab.com/mattbusel/fin-stream/-/raw/main/assets/demo.gif)
//!
//! ```text
//! cargo add fin-stream serde_json
//! ```
//!
//! The main types: [`TickNormalizer`] and [`NormalizedTick`] (four wire
//! formats in, one tick out), [`SpscRing`] (lock-free hand-off between
//! threads), [`OhlcvAggregator`] (ticks to bars), [`OrderBook`](book::OrderBook),
//! [`HealthMonitor`] (stale-feed detection), [`WsManager`](ws::WsManager)
//! (WebSocket with reconnect), and one error type, [`StreamError`]. Prices are
//! exact [`rust_decimal::Decimal`]s.
//!
//! ## Four wire formats in, one tick out
//!
//! Binance, Coinbase, Alpaca and Polygon each spell a trade differently.
//! [`TickNormalizer`] maps all of them onto one [`NormalizedTick`]. From there
//! the stages take and return plain values: hand ticks to another thread with
//! [`SpscRing`], roll them into bars with [`OhlcvAggregator`], watch the feed
//! with [`HealthMonitor`].
//!
//! ```
//! use fin_stream::ohlcv::{OhlcvAggregator, Timeframe};
//! use fin_stream::ring::SpscRing;
//! use fin_stream::tick::{Exchange, NormalizedTick, RawTick, TickNormalizer};
//! use serde_json::json;
//!
//! # fn main() -> Result<(), fin_stream::StreamError> {
//! let normalizer = TickNormalizer::new();
//! let ring: SpscRing<NormalizedTick, 64> = SpscRing::new();
//! let (tx, rx) = ring.split();
//!
//! // Feed side: a Coinbase match and a Binance trade, one second apart.
//! let coinbase = json!({"price": "64250.10", "size": "0.012", "side": "buy",
//!                       "time": "2026-09-24T14:30:00.112Z"});
//! let binance = json!({"p": "64251.30", "q": "0.40", "m": true, "t": 7u64,
//!                      "T": 1_790_260_201_205u64});
//! for (venue, payload) in [(Exchange::Coinbase, coinbase), (Exchange::Binance, binance)] {
//!     tx.push(normalizer.normalize(RawTick::new(venue, "BTC-USD", payload))?)?;
//! }
//!
//! // Consumer side: roll ticks into one-second bars.
//! let mut bars = OhlcvAggregator::new("BTC-USD", Timeframe::Seconds(1))?;
//! let mut closed = Vec::new();
//! while let Ok(tick) = rx.pop() {
//!     closed.extend(bars.feed(&tick)?);
//! }
//! assert_eq!(closed.len(), 1); // the 14:30:00 bar closed when the 14:30:01 trade arrived
//! assert_eq!(closed[0].close.to_string(), "64250.10");
//! # Ok(())
//! # }
//! ```
//!
//! ![Animated diagram: WebSocket frames become RawTicks, TickNormalizer turns four venue formats into NormalizedTicks, a feed thread pushes them through SpscRing, and the consumer rolls them into OHLCV bars and features](https://gitlab.com/mattbusel/fin-stream/-/raw/main/docs/img/pipeline.svg)
//!
//! ## Runnable examples
//!
//! The repository has four examples that need no network and no API keys:
//!
//! | Command | Shows |
//! |---|---|
//! | `cargo run --example tape` | four venues normalized on a feed thread, handed across an `SpscRing`, printed as a time-and-sales tape and rolled into bars |
//! | `cargo run --example normalize` | one trade in four wire formats, the `NormalizedTick` each becomes, and typed rejections |
//! | `cargo run --example feed_health` | `HealthMonitor` marking a stalled feed stale and opening its circuit |
//! | `cargo run --example replay` | `TickReplayer` streaming a recorded NDJSON file into 30-second bars |
//!
//! ## Performance
//!
//! Criterion medians from `cargo bench --bench tick_hot_path` (i7-13700KF,
//! single thread): push and pop of a `u64` through [`SpscRing`] 2.3 ns;
//! build, push and pop a [`NormalizedTick`] 53 ns; normalize a Binance trade
//! including the JSON payload clone 714 ns; feed a tick into an open bar 101 ns;
//! apply an order book delta 65 ns. Numbers vary by machine; the benchmark is in
//! the repository.
//!
//! ## Modules
//!
//! **Ingest and transport**
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`ws`] | WebSocket connection loop with reconnect and backoff |
//! | [`tick`] | Raw exchange payloads to [`NormalizedTick`] for Binance, Coinbase, Alpaca, Polygon |
//! | [`ring`] | Lock-free single-producer single-consumer ring buffer |
//! | [`agg`] | Merge N feeds (best bid, best ask, VWAP, primary with fallback) and cross-feed arbitrage detection |
//! | [`multi_exchange`] | NBBO-style consolidated best bid and ask across exchanges |
//! | [`portfolio_feed`] | One WebSocket task per asset, merged into one tick stream |
//! | [`protocol`] | Unified `MarketEvent` enum, `JsonStreamAdapter`, `EventStream` |
//! | [`fix`] | FIX 4.2 parse, serialize, logon and market data |
//! | [`grpc`] | gRPC tick stream endpoint (behind the `grpc` feature) |
//! | [`replay`] | NDJSON tick replay with speed control, via the [`TickSource`] trait |
//! | [`snapshot`] | Binary tick recording and N-speed replay |
//! | [`synthetic`] | Seeded GBM, jump-diffusion, Ornstein-Uhlenbeck and Heston generators |
//!
//! **Bars, books and health**
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`ohlcv`] | OHLCV bars at any timeframe, optional gap-fill bars |
//! | [`aggregator`] | Time, tick and volume bars from trades |
//! | [`book`] | Order book delta streaming and crossed-book detection |
//! | [`health`] | Feed staleness detection and per-feed circuit breaker |
//! | [`circuit_breaker`] | WebSocket circuit breaker with degraded-mode synthetic ticks |
//! | [`circuit`] | Per-symbol halts on price spikes or volume surges |
//! | [`session`] | Market session and trading-hours classification |
//! | [`quality`] | Feed quality score from latency percentiles, gap rate and duplicate rate |
//! | [`anomaly`] | Price spikes, volume spikes, sequence gaps, timestamp inversions |
//! | [`error`] | The [`StreamError`] hierarchy |
//!
//! **Features and analytics**
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`norm`] | Rolling min-max and z-score normalizers |
//! | [`ofi`] | Order flow imbalance, rolling accumulator, VPIN estimator |
//! | [`toxicity`] | PIN, VPIN, Kyle lambda and Amihud illiquidity |
//! | [`microstructure`] | Amihud, Kyle lambda, Roll spread and bid-ask bounce on one stream |
//! | [`regime`] | Trending, mean-reverting and volatility regimes from Hurst, ADX and realised vol |
//! | [`correlation`] | Streaming N by N Pearson correlation matrix |
//! | [`lorentz`] | Lorentz transforms for price-time feature engineering |
//! | [`mev`] | Sandwich, frontrun and backrun heuristics on tick slices |
//! | [`predictive_book`] | Online logistic regression for next-tick direction from L2 features |
//! | [`execution`] | Implementation shortfall, market impact and slippage |
//! | [`noise`] | Roll spread, realised kernel and de-noised efficient price |
//!
//! The crate also carries further analytics and simulation modules (backtest,
//! market maker, pairs trading, signal processing, portfolio optimisation and
//! others); see the module list below.

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

/// Volatility forecasting: GARCH(1,1), EGARCH, and realized volatility with bipower variation.
pub mod volatility_forecast;

/// Portfolio optimization: mean-variance, risk parity, and Almgren-Chriss execution optimization.
pub mod portfolio_optimizer;

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

/// Compiles and runs every Rust block in README.md as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// Compiles and runs every Rust block in docs/EXAMPLES.md as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../docs/EXAMPLES.md")]
pub struct ExamplesDoctests;

/// Compiles and runs every Rust block in docs/REFERENCE.md as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../docs/REFERENCE.md")]
pub struct ReferenceDoctests;
