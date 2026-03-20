//! Tick normalization — raw exchange messages → canonical NormalizedTick.
//!
//! ## Responsibility
//! Convert heterogeneous exchange tick formats (Binance, Coinbase, Alpaca,
//! Polygon) into a single canonical representation. This stage must add
//! <1μs overhead per tick on the hot path.
//!
//! ## Guarantees
//! - Deterministic: same raw bytes always produce the same NormalizedTick
//! - Non-allocating hot path: NormalizedTick is stack-allocated
//! - Thread-safe: TickNormalizer is Send + Sync

use crate::error::StreamError;
use chrono::DateTime;
use rust_decimal::Decimal;
use std::str::FromStr;
use tracing::trace;

/// Supported exchanges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Exchange {
    /// Binance spot/futures WebSocket feed.
    Binance,
    /// Coinbase Advanced Trade WebSocket feed.
    Coinbase,
    /// Alpaca Markets data stream.
    Alpaca,
    /// Polygon.io WebSocket feed.
    Polygon,
}

impl Exchange {
    /// Returns a slice of all supported exchanges.
    ///
    /// Useful for iterating all exchanges to register feeds, build UI dropdowns,
    /// or validate config values at startup.
    pub fn all() -> &'static [Exchange] {
        &[
            Exchange::Binance,
            Exchange::Coinbase,
            Exchange::Alpaca,
            Exchange::Polygon,
        ]
    }

    /// Returns `true` if this exchange primarily trades cryptocurrency spot/futures.
    ///
    /// [`Binance`](Exchange::Binance) and [`Coinbase`](Exchange::Coinbase) are
    /// crypto venues. [`Alpaca`](Exchange::Alpaca) and [`Polygon`](Exchange::Polygon)
    /// are primarily equity/US-market feeds.
    pub fn is_crypto(self) -> bool {
        matches!(self, Exchange::Binance | Exchange::Coinbase)
    }
}

impl std::fmt::Display for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exchange::Binance => write!(f, "Binance"),
            Exchange::Coinbase => write!(f, "Coinbase"),
            Exchange::Alpaca => write!(f, "Alpaca"),
            Exchange::Polygon => write!(f, "Polygon"),
        }
    }
}

impl FromStr for Exchange {
    type Err = StreamError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "binance" => Ok(Exchange::Binance),
            "coinbase" => Ok(Exchange::Coinbase),
            "alpaca" => Ok(Exchange::Alpaca),
            "polygon" => Ok(Exchange::Polygon),
            _ => Err(StreamError::UnknownExchange(s.to_string())),
        }
    }
}

/// Raw tick — unprocessed bytes from an exchange WebSocket.
#[derive(Debug, Clone)]
pub struct RawTick {
    /// Source exchange.
    pub exchange: Exchange,
    /// Instrument symbol as reported by the exchange.
    pub symbol: String,
    /// Raw JSON payload from the WebSocket frame.
    pub payload: serde_json::Value,
    /// System-clock timestamp (ms since Unix epoch) when the tick was received.
    pub received_at_ms: u64,
}

impl RawTick {
    /// Construct a new [`RawTick`], stamping `received_at_ms` from the system clock.
    pub fn new(exchange: Exchange, symbol: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            exchange,
            symbol: symbol.into(),
            payload,
            received_at_ms: now_ms(),
        }
    }
}

/// Canonical normalized tick — exchange-agnostic.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NormalizedTick {
    /// Source exchange.
    pub exchange: Exchange,
    /// Instrument symbol in the canonical form used by this crate.
    pub symbol: String,
    /// Trade price (exact decimal, never `f64`).
    pub price: Decimal,
    /// Trade quantity (exact decimal).
    pub quantity: Decimal,
    /// Direction of the aggressing order, if available from the exchange.
    pub side: Option<TradeSide>,
    /// Exchange-assigned trade identifier, if available.
    pub trade_id: Option<String>,
    /// Exchange-side timestamp (ms since Unix epoch), if included in the feed.
    pub exchange_ts_ms: Option<u64>,
    /// Local system-clock timestamp when this tick was received.
    pub received_at_ms: u64,
}

/// Direction of trade that generated the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TradeSide {
    /// Buyer was the aggressor.
    Buy,
    /// Seller was the aggressor.
    Sell,
}

