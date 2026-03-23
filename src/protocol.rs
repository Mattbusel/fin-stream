//! Unified Streaming Protocol for financial data feeds.
//!
//! Provides a common message format and stream adapter that normalizes
//! feeds from different sources (WebSocket, REST polling, file replay)
//! into a single async `Stream<Item = MarketEvent>`.
//!
//! ## Design
//!
//! All event types carry nanosecond timestamps and a string exchange label so
//! that consumers can route, filter, and correlate events without inspecting
//! raw bytes.  The [`JsonStreamAdapter`] parses a heuristic set of JSON shapes
//! emitted by common venues and maps them to the appropriate [`MarketEvent`]
//! variant; unrecognised payloads are silently dropped (returning `None` from
//! [`JsonStreamAdapter::parse_event`]).

use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

// ─── Event types ─────────────────────────────────────────────────────────────

/// Normalised market event produced by any feed source.
///
/// All variants carry nanosecond-precision timestamps and a string exchange
/// label, enabling downstream consumers to sort, correlate, and route events
/// from heterogeneous sources through a single channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MarketEvent {
    /// A single trade execution.
    Trade(TradeEvent),
    /// A best-bid / best-ask quote update.
    Quote(QuoteEvent),
    /// A completed OHLCV bar.
    Bar(BarEvent),
    /// An order-book snapshot or incremental delta.
    OrderBook(OrderBookEvent),
    /// A connection / subscription lifecycle notification.
    Status(StatusEvent),
}

/// A single trade execution reported by an exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeEvent {
    /// Ticker symbol, e.g. `"BTCUSDT"` or `"AAPL"`.
    pub symbol: String,
    /// Execution price.
    pub price: f64,
    /// Executed quantity / size.
    pub size: f64,
    /// Aggressor side, if available from the feed.
    pub side: Option<TradeSide>,
    /// Nanoseconds since Unix epoch.
    pub timestamp_ns: u64,
    /// Exchange-assigned trade identifier, if available.
    pub trade_id: Option<String>,
    /// Exchange or venue that reported this trade.
    pub exchange: String,
}

/// Aggressor side of a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeSide {
    /// Buy-side aggressor (taker bought).
    Buy,
    /// Sell-side aggressor (taker sold).
    Sell,
}

/// Best-bid / best-ask quote update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteEvent {
    /// Ticker symbol.
    pub symbol: String,
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
    /// Quantity available at the best bid.
    pub bid_size: f64,
    /// Quantity available at the best ask.
    pub ask_size: f64,
    /// Nanoseconds since Unix epoch.
    pub timestamp_ns: u64,
    /// Exchange or venue.
    pub exchange: String,
}

/// A completed OHLCV bar at a fixed period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BarEvent {
    /// Ticker symbol.
    pub symbol: String,
    /// Opening price of the bar.
    pub open: f64,
    /// Highest price seen during the bar.
    pub high: f64,
    /// Lowest price seen during the bar.
    pub low: f64,
    /// Closing price of the bar.
    pub close: f64,
    /// Total volume traded during the bar.
    pub volume: f64,
    /// Volume-weighted average price, if available.
    pub vwap: Option<f64>,
    /// Number of individual trades in the bar, if available.
    pub trade_count: Option<u32>,
    /// Bar open time as nanoseconds since Unix epoch.
    pub timestamp_ns: u64,
    /// Bar duration in seconds (e.g. `60` for 1-minute bars).
    pub period_secs: u32,
    /// Exchange or venue.
    pub exchange: String,
}

/// Order-book snapshot or incremental update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookEvent {
    /// Ticker symbol.
    pub symbol: String,
    /// Bid price levels as `(price, size)` pairs, best bid first.
    pub bids: Vec<(f64, f64)>,
    /// Ask price levels as `(price, size)` pairs, best ask first.
    pub asks: Vec<(f64, f64)>,
    /// Exchange-assigned sequence number for ordering deltas.
    pub sequence: u64,
    /// Nanoseconds since Unix epoch.
    pub timestamp_ns: u64,
    /// `true` when this event replaces the full book; `false` for an incremental delta.
    pub is_snapshot: bool,
    /// Exchange or venue.
    pub exchange: String,
}

