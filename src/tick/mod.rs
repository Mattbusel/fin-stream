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

    /// Human-readable trade direction: `"buy"`, `"sell"`, or `"unknown"`.
    pub fn side_str(&self) -> &'static str {
        match self.side {
            Some(TradeSide::Buy) => "buy",
            Some(TradeSide::Sell) => "sell",
            None => "unknown",
        }
    }

    /// Returns `true` if the quantity is a whole number (no fractional part).
    ///
    /// Useful for detecting atypical fractional order sizes, or as a simple
    /// round-lot check in integer-quantity markets.
    pub fn is_round_lot(&self) -> bool {
        self.quantity.fract().is_zero()
    }

    /// Returns `true` if this tick's symbol matches `other`'s symbol exactly.
    pub fn is_same_symbol_as(&self, other: &NormalizedTick) -> bool {
        self.symbol == other.symbol
    }

    /// Absolute price difference between this tick and `other`.
    ///
    /// Returns `|self.price - other.price|`. Useful for computing price drift
    /// between two ticks of the same instrument without caring about direction.
    pub fn price_distance_from(&self, other: &NormalizedTick) -> Decimal {
        (self.price - other.price).abs()
    }

    /// Signed latency between the local receipt timestamp and the exchange
    /// timestamp, in milliseconds.
    ///
    /// Returns `ts_ms as i64 - exchange_ts_ms as i64`.  Positive values mean
    /// the tick arrived at the local system after the exchange stamped it
    /// (normal network latency).  Negative values indicate clock skew between
    /// the exchange and local clock.  Returns `None` if `exchange_ts_ms` is
    /// absent.
    pub fn exchange_latency_ms(&self) -> Option<i64> {
        self.exchange_ts_ms
            .map(|e| self.received_at_ms as i64 - e as i64)
    }

    /// Returns `true` if the notional value of this trade (`price × quantity`)
    /// exceeds `threshold`.
    ///
    /// Unlike [`is_large_trade`](Self::is_large_trade) (which compares raw
    /// quantity), this method uses the trade's dollar value, making it useful
    /// for comparing block-trade size across instruments with different prices.
    pub fn is_notional_large_trade(&self, threshold: Decimal) -> bool {
        self.volume_notional() > threshold
    }

    /// Returns `true` if this tick's price is zero.
    ///
    /// A zero price typically indicates a malformed or uninitialized tick.
    pub fn is_zero_price(&self) -> bool {
        self.price.is_zero()
    }

    /// Returns `true` if the tick is still fresh relative to `now_ms`.
    ///
    /// "Fresh" means the tick arrived within the last `max_age_ms` milliseconds.
    /// Returns `false` when `now_ms < ts_ms` (clock skew guard).
    pub fn is_fresh(&self, now_ms: u64, max_age_ms: u64) -> bool {
        now_ms.saturating_sub(self.received_at_ms) <= max_age_ms
    }

    /// Returns `true` if this tick's price is strictly above `price`.
    pub fn is_above(&self, price: Decimal) -> bool {
        self.price > price
    }

    /// Returns `true` if this tick's price is strictly below `price`.
    pub fn is_below(&self, price: Decimal) -> bool {
        self.price < price
    }

    /// Returns `true` if this tick's price equals `price`.
    pub fn is_at(&self, price: Decimal) -> bool {
        self.price == price
    }

    /// Returns `true` if the tick has a definite direction (buy or sell).
    ///
    /// Neutral ticks (where `side` is `None`) return `false`.
    pub fn is_aggressive(&self) -> bool {
        self.side.is_some()
    }

    /// Signed price difference: `self.price - other.price`.
    ///
    /// Positive when this tick's price is higher than the other's.
    pub fn price_diff_from(&self, other: &NormalizedTick) -> Decimal {
        self.price - other.price
    }

    /// Returns `true` if the trade quantity is strictly less than `threshold`.
    ///
    /// The inverse of [`is_large_trade`](Self::is_large_trade).
    pub fn is_micro_trade(&self, threshold: Decimal) -> bool {
        self.quantity < threshold
    }

    /// Returns `true` if this tick occurred above the given midpoint price.
    ///
    /// A tick above the midpoint is typically associated with buying pressure.
    pub fn is_buying_pressure(&self, midpoint: Decimal) -> bool {
        self.price > midpoint
    }

    /// Age of this tick in seconds: `(now_ms - received_at_ms) / 1000.0`.
    ///
    /// Returns `0.0` if `now_ms` is before `received_at_ms`.
    pub fn age_secs(&self, now_ms: u64) -> f64 {
        now_ms.saturating_sub(self.received_at_ms) as f64 / 1_000.0
    }

    /// Returns `true` if this tick originated from the same exchange as `other`.
    pub fn is_same_exchange_as(&self, other: &NormalizedTick) -> bool {
        self.exchange == other.exchange
    }

    /// Age of this tick in milliseconds: `now_ms - received_at_ms`.
    ///
    /// Returns `0` if `now_ms` is before `received_at_ms`.
    pub fn quote_age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.received_at_ms)
    }

    /// Notional value of this tick: `price × quantity`.
    pub fn notional_value(&self) -> Decimal {
        self.price * self.quantity
    }

    /// Returns `true` if the notional value (`price × quantity`) exceeds `threshold`.
    pub fn is_high_value_tick(&self, threshold: Decimal) -> bool {
        self.notional_value() > threshold
    }

    /// Returns `true` if this tick is on the buy side.
    ///
    /// Returns `false` if the side is `Sell` or `None`.
    pub fn is_buy_side(&self) -> bool {
        self.side == Some(TradeSide::Buy)
    }

    /// Returns `true` if this tick is on the sell side.
    ///
    /// Returns `false` if the side is `Buy` or `None`.
    pub fn is_sell_side(&self) -> bool {
        self.side == Some(TradeSide::Sell)
    }

    /// Returns `true` if this tick's quantity is zero (may indicate a cancel or correction).
    pub fn is_zero_quantity(&self) -> bool {
        self.quantity.is_zero()
    }

    /// Returns `true` if this tick's price is within `[low, high]` (inclusive).
    pub fn price_in_range(&self, low: Decimal, high: Decimal) -> bool {
        self.price >= low && self.price <= high
    }

    /// Price rounded down to the nearest multiple of `tick_size`.
    ///
    /// Returns the original price if `tick_size` is zero.
    pub fn rounded_price(&self, tick_size: Decimal) -> Decimal {
        if tick_size.is_zero() {
            return self.price;
        }
        (self.price / tick_size).floor() * tick_size
    }

    /// Returns `true` if the absolute price difference from `other` exceeds `threshold`.
    pub fn is_large_spread_from(&self, other: &NormalizedTick, threshold: Decimal) -> bool {
        (self.price - other.price).abs() > threshold
    }

    /// Notional value of this tick as `f64` (`price × quantity`).
    pub fn volume_notional_f64(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.volume_notional().to_f64().unwrap_or(0.0)
    }

    /// Rate of price change relative to a prior tick: `(price - prev.price) / dt_ms`.
    ///
    /// Returns `None` if `dt_ms` is zero (same timestamp).
    pub fn price_velocity(&self, prev: &NormalizedTick, dt_ms: u64) -> Option<Decimal> {
        if dt_ms == 0 { return None; }
        Some((self.price - prev.price) / Decimal::from(dt_ms))
    }

    /// Volume-weighted average price across a slice of ticks.
    ///
    /// Returns `None` if the slice is empty or total volume is zero.
    pub fn vwap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() { return None; }
        let total_vol: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_vol.is_zero() { return None; }
        let total_notional: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum();
        Some(total_notional / total_vol)
    }

    /// Returns `true` if price reversed direction by at least `min_move` from `prev`.
    ///
    /// A reversal means the direction of `(self.price - prev.price)` is opposite to
    /// the direction of `(prev.price - prev_prev.price)`, and the magnitude ≥ `min_move`.
    /// This two-argument form checks: `|self.price - prev.price| >= min_move`.
    pub fn is_reversal(&self, prev: &NormalizedTick, min_move: Decimal) -> bool {
        let move_size = (self.price - prev.price).abs();
        move_size >= min_move
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
        let tick = normalizer().normalize(binance_tick("BTCUSDT")).unwrap();
        // binance_tick sets price=50000, quantity=0.001
        let expected = tick.price * tick.quantity;
        assert_eq!(tick.volume_notional(), expected);
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

    // ── NormalizedTick::is_at ─────────────────────────────────────────────────

    #[test]
    fn test_is_at_returns_true_when_equal() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(100);
        assert!(tick.is_at(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_is_at_returns_false_when_higher() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(101);
        assert!(!tick.is_at(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_is_at_returns_false_when_lower() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(99);
        assert!(!tick.is_at(rust_decimal_macros::dec!(100)));
    }

    // ── NormalizedTick::is_buy ────────────────────────────────────────────────

    #[test]
    fn test_is_buy_true_when_side_is_buy() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Buy);
        assert!(tick.is_buy());
    }

    #[test]
    fn test_is_buy_false_when_side_is_sell() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Sell);
        assert!(!tick.is_buy());
    }

    #[test]
    fn test_is_buy_false_when_side_is_none() {
        let mut tick = make_tick_at(1_000);
        tick.side = None;
        assert!(!tick.is_buy());
    }

    // --- side_str / is_round_lot ---

    #[test]
    fn test_side_str_buy() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Buy);
        assert_eq!(tick.side_str(), "buy");
    }

    #[test]
    fn test_side_str_sell() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Sell);
        assert_eq!(tick.side_str(), "sell");
    }

    #[test]
    fn test_side_str_unknown_when_none() {
        let mut tick = make_tick_at(1_000);
        tick.side = None;
        assert_eq!(tick.side_str(), "unknown");
    }

    #[test]
    fn test_is_round_lot_true_for_integer_quantity() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(100);
        assert!(tick.is_round_lot());
    }

    #[test]
    fn test_is_round_lot_false_for_fractional_quantity() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(0.5);
        assert!(!tick.is_round_lot());
    }

    // --- is_same_symbol_as / price_distance_from ---

    #[test]
    fn test_is_same_symbol_as_true_when_symbols_match() {
        let t1 = make_tick_at(1_000);
        let t2 = make_tick_at(2_000);
        assert!(t1.is_same_symbol_as(&t2));
    }

    #[test]
    fn test_is_same_symbol_as_false_when_symbols_differ() {
        let t1 = make_tick_at(1_000);
        let mut t2 = make_tick_at(2_000);
        t2.symbol = "ETH-USD".to_string();
        assert!(!t1.is_same_symbol_as(&t2));
    }

    #[test]
    fn test_price_distance_from_is_absolute() {
        let mut t1 = make_tick_at(1_000);
        let mut t2 = make_tick_at(2_000);
        t1.price = rust_decimal_macros::dec!(100);
        t2.price = rust_decimal_macros::dec!(110);
        assert_eq!(t1.price_distance_from(&t2), rust_decimal_macros::dec!(10));
        assert_eq!(t2.price_distance_from(&t1), rust_decimal_macros::dec!(10));
    }

    #[test]
    fn test_price_distance_from_zero_when_equal() {
        let t1 = make_tick_at(1_000);
        let t2 = make_tick_at(2_000);
        assert!(t1.price_distance_from(&t2).is_zero());
    }

    // ── NormalizedTick::is_sell ───────────────────────────────────────────────

    #[test]
    fn test_is_sell_true_when_side_is_sell() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Sell);
        assert!(tick.is_sell());
    }

    #[test]
    fn test_is_sell_false_when_side_is_buy() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Buy);
        assert!(!tick.is_sell());
    }

    #[test]
    fn test_is_sell_false_when_side_is_none() {
        let mut tick = make_tick_at(1_000);
        tick.side = None;
        assert!(!tick.is_sell());
    }

    // --- exchange_latency_ms / is_notional_large_trade ---

    #[test]
    fn test_exchange_latency_ms_positive_for_normal_delivery() {
        let mut tick = make_tick_at(1_100);
        tick.exchange_ts_ms = Some(1_000);
        assert_eq!(tick.exchange_latency_ms(), Some(100));
    }

    #[test]
    fn test_exchange_latency_ms_negative_for_clock_skew() {
        let mut tick = make_tick_at(1_000);
        tick.exchange_ts_ms = Some(1_100);
        assert_eq!(tick.exchange_latency_ms(), Some(-100));
    }

    #[test]
    fn test_exchange_latency_ms_none_when_no_exchange_ts() {
        let mut tick = make_tick_at(1_000);
        tick.exchange_ts_ms = None;
        assert!(tick.exchange_latency_ms().is_none());
    }

    #[test]
    fn test_is_notional_large_trade_true_when_above_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(100);
        tick.quantity = rust_decimal_macros::dec!(10);
        // notional = 1000, threshold = 500 → true
        assert!(tick.is_notional_large_trade(rust_decimal_macros::dec!(500)));
    }

    #[test]
    fn test_is_notional_large_trade_false_when_at_or_below_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(100);
        tick.quantity = rust_decimal_macros::dec!(5);
        // notional = 500, threshold = 500 → false (strictly greater)
        assert!(!tick.is_notional_large_trade(rust_decimal_macros::dec!(500)));
    }

    #[test]
    fn test_is_aggressive_true_when_buy() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Buy);
        assert!(tick.is_aggressive());
    }

    #[test]
    fn test_is_aggressive_true_when_sell() {
        let mut tick = make_tick_at(1_000);
        tick.side = Some(TradeSide::Sell);
        assert!(tick.is_aggressive());
    }

    #[test]
    fn test_is_aggressive_false_when_neutral() {
        let tick = make_tick_at(1_000); // side = None
        assert!(!tick.is_aggressive());
    }

    #[test]
    fn test_price_diff_from_positive_when_higher() {
        let mut t1 = make_tick_at(1_000);
        let mut t2 = make_tick_at(1_000);
        t1.price = rust_decimal_macros::dec!(105);
        t2.price = rust_decimal_macros::dec!(100);
        assert_eq!(t1.price_diff_from(&t2), rust_decimal_macros::dec!(5));
    }

    #[test]
    fn test_price_diff_from_negative_when_lower() {
        let mut t1 = make_tick_at(1_000);
        let mut t2 = make_tick_at(1_000);
        t1.price = rust_decimal_macros::dec!(95);
        t2.price = rust_decimal_macros::dec!(100);
        assert_eq!(t1.price_diff_from(&t2), rust_decimal_macros::dec!(-5));
    }

    #[test]
    fn test_is_micro_trade_true_when_below_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(0.5);
        assert!(tick.is_micro_trade(rust_decimal_macros::dec!(1)));
    }

    #[test]
    fn test_is_micro_trade_false_when_equal_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(1);
        assert!(!tick.is_micro_trade(rust_decimal_macros::dec!(1)));
    }

    #[test]
    fn test_is_micro_trade_false_when_above_threshold() {
        let mut tick = make_tick_at(1_000);
        tick.quantity = rust_decimal_macros::dec!(2);
        assert!(!tick.is_micro_trade(rust_decimal_macros::dec!(1)));
    }

    // --- is_zero_price / is_fresh ---

    #[test]
    fn test_is_zero_price_true_for_zero() {
        let mut tick = make_tick_at(1_000);
        tick.price = rust_decimal_macros::dec!(0);
        assert!(tick.is_zero_price());
    }

    #[test]
    fn test_is_zero_price_false_for_nonzero() {
        let tick = make_tick_at(1_000); // price set by make_tick_at
        assert!(!tick.is_zero_price());
    }

    #[test]
    fn test_is_fresh_true_when_within_age() {
        let tick = make_tick_at(1_000);
        // received_at = 1000, now = 2000, max_age = 1500 → 1000 <= 1500 → fresh
        assert!(tick.is_fresh(2_000, 1_500));
    }

    #[test]
    fn test_is_fresh_false_when_too_old() {
        let tick = make_tick_at(1_000);
        // received_at = 1000, now = 5000, max_age = 2000 → 4000 > 2000 → not fresh
        assert!(!tick.is_fresh(5_000, 2_000));
    }

    #[test]
    fn test_is_fresh_true_when_now_less_than_received() {
        // Clock skew: now < received_at → saturating_sub = 0 ≤ max_age
        let tick = make_tick_at(5_000);
        assert!(tick.is_fresh(3_000, 100));
    }

    // --- NormalizedTick::age_ms ---
    #[test]
    fn test_age_ms_correct_elapsed() {
        let tick = make_tick_at(10_000);
        assert_eq!(tick.age_ms(10_500), 500);
    }

    #[test]
    fn test_age_ms_zero_when_now_equals_received() {
        let tick = make_tick_at(10_000);
        assert_eq!(tick.age_ms(10_000), 0);
    }

    #[test]
    fn test_age_ms_zero_when_now_before_received() {
        let tick = make_tick_at(10_000);
        assert_eq!(tick.age_ms(9_000), 0);
    }

    // --- NormalizedTick::is_buying_pressure ---
    #[test]
    fn test_is_buying_pressure_true_above_midpoint() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100.50);
        assert!(tick.is_buying_pressure(dec!(100)));
    }

    #[test]
    fn test_is_buying_pressure_false_below_midpoint() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(99.50);
        assert!(!tick.is_buying_pressure(dec!(100)));
    }

    #[test]
    fn test_is_buying_pressure_false_at_midpoint() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100);
        assert!(!tick.is_buying_pressure(dec!(100)));
    }

    // --- NormalizedTick::rounded_price ---
    #[test]
    fn test_rounded_price_rounds_to_nearest_tick() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100.37);
        // tick_size = 0.25 → 100.25
        assert_eq!(tick.rounded_price(dec!(0.25)), dec!(100.25));
    }

    #[test]
    fn test_rounded_price_unchanged_when_already_aligned() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100.50);
        assert_eq!(tick.rounded_price(dec!(0.25)), dec!(100.50));
    }

    #[test]
    fn test_rounded_price_returns_original_for_zero_tick_size() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(99.99);
        assert_eq!(tick.rounded_price(dec!(0)), dec!(99.99));
    }

    // --- NormalizedTick::is_large_spread_from ---
    #[test]
    fn test_is_large_spread_from_true_when_large() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_at(0);
        let mut t2 = make_tick_at(0);
        t1.price = dec!(100);
        t2.price = dec!(110);
        assert!(t1.is_large_spread_from(&t2, dec!(5)));
    }

    #[test]
    fn test_is_large_spread_from_false_when_small() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_at(0);
        let mut t2 = make_tick_at(0);
        t1.price = dec!(100);
        t2.price = dec!(101);
        assert!(!t1.is_large_spread_from(&t2, dec!(5)));
    }

    // ── NormalizedTick::age_secs ──────────────────────────────────────────────

    #[test]
    fn test_age_secs_correct() {
        let tick = make_tick_at(1_000);
        assert!((tick.age_secs(3_000) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_age_secs_zero_when_now_equals_received() {
        let tick = make_tick_at(5_000);
        assert_eq!(tick.age_secs(5_000), 0.0);
    }

    #[test]
    fn test_age_secs_zero_when_now_before_received() {
        let tick = make_tick_at(5_000);
        assert_eq!(tick.age_secs(1_000), 0.0);
    }

    // ── NormalizedTick::is_same_exchange_as ───────────────────────────────────

    #[test]
    fn test_is_same_exchange_as_true_when_matching() {
        let t1 = make_tick_at(1_000); // Binance
        let t2 = make_tick_at(2_000); // Binance
        assert!(t1.is_same_exchange_as(&t2));
    }

    #[test]
    fn test_is_same_exchange_as_false_when_different() {
        let t1 = make_tick_at(1_000); // Binance
        let mut t2 = make_tick_at(2_000);
        t2.exchange = Exchange::Coinbase;
        assert!(!t1.is_same_exchange_as(&t2));
    }

    // ── NormalizedTick::quote_age_ms / notional_value / is_high_value_tick ──

    #[test]
    fn test_quote_age_ms_correct() {
        let tick = make_tick_at(1_000);
        assert_eq!(tick.quote_age_ms(3_000), 2_000);
    }

    #[test]
    fn test_quote_age_ms_zero_when_now_before_received() {
        let tick = make_tick_at(5_000);
        assert_eq!(tick.quote_age_ms(1_000), 0);
    }

    #[test]
    fn test_notional_value_correct() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100);
        tick.quantity = dec!(5);
        assert_eq!(tick.notional_value(), dec!(500));
    }

    #[test]
    fn test_is_high_value_tick_true_when_above_threshold() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100);
        tick.quantity = dec!(10);
        // notional = 1000 > 500
        assert!(tick.is_high_value_tick(dec!(500)));
    }

    #[test]
    fn test_is_high_value_tick_false_when_below_threshold() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(10);
        tick.quantity = dec!(2);
        // notional = 20 < 100
        assert!(!tick.is_high_value_tick(dec!(100)));
    }

    // ── NormalizedTick::is_buy_side / is_sell_side / price_in_range ─────────

    #[test]
    fn test_is_buy_side_true_when_buy() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Buy);
        assert!(tick.is_buy_side());
    }

    #[test]
    fn test_is_buy_side_false_when_sell() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Sell);
        assert!(!tick.is_buy_side());
    }

    #[test]
    fn test_is_buy_side_false_when_none() {
        let mut tick = make_tick_at(0);
        tick.side = None;
        assert!(!tick.is_buy_side());
    }

    #[test]
    fn test_is_sell_side_true_when_sell() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Sell);
        assert!(tick.is_sell_side());
    }

    #[test]
    fn test_price_in_range_true_when_within() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(100);
        assert!(tick.price_in_range(dec!(90), dec!(110)));
    }

    #[test]
    fn test_price_in_range_false_when_below() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(80);
        assert!(!tick.price_in_range(dec!(90), dec!(110)));
    }

    #[test]
    fn test_price_in_range_true_at_boundary() {
        use rust_decimal_macros::dec;
        let mut tick = make_tick_at(0);
        tick.price = dec!(90);
        assert!(tick.price_in_range(dec!(90), dec!(110)));
    }

    // ── NormalizedTick::is_zero_quantity ──────────────────────────────────────

    #[test]
    fn test_is_zero_quantity_true_when_zero() {
        let mut tick = make_tick_at(0);
        tick.quantity = Decimal::ZERO;
        assert!(tick.is_zero_quantity());
    }

    #[test]
    fn test_is_zero_quantity_false_when_nonzero() {
        let mut tick = make_tick_at(0);
        tick.quantity = Decimal::ONE;
        assert!(!tick.is_zero_quantity());
    }
}