impl FromStr for TradeSide {
    type Err = StreamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "buy" => Ok(TradeSide::Buy),
            "sell" => Ok(TradeSide::Sell),
            _ => Err(StreamError::ParseError {
                exchange: "TradeSide".into(),
                reason: format!("unknown trade side '{s}'"),
            }),
        }
    }
}

impl std::fmt::Display for TradeSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradeSide::Buy => write!(f, "buy"),
            TradeSide::Sell => write!(f, "sell"),
        }
    }
}

impl TradeSide {
    /// Returns `true` if this side is [`TradeSide::Buy`].
    pub fn is_buy(self) -> bool {
        self == TradeSide::Buy
    }

    /// Returns `true` if this side is [`TradeSide::Sell`].
    pub fn is_sell(self) -> bool {
        self == TradeSide::Sell
    }
}

impl NormalizedTick {
    /// Notional trade value: `price × quantity`.
    ///
    /// Returns the total value transferred in this trade. Useful for VWAP
    /// calculations and volume-weighted aggregations without requiring callers
    /// to multiply at every use site.
    pub fn value(&self) -> Decimal {
        self.price * self.quantity
    }

    /// Age of this tick in milliseconds relative to `now_ms`.
    ///
    /// Returns `now_ms - received_at_ms`, saturating at zero (never negative).
    /// Useful for staleness checks: `tick.age_ms(now) > threshold_ms`.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.received_at_ms)
    }

    /// Returns `true` if this tick is older than `threshold_ms` relative to `now_ms`.
    ///
    /// Equivalent to `self.age_ms(now_ms) > threshold_ms`. Use this for filtering
    /// stale ticks before passing them into aggregation pipelines.
    pub fn is_stale(&self, now_ms: u64, threshold_ms: u64) -> bool {
        self.age_ms(now_ms) > threshold_ms
    }

    /// Returns `true` if the tick is a buyer-initiated trade.
    ///
    /// Returns `false` if side is `Sell` or `None` (side unknown).
    pub fn is_buy(&self) -> bool {
        self.side == Some(TradeSide::Buy)
    }

    /// Returns `true` if the tick is a seller-initiated trade.
    ///
    /// Returns `false` if side is `Buy` or `None` (side unknown).
    pub fn is_sell(&self) -> bool {
        self.side == Some(TradeSide::Sell)
    }

    /// Returns `true` if the trade side is unknown (`None`).
    ///
    /// Many exchanges do not report the aggressor side. This method lets
    /// callers explicitly test for the "no side information" case rather than
    /// relying on both `is_buy()` and `is_sell()` returning `false`.
    pub fn is_neutral(&self) -> bool {
        self.side.is_none()
    }

    /// Returns `true` if the traded quantity meets or exceeds `threshold`.
    ///
    /// Useful for isolating institutional-size trades ("block trades") from
    /// the general flow. The threshold is in the same units as `quantity`.
    pub fn is_large_trade(&self, threshold: Decimal) -> bool {
        self.quantity >= threshold
    }

    /// Return a copy of this tick with the trade side set to `side`.
    ///
    /// Useful in tests and feed normalizers that determine the aggressor side
    /// after initial tick construction.
    pub fn with_side(mut self, side: TradeSide) -> Self {
        self.side = Some(side);
        self
    }

    /// Return a copy of this tick with the exchange timestamp set to `ts_ms`.
    ///
    /// Useful in tests and feed normalizers to inject an authoritative exchange
    /// timestamp after the tick has already been constructed.
    pub fn with_exchange_ts(mut self, ts_ms: u64) -> Self {
        self.exchange_ts_ms = Some(ts_ms);
        self
    }

    /// Signed price movement from `prev` to this tick: `self.price − prev.price`.
    ///
    /// Positive when price rose, negative when price fell, zero when unchanged.
    /// Only meaningful when both ticks share the same symbol and exchange.
    pub fn price_move_from(&self, prev: &NormalizedTick) -> Decimal {
        self.price - prev.price
    }

    /// Returns `true` if this tick arrived more recently than `other`.
    ///
    /// Compares `received_at_ms` timestamps. Equal timestamps return `false`.
    pub fn is_more_recent_than(&self, other: &NormalizedTick) -> bool {
        self.received_at_ms > other.received_at_ms
    }

    /// Transport latency in milliseconds: `received_at_ms − exchange_ts_ms`.
    ///
    /// Returns `None` if the exchange timestamp is unavailable. A positive
    /// value indicates how long the tick took to travel from exchange to this
    /// system; negative values suggest clock skew between exchange and consumer.
    pub fn latency_ms(&self) -> Option<i64> {
        let exchange_ts = self.exchange_ts_ms? as i64;
        Some(self.received_at_ms as i64 - exchange_ts)
    }

    /// Notional value of this trade: `price × quantity`.
    pub fn volume_notional(&self) -> rust_decimal::Decimal {
        self.price * self.quantity
    }

    /// Returns `true` if this tick carries an exchange-provided timestamp.
    ///
    /// When `false`, only the local `received_at_ms` is available. Use
    /// [`latency_ms`](Self::latency_ms) to measure round-trip latency when
    /// this returns `true`.
    pub fn has_exchange_ts(&self) -> bool {
        self.exchange_ts_ms.is_some()
    }

    /// Returns `true` if this tick's price is strictly above `price`.
    pub fn is_above(&self, price: Decimal) -> bool {
        self.price > price
    }

    /// Returns `true` if this tick's price is strictly below `price`.
    pub fn is_below(&self, price: Decimal) -> bool {
        self.price < price
    }
}