/// Feed lifecycle notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusEvent {
    /// Symbol this status applies to, or `None` for a connection-level event.
    pub symbol: Option<String>,
    /// Lifecycle status.
    pub status: FeedStatus,
    /// Human-readable description.
    pub message: String,
    /// Nanoseconds since Unix epoch.
    pub timestamp_ns: u64,
}

/// Connection / subscription lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedStatus {
    /// WebSocket / HTTP stream connected and authenticated.
    Connected,
    /// Connection dropped; reconnect may be in progress.
    Disconnected,
    /// Feed encountered a non-recoverable error.
    Error,
    /// Feed is rate-limited or throttled by the venue.
    Throttled,
    /// Subscription to a symbol confirmed.
    Subscribed,
}

// ─── EventStream type alias ───────────────────────────────────────────────────

/// A heap-allocated, `Send`-able async stream of normalised market events.
///
/// Any feed source (WebSocket, REST poll, file replay) can be wrapped with
/// [`futures_util::stream::BoxStream`] and typed as `EventStream` so that
/// downstream components are decoupled from the source implementation.
pub type EventStream = Pin<Box<dyn Stream<Item = MarketEvent> + Send>>;

// ─── JsonStreamAdapter ───────────────────────────────────────────────────────

/// Adapts raw JSON strings from a single feed source into [`MarketEvent`]s.
///
/// The adapter applies a best-effort heuristic parser: it attempts each event
/// shape in order (trade → quote → bar → book → status) and returns the first
/// successful match.  If no shape matches the JSON, [`parse_event`] returns
/// `None` so the caller can skip or log the unknown payload without crashing.
///
/// [`parse_event`]: JsonStreamAdapter::parse_event
pub struct JsonStreamAdapter {
    /// Human-readable source identifier (e.g. a WebSocket URL).
    pub source: String,
    /// Exchange label embedded in every produced event.
    pub exchange: String,
}

impl JsonStreamAdapter {
    /// Creates a new adapter for the given source and exchange labels.
    pub fn new(source: impl Into<String>, exchange: impl Into<String>) -> Self {
        Self { source: source.into(), exchange: exchange.into() }
    }

    /// Attempts to parse `json` into a [`MarketEvent`].
    ///
    /// Returns `None` when the JSON is not recognised as any known event shape,
    /// so that callers can log and skip without panicking or propagating errors.
    ///
    /// ## Supported shapes
    ///
    /// The heuristic checks a small set of discriminator fields:
    ///
    /// | Shape | Required fields |
    /// |-------|----------------|
    /// | Trade | `"type": "trade"` **or** both `"price"` and `"size"` present |
    /// | Quote | `"type": "quote"` **or** both `"bid"` and `"ask"` present |
    /// | Bar   | `"type": "bar"` **or** all of `"open"`, `"high"`, `"low"`, `"close"` present |
    /// | Book  | `"type": "book"` **or** both `"bids"` and `"asks"` arrays present |
    /// | Status | `"type": "status"` **or** `"status"` field present |
    pub fn parse_event(&self, json: &str) -> Option<MarketEvent> {
        let v: serde_json::Value = serde_json::from_str(json).ok()?;

        // Determine the message type via an explicit discriminator or heuristics.
        let type_hint = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        if type_hint == "trade"
            || (type_hint.is_empty() && v.get("price").is_some() && v.get("size").is_some())
        {
            return self.parse_trade(&v);
        }

        if type_hint == "quote"
            || (type_hint.is_empty() && v.get("bid").is_some() && v.get("ask").is_some())
        {
            return self.parse_quote(&v);
        }

        if type_hint == "bar"
            || (type_hint.is_empty()
                && v.get("open").is_some()
                && v.get("high").is_some()
                && v.get("low").is_some()
                && v.get("close").is_some())
        {
            return self.parse_bar(&v);
        }

        if type_hint == "book"
            || (type_hint.is_empty() && v.get("bids").is_some() && v.get("asks").is_some())
        {
            return self.parse_book(&v);
        }

        if type_hint == "status"
            || (type_hint.is_empty() && v.get("status").is_some())
        {
            return self.parse_status(&v);
        }

        None
    }

