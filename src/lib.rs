// SPDX-License-Identifier: MIT
#![cfg_attr(docsrs, feature(doc_cfg))]
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
//! threads), [`OhlcvAggregator`] (ticks to bars), [`OrderBook`],
//! [`HealthMonitor`] (stale-feed detection), [`WsManager`]
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
pub mod interop;
pub mod telemetry;
pub mod aggregator;

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

pub mod portfolio_risk;
pub mod regime;
pub mod synthetic;
pub mod toxicity;
pub mod noise;

pub mod ofi;

pub mod microstructure;

pub mod quality;

pub mod circuit;

pub mod lob_sim;

pub mod arbitrage;

pub mod compression;

pub mod network;

pub mod marketmaker;

pub mod latency;

pub mod statarb;

pub mod pipeline;

pub mod volatility_forecast;

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

pub mod risk;

pub mod classifier;

pub mod spread;

pub mod depth;

pub mod funding;

pub mod pattern;

pub mod news;

pub mod signal;

pub mod position_tracker;

pub mod venue_selector;

pub mod tick_filter;

pub mod hft;

pub mod cross_asset;

pub mod market_hours;

pub mod event_detector;

pub mod session_analysis;

pub mod trade_analytics;

pub mod candle;

pub mod alert;

pub mod persistence;

pub mod technical;

pub mod order_flow;

pub mod market_maker;

pub mod risk_engine;

pub mod data_pipeline;

pub mod signal_combiner;

pub mod backtest;

pub mod liquidity;

pub mod order_book_sim;

pub mod microstructure_v2;

pub mod vwap_tracker;

pub mod spread_monitor;

pub mod alpha_engine;

pub mod position_manager;

pub mod market_simulator;

pub mod tick_processor;

pub mod performance_attribution;

pub mod order_router;

pub mod pairs_trader;

pub mod signal_processor;

pub mod regime_detector;

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