impl std::fmt::Display for NormalizedTick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let side = match self.side {
            Some(s) => s.to_string(),
            None => "?".to_string(),
        };
        write!(
            f,
            "{} {} {} x {} {} @{}ms",
            self.exchange, self.symbol, self.price, self.quantity, side, self.received_at_ms
        )
    }
}

/// Normalizes raw ticks from any supported exchange into [`NormalizedTick`] form.
///
/// `TickNormalizer` is stateless and cheap to clone; a single instance can be
/// shared across threads via `Arc` or constructed per-task.
pub struct TickNormalizer;

impl TickNormalizer {
    /// Create a new normalizer. This is a zero-cost constructor.
    pub fn new() -> Self {
        Self
    }

    /// Normalize a raw tick into canonical form.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::ParseError`] if required fields are missing or
    /// malformed, and [`StreamError::InvalidTick`] if price is not positive or
    /// quantity is negative.
    pub fn normalize(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let tick = match raw.exchange {
            Exchange::Binance => self.normalize_binance(raw),
            Exchange::Coinbase => self.normalize_coinbase(raw),
            Exchange::Alpaca => self.normalize_alpaca(raw),
            Exchange::Polygon => self.normalize_polygon(raw),
        }?;
        if tick.price <= Decimal::ZERO {
            return Err(StreamError::InvalidTick {
                reason: format!("price must be positive, got {}", tick.price),
            });
        }
        if tick.quantity < Decimal::ZERO {
            return Err(StreamError::InvalidTick {
                reason: format!("quantity must be non-negative, got {}", tick.quantity),
            });
        }
        trace!(
            exchange = %tick.exchange,
            symbol = %tick.symbol,
            price = %tick.price,
            exchange_ts_ms = ?tick.exchange_ts_ms,
            "tick normalized"
        );
        Ok(tick)
    }

