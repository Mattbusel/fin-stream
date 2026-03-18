// SPDX-License-Identifier: MIT
//! # fin-stream
//!
//! Lock-free streaming primitives for real-time financial market data.
//!
//! ## Architecture
//!
//! ```text
//! Tick Source
//!     |
//!     v
//! SPSC Ring Buffer  (lock-free, zero-allocation hot path)
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
//! on the fast path. All error paths return `Result<_, StreamError>` -- the
//! library never panics in release builds.
//!
//! ## Modules
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! [`book`] | Order book delta streaming and crossed-book detection |
//! [`error`] | Typed error hierarchy (`StreamError`) |
//! [`health`] | Feed staleness detection and circuit-breaker |
//! [`lorentz`] | Lorentz spacetime transforms for time-series features |
//! [`norm`] | Rolling min-max coordinate normalization |
//! [`ohlcv`] | OHLCV bar aggregation at arbitrary timeframes |
//! [`ring`] | SPSC lock-free ring buffer |
//! [`session`] | Market session and trading-hours classification |
//! [`tick`] | Raw-to-normalized tick conversion for all exchanges |
//! [`ws`] | WebSocket connection management and reconnect policy |

#[path = "book/mod.rs"]
pub mod book;
pub mod error;
#[path = "health/mod.rs"]
pub mod health;
#[path = "lorentz/mod.rs"]
pub mod lorentz;
#[path = "norm/mod.rs"]
pub mod norm;
#[path = "ohlcv/mod.rs"]
pub mod ohlcv;
#[path = "ring/mod.rs"]
pub mod ring;
#[path = "session/mod.rs"]
pub mod session;
#[path = "tick/mod.rs"]
pub mod tick;
#[path = "ws/mod.rs"]
pub mod ws;

pub use book::{BookDelta, BookSide, OrderBook, PriceLevel};
pub use error::StreamError;
pub use health::{FeedHealth, HealthMonitor, HealthStatus};
pub use lorentz::{LorentzTransform, SpacetimePoint};
pub use norm::MinMaxNormalizer;
pub use ohlcv::{OhlcvAggregator, OhlcvBar, Timeframe};
pub use ring::{SpscConsumer, SpscProducer, SpscRing};
pub use session::{MarketSession, SessionAwareness, TradingStatus};
pub use tick::{Exchange, NormalizedTick, RawTick, TickNormalizer};
pub use ws::{ConnectionConfig, ReconnectPolicy, WsManager};