    // ── private parsers ──────────────────────────────────────────────────────

    fn parse_trade(&self, v: &serde_json::Value) -> Option<MarketEvent> {
        let symbol = extract_symbol(v)?;
        let price = extract_f64(v, &["price", "p", "last"])?;
        let size = extract_f64(v, &["size", "qty", "q", "quantity", "amount"])?;
        let timestamp_ns = extract_timestamp_ns(v);

        let side = v
            .get("side")
            .or_else(|| v.get("taker_side"))
            .and_then(|s| s.as_str())
            .and_then(|s| match s.to_lowercase().as_str() {
                "buy" | "b" | "bid" => Some(TradeSide::Buy),
                "sell" | "s" | "ask" => Some(TradeSide::Sell),
                _ => None,
            });

        let trade_id = v
            .get("trade_id")
            .or_else(|| v.get("id"))
            .or_else(|| v.get("t"))
            .and_then(|id| {
                if let Some(s) = id.as_str() {
                    Some(s.to_owned())
                } else {
                    id.as_u64().map(|n| n.to_string())
                }
            });

        Some(MarketEvent::Trade(TradeEvent {
            symbol,
            price,
            size,
            side,
            timestamp_ns,
            trade_id,
            exchange: self.exchange.clone(),
        }))
    }

    fn parse_quote(&self, v: &serde_json::Value) -> Option<MarketEvent> {
        let symbol = extract_symbol(v)?;
        let bid = extract_f64(v, &["bid", "bid_price", "bp"])?;
        let ask = extract_f64(v, &["ask", "ask_price", "ap"])?;
        let bid_size = extract_f64(v, &["bid_size", "bid_qty", "bs"]).unwrap_or(0.0);
        let ask_size = extract_f64(v, &["ask_size", "ask_qty", "as"]).unwrap_or(0.0);
        let timestamp_ns = extract_timestamp_ns(v);

        Some(MarketEvent::Quote(QuoteEvent {
            symbol,
            bid,
            ask,
            bid_size,
            ask_size,
            timestamp_ns,
            exchange: self.exchange.clone(),
        }))
    }

    fn parse_bar(&self, v: &serde_json::Value) -> Option<MarketEvent> {
        let symbol = extract_symbol(v)?;
        let open = extract_f64(v, &["open", "o"])?;
        let high = extract_f64(v, &["high", "h"])?;
        let low = extract_f64(v, &["low", "l"])?;
        let close = extract_f64(v, &["close", "c"])?;
        let volume = extract_f64(v, &["volume", "vol", "v"]).unwrap_or(0.0);
        let vwap = extract_f64(v, &["vwap", "vw"]);
        let trade_count = v
            .get("trade_count")
            .or_else(|| v.get("n"))
            .and_then(|n| n.as_u64())
            .map(|n| n as u32);
        let period_secs = v
            .get("period_secs")
            .or_else(|| v.get("period"))
            .and_then(|p| p.as_u64())
            .map(|p| p as u32)
            .unwrap_or(60);
        let timestamp_ns = extract_timestamp_ns(v);

        Some(MarketEvent::Bar(BarEvent {
            symbol,
            open,
            high,
            low,
            close,
            volume,
            vwap,
            trade_count,
            timestamp_ns,
            period_secs,
            exchange: self.exchange.clone(),
        }))
    }

    fn parse_book(&self, v: &serde_json::Value) -> Option<MarketEvent> {
        let symbol = extract_symbol(v)?;
        let bids = parse_price_levels(v.get("bids")?);
        let asks = parse_price_levels(v.get("asks")?);
        let sequence = v
            .get("sequence")
            .or_else(|| v.get("seq"))
            .or_else(|| v.get("u"))
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let is_snapshot = v
            .get("is_snapshot")
            .or_else(|| v.get("snapshot"))
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        let timestamp_ns = extract_timestamp_ns(v);

        Some(MarketEvent::OrderBook(OrderBookEvent {
            symbol,
            bids,
            asks,
            sequence,
            timestamp_ns,
            is_snapshot,
            exchange: self.exchange.clone(),
        }))
    }