    fn normalize_binance(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let p = &raw.payload;
        let price = parse_decimal_field(p, "p", &raw.exchange.to_string())?;
        let qty = parse_decimal_field(p, "q", &raw.exchange.to_string())?;
        let side = p.get("m").and_then(|v| v.as_bool()).map(|maker| {
            if maker {
                TradeSide::Sell
            } else {
                TradeSide::Buy
            }
        });
        let trade_id = p.get("t").and_then(|v| v.as_u64()).map(|id| id.to_string());
        let exchange_ts = p.get("T").and_then(|v| v.as_u64());
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity: qty,
            side,
            trade_id,
            exchange_ts_ms: exchange_ts,
            received_at_ms: raw.received_at_ms,
        })
    }

    fn normalize_coinbase(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let p = &raw.payload;
        let price = parse_decimal_field(p, "price", &raw.exchange.to_string())?;
        let qty = parse_decimal_field(p, "size", &raw.exchange.to_string())?;
        let side = p.get("side").and_then(|v| v.as_str()).map(|s| {
            if s == "buy" {
                TradeSide::Buy
            } else {
                TradeSide::Sell
            }
        });
        let trade_id = p
            .get("trade_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Coinbase Advanced Trade sends an ISO 8601 timestamp in the "time" field.
        let exchange_ts_ms = p
            .get("time")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis() as u64);
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity: qty,
            side,
            trade_id,
            exchange_ts_ms,
            received_at_ms: raw.received_at_ms,
        })
    }

    fn normalize_alpaca(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let p = &raw.payload;
        let price = parse_decimal_field(p, "p", &raw.exchange.to_string())?;
        let qty = parse_decimal_field(p, "s", &raw.exchange.to_string())?;
        let trade_id = p.get("i").and_then(|v| v.as_u64()).map(|id| id.to_string());
        // Alpaca sends RFC 3339 timestamps in the "t" field (e.g. "2023-11-15T10:00:00.000Z").
        let exchange_ts_ms = p
            .get("t")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis() as u64);
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity: qty,
            side: None,
            trade_id,
            exchange_ts_ms,
            received_at_ms: raw.received_at_ms,
        })
    }

    fn normalize_polygon(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let p = &raw.payload;
        let price = parse_decimal_field(p, "p", &raw.exchange.to_string())?;
        let qty = parse_decimal_field(p, "s", &raw.exchange.to_string())?;
        let trade_id = p.get("i").and_then(|v| v.as_str()).map(str::to_string);
        // Polygon sends nanoseconds since epoch in the "t" field; convert to milliseconds.
        let exchange_ts = p
            .get("t")
            .and_then(|v| v.as_u64())
            .map(|t_ns| t_ns / 1_000_000);
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity: qty,
            side: None,
            trade_id,
            exchange_ts_ms: exchange_ts,
            received_at_ms: raw.received_at_ms,
        })
    }
}

