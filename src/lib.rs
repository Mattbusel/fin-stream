// SPDX-License-Identifier: MIT
//! # fin-stream
//!
//! Real-time market data streaming primitives.
//! Pure ingestion layer: WebSocket management, tick normalization across
//! exchanges, order book delta streaming, OHLCV aggregation, feed health
//! monitoring. Designed for 100K+ ticks/second throughput.

#[path = "book/mod.rs"]
pub mod book;
pub mod error;
#[path = "health/mod.rs"]
pub mod health;
#[path = "ohlcv/mod.rs"]
pub mod ohlcv;
#[path = "session/mod.rs"]
pub mod session;
#[path = "tick/mod.rs"]
pub mod tick;
#[path = "ws/mod.rs"]
pub mod ws;

pub use book::{BookDelta, BookSide, OrderBook, PriceLevel};
pub use error::StreamError;
pub use health::{FeedHealth, HealthMonitor, HealthStatus};
pub use ohlcv::{OhlcvAggregator, OhlcvBar, Timeframe};
pub use session::{MarketSession, SessionAwareness, TradingStatus};
pub use tick::{Exchange, NormalizedTick, RawTick, TickNormalizer};
pub use ws::{ConnectionConfig, ReconnectPolicy, WsManager};