    fn parse_status(&self, v: &serde_json::Value) -> Option<MarketEvent> {
        let symbol = v
            .get("symbol")
            .or_else(|| v.get("sym"))
            .or_else(|| v.get("s"))
            .and_then(|s| s.as_str())
            .map(str::to_owned);

        let status_str = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("connected");

        let status = match status_str.to_lowercase().as_str() {
            "connected" | "connect" => FeedStatus::Connected,
            "disconnected" | "disconnect" => FeedStatus::Disconnected,
            "error" => FeedStatus::Error,
            "throttled" | "rate_limited" => FeedStatus::Throttled,
            "subscribed" | "subscribe" => FeedStatus::Subscribed,
            _ => FeedStatus::Connected,
        };

        let message = v
            .get("message")
            .or_else(|| v.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or(status_str)
            .to_owned();

        let timestamp_ns = extract_timestamp_ns(v);

        Some(MarketEvent::Status(StatusEvent { symbol, status, message, timestamp_ns }))
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────────

/// Extracts a symbol string, trying several common field names.
fn extract_symbol(v: &serde_json::Value) -> Option<String> {
    v.get("symbol")
        .or_else(|| v.get("sym"))
        .or_else(|| v.get("s"))
        .or_else(|| v.get("product_id"))
        .or_else(|| v.get("ticker"))
        .and_then(|s| s.as_str())
        .map(str::to_owned)
}

/// Extracts an `f64` from the first matching field name, trying string → number coercion.
fn extract_f64(v: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(field) = v.get(key) {
            if let Some(n) = field.as_f64() {
                return Some(n);
            }
            // Some feeds encode numbers as strings
            if let Some(s) = field.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extracts a nanosecond timestamp, falling back to millisecond or second epochs.
fn extract_timestamp_ns(v: &serde_json::Value) -> u64 {
    // Prefer explicit nanosecond fields
    if let Some(ns) = v.get("timestamp_ns").or_else(|| v.get("ts_ns")).and_then(|t| t.as_u64()) {
        return ns;
    }
    // Millisecond epoch
    if let Some(ms) = v
        .get("timestamp")
        .or_else(|| v.get("ts"))
        .or_else(|| v.get("time"))
        .or_else(|| v.get("t"))
        .and_then(|t| t.as_u64())
    {
        // Heuristic: values > 1e15 are nanoseconds, > 1e12 are microseconds, otherwise ms
        if ms > 1_000_000_000_000_000 {
            return ms; // already ns
        }
        if ms > 1_000_000_000_000 {
            return ms * 1_000; // microseconds → ns
        }
        return ms * 1_000_000; // milliseconds → ns
    }
    // Fallback: no timestamp in payload
    0
}

/// Parses a JSON array of price-level tuples in the forms:
/// - `[[price, size], …]`
/// - `[{"price": …, "size": …}, …]`
fn parse_price_levels(arr: &serde_json::Value) -> Vec<(f64, f64)> {
    let Some(arr) = arr.as_array() else { return Vec::new() };
    let mut levels = Vec::with_capacity(arr.len());
    for entry in arr {
        if let Some(tuple) = entry.as_array() {
            // [price, size] form
            if tuple.len() >= 2 {
                let price = tuple[0]
                    .as_f64()
                    .or_else(|| tuple[0].as_str().and_then(|s| s.parse().ok()));
                let size = tuple[1]
                    .as_f64()
                    .or_else(|| tuple[1].as_str().and_then(|s| s.parse().ok()));
                if let (Some(p), Some(s)) = (price, size) {
                    levels.push((p, s));
                }
            }
        } else if entry.is_object() {
            // {"price": …, "size": …} form
            let price = extract_f64(entry, &["price", "p"]);
            let size = extract_f64(entry, &["size", "qty", "q"]);
            if let (Some(p), Some(s)) = (price, size) {
                levels.push((p, s));
            }
        }
    }
    levels
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> JsonStreamAdapter {
        JsonStreamAdapter::new("wss://test.example.com", "TestExchange")
    }

    #[test]
    fn parse_trade_explicit_type() {
        let json = r#"{"type":"trade","symbol":"BTCUSDT","price":"30000.5","size":"0.1","side":"buy","t":1700000000000}"#;
        let ev = adapter().parse_event(json).expect("should parse trade");
        let MarketEvent::Trade(t) = ev else { panic!("expected Trade") };
        assert_eq!(t.symbol, "BTCUSDT");
        assert!((t.price - 30000.5).abs() < 1e-9);
        assert_eq!(t.side, Some(TradeSide::Buy));
    }

    #[test]
    fn parse_trade_heuristic() {
        let json = r#"{"symbol":"ETHUSDT","price":1800.0,"size":2.5}"#;
        let ev = adapter().parse_event(json).expect("should parse trade heuristic");
        assert!(matches!(ev, MarketEvent::Trade(_)));
    }

    #[test]
    fn parse_quote_explicit_type() {
        let json = r#"{"type":"quote","symbol":"AAPL","bid":175.10,"ask":175.12,"bid_size":100,"ask_size":200,"ts":1700000000000}"#;
        let ev = adapter().parse_event(json).expect("should parse quote");
        let MarketEvent::Quote(q) = ev else { panic!("expected Quote") };
        assert!((q.bid - 175.10).abs() < 1e-9);
        assert!((q.ask - 175.12).abs() < 1e-9);
    }

    #[test]
    fn parse_bar_explicit_type() {
        let json = r#"{"type":"bar","symbol":"SPY","open":440.0,"high":445.0,"low":438.0,"close":443.0,"volume":1000000,"period_secs":60}"#;
        let ev = adapter().parse_event(json).expect("should parse bar");
        let MarketEvent::Bar(b) = ev else { panic!("expected Bar") };
        assert_eq!(b.period_secs, 60);
        assert!((b.close - 443.0).abs() < 1e-9);
    }

    #[test]
    fn parse_orderbook_explicit_type() {
        let json = r#"{"type":"book","symbol":"BTCUSDT","bids":[[30000.0,1.5],[29999.0,2.0]],"asks":[[30001.0,1.0]],"sequence":12345,"is_snapshot":true}"#;
        let ev = adapter().parse_event(json).expect("should parse book");
        let MarketEvent::OrderBook(ob) = ev else { panic!("expected OrderBook") };
        assert_eq!(ob.bids.len(), 2);
        assert!(ob.is_snapshot);
        assert_eq!(ob.sequence, 12345);
    }

    #[test]
    fn parse_status_event() {
        let json = r#"{"type":"status","status":"subscribed","symbol":"BTCUSDT","message":"subscription confirmed"}"#;
        let ev = adapter().parse_event(json).expect("should parse status");
        let MarketEvent::Status(s) = ev else { panic!("expected Status") };
        assert_eq!(s.status, FeedStatus::Subscribed);
        assert_eq!(s.symbol.as_deref(), Some("BTCUSDT"));
    }

    #[test]
    fn unknown_json_returns_none() {
        let json = r#"{"foo":"bar","baz":42}"#;
        assert!(adapter().parse_event(json).is_none());
    }

    #[test]
    fn invalid_json_returns_none() {
        assert!(adapter().parse_event("not json at all {{{").is_none());
    }

    #[test]
    fn timestamp_ns_passthrough() {
        let json = r#"{"type":"trade","symbol":"X","price":1.0,"size":1.0,"timestamp_ns":1700000000000000000}"#;
        let ev = adapter().parse_event(json).expect("trade");
        let MarketEvent::Trade(t) = ev else { panic!() };
        assert_eq!(t.timestamp_ns, 1_700_000_000_000_000_000);
    }

    #[test]
    fn event_stream_type_alias_compiles() {
        // Verify EventStream can hold a stream of MarketEvents
        let items = vec![MarketEvent::Status(StatusEvent {
            symbol: None,
            status: FeedStatus::Connected,
            message: "ok".to_owned(),
            timestamp_ns: 0,
        })];
        let stream: EventStream =
            Box::pin(futures_util::stream::iter(items));
        drop(stream);
    }
}