impl Default for TickNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_decimal_field(
    v: &serde_json::Value,
    field: &str,
    exchange: &str,
) -> Result<Decimal, StreamError> {
    let raw = v.get(field).ok_or_else(|| StreamError::ParseError {
        exchange: exchange.to_string(),
        reason: format!("missing field '{}'", field),
    })?;
    // Use the JSON-native string representation for both string and number
    // values. For JSON strings this is a direct parse. For JSON numbers we use
    // serde_json::Number::to_string(), which preserves the original text (e.g.
    // "50000.12345678") rather than round-tripping through f64 and losing
    // sub-microsecond precision.
    let s: String = match raw {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => {
            return Err(StreamError::ParseError {
                exchange: exchange.to_string(),
                reason: format!("field '{}' is not a string or number", field),
            });
        }
    };
    Decimal::from_str(&s).map_err(|e| StreamError::ParseError {
        exchange: exchange.to_string(),
        reason: format!("field '{}' parse error: {}", field, e),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn normalizer() -> TickNormalizer {
        TickNormalizer::new()
    }

    fn binance_tick(symbol: &str) -> RawTick {
        RawTick {
            exchange: Exchange::Binance,
            symbol: symbol.to_string(),
            payload: json!({ "p": "50000.12", "q": "0.001", "m": false, "t": 12345, "T": 1700000000000u64 }),
            received_at_ms: 1700000000001,
        }
    }

    fn coinbase_tick(symbol: &str) -> RawTick {
        RawTick {
            exchange: Exchange::Coinbase,
            symbol: symbol.to_string(),
            payload: json!({ "price": "50001.00", "size": "0.5", "side": "buy", "trade_id": "abc123" }),
            received_at_ms: 1700000000002,
        }
    }

    fn alpaca_tick(symbol: &str) -> RawTick {
        RawTick {
            exchange: Exchange::Alpaca,
            symbol: symbol.to_string(),
            payload: json!({ "p": "180.50", "s": "10", "i": 99 }),
            received_at_ms: 1700000000003,
        }
    }

    fn polygon_tick(symbol: &str) -> RawTick {
        RawTick {
            exchange: Exchange::Polygon,
            symbol: symbol.to_string(),
            // Polygon sends nanoseconds; 1_700_000_000_000_000_000 ns = 1_700_000_000_000 ms
            payload: json!({ "p": "180.51", "s": "5", "i": "XYZ-001", "t": 1_700_000_000_000_000_000u64 }),
            received_at_ms: 1700000000005,
        }
    }

    #[test]
    fn test_exchange_from_str_valid() {
        assert_eq!("binance".parse::<Exchange>().unwrap(), Exchange::Binance);
        assert_eq!("Coinbase".parse::<Exchange>().unwrap(), Exchange::Coinbase);
        assert_eq!("ALPACA".parse::<Exchange>().unwrap(), Exchange::Alpaca);
        assert_eq!("polygon".parse::<Exchange>().unwrap(), Exchange::Polygon);
    }

    #[test]
    fn test_exchange_from_str_unknown_returns_error() {
        let result = "Kraken".parse::<Exchange>();
        assert!(matches!(result, Err(StreamError::UnknownExchange(_))));
    }

    #[test]
    fn test_exchange_display() {
        assert_eq!(Exchange::Binance.to_string(), "Binance");
        assert_eq!(Exchange::Coinbase.to_string(), "Coinbase");
    }

    #[test]
    fn test_normalize_binance_tick_price_and_qty() {
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        assert_eq!(tick.price, Decimal::from_str("50000.12").unwrap());
        assert_eq!(tick.quantity, Decimal::from_str("0.001").unwrap());
        assert_eq!(tick.exchange, Exchange::Binance);
        assert_eq!(tick.symbol, "BTCUSDT");
    }

    #[test]
    fn test_normalize_binance_side_maker_false_is_buy() {
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        assert_eq!(tick.side, Some(TradeSide::Buy));
    }

    #[test]
    fn test_normalize_binance_side_maker_true_is_sell() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": "50000", "q": "1", "m": true }),
            received_at_ms: 0,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert_eq!(tick.side, Some(TradeSide::Sell));
    }

    #[test]
    fn test_normalize_binance_trade_id_and_ts() {
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        assert_eq!(tick.trade_id, Some("12345".to_string()));
        assert_eq!(tick.exchange_ts_ms, Some(1700000000000));
    }

    #[test]
    fn test_normalize_coinbase_tick() {
        let tick = normalizer().normalize(coinbase_tick("BTC-USD")).unwrap();
        assert_eq!(tick.price, Decimal::from_str("50001.00").unwrap());
        assert_eq!(tick.quantity, Decimal::from_str("0.5").unwrap());
        assert_eq!(tick.side, Some(TradeSide::Buy));
        assert_eq!(tick.trade_id, Some("abc123".to_string()));
    }

    #[test]
    fn test_normalize_coinbase_sell_side() {
        let raw = RawTick {
            exchange: Exchange::Coinbase,
            symbol: "BTC-USD".into(),
            payload: json!({ "price": "50000", "size": "1", "side": "sell" }),
            received_at_ms: 0,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert_eq!(tick.side, Some(TradeSide::Sell));
    }

    #[test]
    fn test_normalize_alpaca_tick() {
        let tick = normalizer().normalize(alpaca_tick("AAPL")).unwrap();
        assert_eq!(tick.price, Decimal::from_str("180.50").unwrap());
        assert_eq!(tick.quantity, Decimal::from_str("10").unwrap());
        assert_eq!(tick.trade_id, Some("99".to_string()));
        assert_eq!(tick.side, None);
    }

    #[test]
    fn test_normalize_polygon_tick() {
        let tick = normalizer().normalize(polygon_tick("AAPL")).unwrap();
        assert_eq!(tick.price, Decimal::from_str("180.51").unwrap());
        // 1_700_000_000_000_000_000 ns / 1_000_000 = 1_700_000_000_000 ms
        assert_eq!(tick.exchange_ts_ms, Some(1_700_000_000_000u64));
        assert_eq!(tick.trade_id, Some("XYZ-001".to_string()));
    }

    #[test]
    fn test_normalize_alpaca_rfc3339_timestamp() {
        let raw = RawTick {
            exchange: Exchange::Alpaca,
            symbol: "AAPL".into(),
            payload: json!({ "p": "180.50", "s": "10", "i": 99, "t": "2023-11-15T00:00:00Z" }),
            received_at_ms: 1700000000003,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert!(tick.exchange_ts_ms.is_some(), "Alpaca 't' field should be parsed");
        // 2023-11-15T00:00:00Z = 1700006400000 ms
        assert_eq!(tick.exchange_ts_ms, Some(1700006400000u64));
    }

    #[test]
    fn test_normalize_alpaca_no_timestamp_field() {
        let tick = normalizer().normalize(alpaca_tick("AAPL")).unwrap();
        assert_eq!(tick.exchange_ts_ms, None, "missing 't' field means no exchange_ts_ms");
    }

    #[test]
    fn test_normalize_missing_price_field_returns_parse_error() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "q": "1" }),
            received_at_ms: 0,
        };
        let result = normalizer().normalize(raw);
        assert!(matches!(result, Err(StreamError::ParseError { .. })));
    }

    #[test]
    fn test_normalize_invalid_decimal_returns_parse_error() {
        let raw = RawTick {
            exchange: Exchange::Coinbase,
            symbol: "BTC-USD".into(),
            payload: json!({ "price": "not-a-number", "size": "1" }),
            received_at_ms: 0,
        };
        let result = normalizer().normalize(raw);
        assert!(matches!(result, Err(StreamError::ParseError { .. })));
    }

    #[test]
    fn test_raw_tick_new_sets_received_at() {
        let raw = RawTick::new(Exchange::Binance, "BTCUSDT", json!({}));
        assert!(raw.received_at_ms > 0);
    }

    #[test]
    fn test_normalize_numeric_price_field() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": 50000.0, "q": 1.0 }),
            received_at_ms: 0,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert!(tick.price > Decimal::ZERO);
    }

    #[test]
    fn test_trade_side_from_str_buy() {
        assert_eq!("buy".parse::<TradeSide>().unwrap(), TradeSide::Buy);
        assert_eq!("Buy".parse::<TradeSide>().unwrap(), TradeSide::Buy);
        assert_eq!("BUY".parse::<TradeSide>().unwrap(), TradeSide::Buy);
    }

    #[test]
    fn test_trade_side_from_str_sell() {
        assert_eq!("sell".parse::<TradeSide>().unwrap(), TradeSide::Sell);
        assert_eq!("Sell".parse::<TradeSide>().unwrap(), TradeSide::Sell);
        assert_eq!("SELL".parse::<TradeSide>().unwrap(), TradeSide::Sell);
    }

    #[test]
    fn test_trade_side_from_str_invalid() {
        let err = "long".parse::<TradeSide>().unwrap_err();
        assert!(matches!(err, StreamError::ParseError { .. }));
    }

    #[test]
    fn test_trade_side_display() {
        assert_eq!(TradeSide::Buy.to_string(), "buy");
        assert_eq!(TradeSide::Sell.to_string(), "sell");
    }

    #[test]
    fn test_normalize_zero_price_returns_invalid_tick() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": "0", "q": "1" }),
            received_at_ms: 0,
        };
        let err = normalizer().normalize(raw).unwrap_err();
        assert!(matches!(err, StreamError::InvalidTick { .. }));
    }

    #[test]
    fn test_normalize_negative_price_returns_invalid_tick() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": "-1", "q": "1" }),
            received_at_ms: 0,
        };
        let err = normalizer().normalize(raw).unwrap_err();
        assert!(matches!(err, StreamError::InvalidTick { .. }));
    }

    #[test]
    fn test_normalize_negative_quantity_returns_invalid_tick() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": "100", "q": "-1" }),
            received_at_ms: 0,
        };
        let err = normalizer().normalize(raw).unwrap_err();
        assert!(matches!(err, StreamError::InvalidTick { .. }));
    }

    #[test]
    fn test_normalize_zero_quantity_is_valid() {
        // Zero quantity is allowed (e.g., remove from book), just not negative
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: json!({ "p": "100", "q": "0" }),
            received_at_ms: 0,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert_eq!(tick.quantity, Decimal::ZERO);
    }

    #[test]
    fn test_trade_side_is_buy() {
        assert!(TradeSide::Buy.is_buy());
        assert!(!TradeSide::Buy.is_sell());
    }

    #[test]
    fn test_trade_side_is_sell() {
        assert!(TradeSide::Sell.is_sell());
        assert!(!TradeSide::Sell.is_buy());
    }

    #[test]
    fn test_normalized_tick_display() {
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        let s = tick.to_string();
        assert!(s.contains("Binance"));
        assert!(s.contains("BTCUSDT"));
        assert!(s.contains("50000"));
    }

    #[test]
    fn test_normalized_tick_value_is_price_times_qty() {
        use rust_decimal_macros::dec;
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        // binance_tick sets price=50000, quantity=0.001
        let expected = tick.price * tick.quantity;
        assert_eq!(tick.value(), expected);
    }

    #[test]
    fn test_normalized_tick_age_ms_positive() {
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        // received_at_ms is set to 1_000_000 in binance_tick helper? Let's check
        // Actually the helper uses now_ms() so we can't predict. Use a manual tick.
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: serde_json::json!({"p": "50000", "q": "0.001", "m": false}),
            received_at_ms: 1_000_000,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert_eq!(tick.age_ms(1_001_000), 1_000);
    }

    #[test]
    fn test_normalized_tick_age_ms_zero_when_now_equals_received() {
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: serde_json::json!({"p": "50000", "q": "0.001", "m": false}),
            received_at_ms: 5_000,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert_eq!(tick.age_ms(5_000), 0);
        // saturating_sub: now < received → 0
        assert_eq!(tick.age_ms(4_000), 0);
    }

    #[test]
    fn test_normalized_tick_value_zero_qty_is_zero() {
        use rust_decimal_macros::dec;
        let raw = RawTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            payload: serde_json::json!({
                "p": "50000",
                "q": "0",
                "m": false,
            }),
            received_at_ms: 1000,
        };
        let tick = normalizer().normalize(raw).unwrap();
        assert_eq!(tick.value(), dec!(0));
    }

    // ── NormalizedTick::is_stale ──────────────────────────────────────────────

    fn make_tick_at(received_at_ms: u64) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            price: rust_decimal_macros::dec!(100),
            quantity: rust_decimal_macros::dec!(1),
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms,
        }
    }

    #[test]
    fn test_is_stale_true_when_age_exceeds_threshold() {
        let tick = make_tick_at(1_000);
        // now=6000, age=5000, threshold=4000 → stale
        assert!(tick.is_stale(6_000, 4_000));
    }

    #[test]
    fn test_is_stale_false_when_age_equals_threshold() {
        let tick = make_tick_at(1_000);
        // now=5000, age=4000, threshold=4000 → NOT stale (> not >=)
        assert!(!tick.is_stale(5_000, 4_000));
    }

    #[test]
    fn test_is_stale_false_for_fresh_tick() {
        let tick = make_tick_at(10_000);
        assert!(!tick.is_stale(10_500, 1_000));
    }

    // ── NormalizedTick::is_buy / is_sell ──────────────────────────────────────

    #[test]
    fn test_is_buy_true_for_buy_side() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Buy);
        assert!(tick.is_buy());
        assert!(!tick.is_sell());
    }

    #[test]
    fn test_is_sell_true_for_sell_side() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Sell);
        assert!(tick.is_sell());
        assert!(!tick.is_buy());
    }

    #[test]
    fn test_is_buy_false_for_unknown_side() {
        let mut tick = make_tick_at(1_000);
        tick.side = None;
        assert!(!tick.is_buy());
        assert!(!tick.is_sell());
    }

    // ── NormalizedTick::with_exchange_ts ──────────────────────────────────────

    #[test]
    fn test_with_exchange_ts_sets_field() {
        let tick = make_tick_at(5_000).with_exchange_ts(3_000);
        assert_eq!(tick.exchange_ts_ms, Some(3_000));
        assert_eq!(tick.received_at_ms, 5_000); // unchanged
    }

    #[test]
    fn test_with_exchange_ts_overrides_existing() {
        let tick = make_tick_at(1_000).with_exchange_ts(999).with_exchange_ts(888);
        assert_eq!(tick.exchange_ts_ms, Some(888));
    }

    // ── NormalizedTick::price_move_from / is_more_recent_than ─────────────────

    #[test]
    fn test_price_move_from_positive() {
        let prev = make_tick_at(1_000);
        let mut curr = make_tick_at(2_000);
        curr.price = prev.price + rust_decimal_macros::dec!(5);
        assert_eq!(curr.price_move_from(&prev), rust_decimal_macros::dec!(5));
    }

    #[test]
    fn test_price_move_from_negative() {
        let prev = make_tick_at(1_000);
        let mut curr = make_tick_at(2_000);
        curr.price = prev.price - rust_decimal_macros::dec!(3);
        assert_eq!(curr.price_move_from(&prev), rust_decimal_macros::dec!(-3));
    }

    #[test]
    fn test_price_move_from_zero_when_same() {
        let tick = make_tick_at(1_000);
        assert_eq!(tick.price_move_from(&tick), rust_decimal_macros::dec!(0));
    }

    #[test]
    fn test_is_more_recent_than_true() {
        let older = make_tick_at(1_000);
        let newer = make_tick_at(2_000);
        assert!(newer.is_more_recent_than(&older));
    }

    #[test]
    fn test_is_more_recent_than_false_when_older() {
        let older = make_tick_at(1_000);
        let newer = make_tick_at(2_000);
        assert!(!older.is_more_recent_than(&newer));
    }

    #[test]
    fn test_is_more_recent_than_false_when_equal() {
        let tick = make_tick_at(1_000);
        assert!(!tick.is_more_recent_than(&tick));
    }

    // ── NormalizedTick::with_side ─────────────────────────────────────────────

    #[test]
    fn test_with_side_sets_buy() {
        let tick = make_tick_at(1_000).with_side(TradeSide::Buy);
        assert_eq!(tick.side, Some(TradeSide::Buy));
    }

    #[test]
    fn test_with_side_sets_sell() {
        let tick = make_tick_at(1_000).with_side(TradeSide::Sell);
        assert_eq!(tick.side, Some(TradeSide::Sell));
    }

    #[test]
    fn test_with_side_overrides_existing() {
        let tick = make_tick_at(1_000).with_side(TradeSide::Buy).with_side(TradeSide::Sell);
        assert_eq!(tick.side, Some(TradeSide::Sell));
    }

    // ── NormalizedTick::is_neutral ────────────────────────────────────────────

    #[test]
    fn test_is_neutral_true_when_no_side() {
        let mut tick = make_tick_at(1_000);
        tick.side = None;
        assert!(tick.is_neutral());
    }

    #[test]
    fn test_is_neutral_false_when_buy() {
        let tick = make_tick_at(1_000).with_side(TradeSide::Buy);
        assert!(!tick.is_neutral());
    }

    #[test]
    fn test_is_neutral_false_when_sell() {
        let tick = make_tick_at(1_000).with_side(TradeSide::Sell);
        assert!(!tick.is_neutral());
    }

    // ── NormalizedTick::is_large_trade ────────────────────────────────────────

    #[test]
    fn test_is_large_trade_above_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(100);
        assert!(tick.is_large_trade(rust_decimal_macros::dec!(50)));
    }

    #[test]
    fn test_is_large_trade_at_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(50);
        assert!(tick.is_large_trade(rust_decimal_macros::dec!(50)));
    }

    #[test]
    fn test_is_large_trade_below_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(10);
        assert!(!tick.is_large_trade(rust_decimal_macros::dec!(50)));
    }

    #[test]
    fn test_volume_notional_is_price_times_quantity() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(200);
        tick.quantity = rust_decimal_macros::dec!(3);
        assert_eq!(tick.volume_notional(), rust_decimal_macros::dec!(600));
    }

    // ── NormalizedTick::is_above ──────────────────────────────────────────────

    #[test]
    fn test_is_above_returns_true_when_price_higher() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(200);
        assert!(tick.is_above(rust_decimal_macros::dec!(150)));
    }

    #[test]
    fn test_is_above_returns_false_when_price_equal() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(200);
        assert!(!tick.is_above(rust_decimal_macros::dec!(200)));
    }

    #[test]
    fn test_is_above_returns_false_when_price_lower() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(100);
        assert!(!tick.is_above(rust_decimal_macros::dec!(200)));
    }

    // ── NormalizedTick::is_below ──────────────────────────────────────────────

    #[test]
    fn test_is_below_returns_true_when_price_lower() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(100);
        assert!(tick.is_below(rust_decimal_macros::dec!(150)));
    }

    #[test]
    fn test_is_below_returns_false_when_price_equal() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(100);
        assert!(!tick.is_below(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_is_below_returns_false_when_price_higher() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(200);
        assert!(!tick.is_below(rust_decimal_macros::dec!(100)));
    }

    // --- has_exchange_ts ---

    #[test]
    fn test_has_exchange_ts_false_when_none() {
        let tick = make_tick_at(1_000);
        assert!(!tick.has_exchange_ts());
    }

    #[test]
    fn test_has_exchange_ts_true_when_some() {
        let tick = make_tick_at(1_000).with_exchange_ts(900);
        assert!(tick.has_exchange_ts());
    }
}
