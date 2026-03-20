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
    ///
    /// See also [`volume_notional`](Self::volume_notional), [`notional_value`](Self::notional_value),
    /// and [`dollar_value`](Self::dollar_value), which are all aliases.
    pub fn value(&self) -> Decimal {
        self.price * self.quantity
    }

    /// Age of this tick in milliseconds relative to `now_ms`.
    ///
    /// Returns `now_ms - received_at_ms`, saturating at zero (never negative).
    /// Useful for staleness checks: `tick.age_ms(now) > threshold_ms`.
    ///
    /// See also [`quote_age_ms`](Self::quote_age_ms), which is an alias.
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
    ///
    /// See also [`exchange_latency_ms`](Self::exchange_latency_ms), which is an alias.
    pub fn latency_ms(&self) -> Option<i64> {
        let exchange_ts = self.exchange_ts_ms? as i64;
        Some(self.received_at_ms as i64 - exchange_ts)
    }

    /// Notional value of this trade: `price × quantity`.
    ///
    /// Alias for [`value`](Self::value).
    pub fn volume_notional(&self) -> rust_decimal::Decimal {
        self.value()
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
    /// Alias for [`latency_ms`](Self::latency_ms).
    pub fn exchange_latency_ms(&self) -> Option<i64> {
        self.latency_ms()
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
    /// Alias for [`price_move_from`](Self::price_move_from).
    #[deprecated(since = "2.2.0", note = "Use `price_move_from` instead")]
    pub fn price_diff_from(&self, other: &NormalizedTick) -> Decimal {
        self.price_move_from(other)
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
    /// Alias for [`age_ms`](Self::age_ms).
    #[deprecated(since = "2.2.0", note = "Use `age_ms` instead")]
    pub fn quote_age_ms(&self, now_ms: u64) -> u64 {
        self.age_ms(now_ms)
    }

    /// Notional value of this tick: `price × quantity`.
    ///
    /// Alias for [`value`](Self::value).
    #[deprecated(since = "2.2.0", note = "Use `value` instead")]
    pub fn notional_value(&self) -> Decimal {
        self.value()
    }

    /// Returns `true` if the notional value (`price × quantity`) exceeds `threshold`.
    ///
    /// Alias for [`is_notional_large_trade`](Self::is_notional_large_trade).
    #[deprecated(since = "2.2.0", note = "Use `is_notional_large_trade` instead")]
    pub fn is_high_value_tick(&self, threshold: Decimal) -> bool {
        self.is_notional_large_trade(threshold)
    }

    /// Returns the trade side as a string slice: `"buy"`, `"sell"`, or `None`.
    pub fn side_as_str(&self) -> Option<&'static str> {
        match self.side {
            Some(TradeSide::Buy) => Some("buy"),
            Some(TradeSide::Sell) => Some("sell"),
            None => None,
        }
    }

    /// Returns `true` if this tick's price is strictly above `reference`.
    ///
    /// Alias for [`is_above`](Self::is_above).
    #[deprecated(since = "2.2.0", note = "Use `is_above` instead")]
    pub fn is_above_price(&self, reference: Decimal) -> bool {
        self.is_above(reference)
    }

    /// Signed price change relative to `reference`: `self.price - reference`.
    pub fn price_change_from(&self, reference: Decimal) -> Decimal {
        self.price - reference
    }

    /// Returns `true` if this tick's `received_at_ms` falls within a trading session window.
    pub fn is_market_open_tick(&self, session_start_ms: u64, session_end_ms: u64) -> bool {
        self.received_at_ms >= session_start_ms && self.received_at_ms < session_end_ms
    }

    /// Returns `true` if this tick's price exactly equals `target`.
    ///
    /// Alias for [`is_at`](Self::is_at).
    #[deprecated(since = "2.2.0", note = "Use `is_at` instead")]
    pub fn is_at_price(&self, target: Decimal) -> bool {
        self.is_at(target)
    }

    /// Returns `true` if this tick's price is strictly below `reference`.
    ///
    /// Alias for [`is_below`](Self::is_below).
    #[deprecated(since = "2.2.0", note = "Use `is_below` instead")]
    pub fn is_below_price(&self, reference: Decimal) -> bool {
        self.is_below(reference)
    }

    /// Returns `true` if this tick's price is divisible by `step` with no remainder.
    ///
    /// Useful for identifying round-number price levels (e.g., `step = 100`).
    /// Returns `false` if `step` is zero.
    pub fn is_round_number(&self, step: Decimal) -> bool {
        if step.is_zero() {
            return false;
        }
        (self.price % step).is_zero()
    }

    /// Returns the trade quantity signed by side: `+quantity` for Buy, `-quantity` for Sell, `0` for unknown.
    pub fn signed_quantity(&self) -> Decimal {
        match self.side {
            Some(TradeSide::Buy) => self.quantity,
            Some(TradeSide::Sell) => -self.quantity,
            None => Decimal::ZERO,
        }
    }

    /// Returns `(price, quantity)` as a convenient tuple.
    pub fn as_price_level(&self) -> (Decimal, Decimal) {
        (self.price, self.quantity)
    }

    /// Returns `true` if this tick's quantity is strictly above `threshold`.
    pub fn quantity_above(&self, threshold: Decimal) -> bool {
        self.quantity > threshold
    }

    /// Returns `true` if this tick was received within `threshold_ms` of `now_ms`.
    ///
    /// Alias for [`is_fresh(now_ms, threshold_ms)`](Self::is_fresh).
    #[deprecated(since = "2.2.0", note = "Use `is_fresh(now_ms, threshold_ms)` instead")]
    pub fn is_recent(&self, threshold_ms: u64, now_ms: u64) -> bool {
        self.is_fresh(now_ms, threshold_ms)
    }

    /// Returns `true` if this tick is on the buy side.
    ///
    /// Alias for [`is_buy`](Self::is_buy).
    #[deprecated(since = "2.2.0", note = "Use `is_buy` instead")]
    pub fn is_buy_side(&self) -> bool {
        self.is_buy()
    }

    /// Returns `true` if this tick is on the sell side.
    ///
    /// Alias for [`is_sell`](Self::is_sell).
    #[deprecated(since = "2.2.0", note = "Use `is_sell` instead")]
    pub fn is_sell_side(&self) -> bool {
        self.is_sell()
    }

    /// Returns `true` if this tick's quantity is zero (may indicate a cancel or correction).
    pub fn is_zero_quantity(&self) -> bool {
        self.quantity.is_zero()
    }

    /// Returns `true` if this tick's price is strictly between `bid` and `ask`.
    pub fn is_within_spread(&self, bid: Decimal, ask: Decimal) -> bool {
        self.price > bid && self.price < ask
    }

    /// Returns `true` if this tick's price deviates from `reference` by more than `threshold`.
    pub fn is_away_from_price(&self, reference: Decimal, threshold: Decimal) -> bool {
        (self.price - reference).abs() > threshold
    }

    /// Returns `true` if this tick's quantity is strictly above `threshold`.
    ///
    /// Note: unlike [`is_large_trade`](Self::is_large_trade) which uses `>=`,
    /// this method uses strict `>` for backwards compatibility.
    #[deprecated(since = "2.2.0", note = "Use `is_large_trade` instead")]
    pub fn is_large_tick(&self, threshold: Decimal) -> bool {
        self.quantity > threshold
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

    /// Returns `true` if price reversed direction by at least `min_move` from `prev`.
    ///
    /// A reversal means the direction of `(self.price - prev.price)` is opposite to
    /// the direction of `(prev.price - prev_prev.price)`, and the magnitude ≥ `min_move`.
    /// This two-argument form checks: `|self.price - prev.price| >= min_move`.
    pub fn is_reversal(&self, prev: &NormalizedTick, min_move: Decimal) -> bool {
        let move_size = (self.price - prev.price).abs();
        move_size >= min_move
    }

    /// Returns `true` if a trade crossed the spread: `bid_tick.price >= ask_tick.price`.
    ///
    /// A spread-crossed condition indicates an aggressive order consumed
    /// the best opposing quote.
    pub fn spread_crossed(bid_tick: &NormalizedTick, ask_tick: &NormalizedTick) -> bool {
        bid_tick.price >= ask_tick.price
    }

    /// Dollar (notional) value of this tick: `price * quantity`.
    ///
    /// Alias for [`value`](Self::value).
    pub fn dollar_value(&self) -> Decimal {
        self.value()
    }

    /// Contract value using a futures/options multiplier: `price * quantity * multiplier`.
    pub fn contract_value(&self, multiplier: Decimal) -> Decimal {
        self.value() * multiplier
    }

    /// Tick imbalance: `(buy_qty - sell_qty) / total_qty` across a tick slice.
    ///
    /// Buy ticks are those with `side == Some(TradeSide::Buy)`.
    /// Returns `None` if total quantity is zero.
    pub fn tick_imbalance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_qty: Decimal = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.quantity)
            .sum();
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() { return None; }
        let sell_qty = total_qty - buy_qty;
        ((buy_qty - sell_qty) / total_qty).to_f64()
    }

    /// Theoretical quote midpoint: `(bid.price + ask.price) / 2`.
    ///
    /// Returns `None` if either tick has a non-positive price or if the bid
    /// price exceeds the ask price (crossed market).
    pub fn quote_midpoint(bid: &NormalizedTick, ask: &NormalizedTick) -> Option<Decimal> {
        if bid.price <= Decimal::ZERO || ask.price <= Decimal::ZERO {
            return None;
        }
        if bid.price > ask.price {
            return None;
        }
        Some((bid.price + ask.price) / Decimal::TWO)
    }

    /// Total quantity across all buy-initiated ticks in a slice.
    ///
    /// Filters ticks where `side == Some(TradeSide::Buy)` and sums their quantities.
    /// Returns `Decimal::ZERO` for an empty slice or one with no buy ticks.
    pub fn buy_volume(ticks: &[NormalizedTick]) -> Decimal {
        ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.quantity)
            .sum()
    }

    /// Total quantity across all sell-initiated ticks in a slice.
    ///
    /// Filters ticks where `side == Some(TradeSide::Sell)` and sums their quantities.
    /// Returns `Decimal::ZERO` for an empty slice or one with no sell ticks.
    pub fn sell_volume(ticks: &[NormalizedTick]) -> Decimal {
        ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .map(|t| t.quantity)
            .sum()
    }

    /// Price range across a slice of ticks: `max(price) − min(price)`.
    ///
    /// Returns `None` if the slice is empty.
    pub fn price_range(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let max = ticks.iter().map(|t| t.price).max()?;
        let min = ticks.iter().map(|t| t.price).min()?;
        Some(max - min)
    }

    /// Arithmetic mean price across a slice of ticks.
    ///
    /// Returns `None` if the slice is empty.
    pub fn average_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let sum: Decimal = ticks.iter().map(|t| t.price).sum();
        Some(sum / Decimal::from(ticks.len() as u64))
    }

    /// Volume-weighted average price (VWAP) across a slice of ticks.
    ///
    /// Computes `Σ(price × quantity) / Σ(quantity)`.
    /// Returns `None` if the slice is empty or total volume is zero.
    pub fn vwap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let volume: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if volume.is_zero() {
            return None;
        }
        Some(Self::total_notional(ticks) / volume)
    }

    /// Count of ticks whose price is strictly above `threshold`.
    pub fn count_above_price(ticks: &[NormalizedTick], threshold: Decimal) -> usize {
        ticks.iter().filter(|t| t.price > threshold).count()
    }

    /// Count of ticks whose price is strictly below `threshold`.
    pub fn count_below_price(ticks: &[NormalizedTick], threshold: Decimal) -> usize {
        ticks.iter().filter(|t| t.price < threshold).count()
    }

    /// Total notional value (`Σ price × quantity`) across all ticks.
    pub fn total_notional(ticks: &[NormalizedTick]) -> Decimal {
        ticks.iter().map(|t| t.value()).sum()
    }

    /// Total notional for buy-side ticks (`side == Some(TradeSide::Buy)`).
    pub fn buy_notional(ticks: &[NormalizedTick]) -> Decimal {
        ticks.iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.value())
            .sum()
    }

    /// Total notional for sell-side ticks (`side == Some(TradeSide::Sell)`).
    pub fn sell_notional(ticks: &[NormalizedTick]) -> Decimal {
        ticks.iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .map(|t| t.value())
            .sum()
    }

    /// Median price across a slice of ticks.
    ///
    /// Sorts tick prices and returns the middle value (or mean of two middle
    /// values for an even-length slice). Returns `None` for an empty slice.
    pub fn median_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let mut prices: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        prices.sort();
        let n = prices.len();
        if n % 2 == 1 {
            Some(prices[n / 2])
        } else {
            Some((prices[n / 2 - 1] + prices[n / 2]) / Decimal::from(2u64))
        }
    }

    /// Net volume: `buy_volume − sell_volume`.
    ///
    /// Positive means net buying pressure; negative means net selling pressure.
    pub fn net_volume(ticks: &[NormalizedTick]) -> Decimal {
        Self::buy_volume(ticks) - Self::sell_volume(ticks)
    }

    /// Average trade quantity: `total_volume / tick_count`.
    ///
    /// Returns `None` if the slice is empty.
    pub fn average_quantity(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        Some(total / Decimal::from(ticks.len() as u64))
    }

    /// Maximum single-trade quantity across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn max_quantity(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().map(|t| t.quantity).reduce(Decimal::max)
    }

    /// Minimum single-trade quantity across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn min_quantity(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().map(|t| t.quantity).reduce(Decimal::min)
    }

    /// Number of buy-side ticks in the slice.
    pub fn buy_count(ticks: &[NormalizedTick]) -> usize {
        ticks.iter().filter(|t| t.is_buy()).count()
    }

    /// Number of sell-side ticks in the slice.
    pub fn sell_count(ticks: &[NormalizedTick]) -> usize {
        ticks.iter().filter(|t| t.is_sell()).count()
    }

    /// Percentage price change from the first to the last tick.
    ///
    /// Returns `None` if the slice has fewer than 2 ticks or the first
    /// tick's price is zero.
    pub fn price_momentum(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 2 {
            return None;
        }
        let first = ticks[0].price;
        let last = ticks[n - 1].price;
        if first.is_zero() {
            return None;
        }
        ((last - first) / first).to_f64()
    }

    /// Minimum price across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn min_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().map(|t| t.price).reduce(Decimal::min)
    }

    /// Maximum price across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn max_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().map(|t| t.price).reduce(Decimal::max)
    }

    /// Standard deviation of tick prices across the slice.
    ///
    /// Returns `None` if the slice has fewer than 2 elements.
    pub fn price_std_dev(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 2 { return None; }
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if vals.len() < 2 { return None; }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let variance = vals.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (vals.len() - 1) as f64;
        Some(variance.sqrt())
    }

    /// Ratio of buy volume to sell volume.
    ///
    /// Returns `None` if sell volume is zero.
    pub fn buy_sell_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let sell = Self::sell_volume(ticks);
        if sell.is_zero() {
            return None;
        }
        (Self::buy_volume(ticks) / sell).to_f64()
    }

    /// Returns the tick with the highest quantity in the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn largest_trade(ticks: &[NormalizedTick]) -> Option<&NormalizedTick> {
        ticks.iter().max_by(|a, b| a.quantity.cmp(&b.quantity))
    }

    /// Count of ticks whose quantity strictly exceeds `threshold`.
    pub fn large_trade_count(ticks: &[NormalizedTick], threshold: Decimal) -> usize {
        ticks.iter().filter(|t| t.quantity > threshold).count()
    }

    /// Interquartile range (Q3 − Q1) of tick prices.
    ///
    /// Returns `None` if the slice has fewer than 4 elements.
    pub fn price_iqr(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let n = ticks.len();
        if n < 4 {
            return None;
        }
        let mut prices: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        prices.sort();
        let q1_idx = n / 4;
        let q3_idx = 3 * n / 4;
        Some(prices[q3_idx] - prices[q1_idx])
    }

    /// Fraction of ticks that are buy-side.
    ///
    /// Returns `None` if the slice is empty.
    pub fn fraction_buy(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        Some(Self::buy_count(ticks) as f64 / ticks.len() as f64)
    }

    /// Standard deviation of trade quantities across the slice.
    ///
    /// Returns `None` if the slice has fewer than 2 elements.
    pub fn std_quantity(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        Self::sample_std_dev_f64(&vals)
    }

    /// Fraction of sided volume that is buy-initiated: buy_vol / (buy_vol + sell_vol).
    ///
    /// Returns `None` if there are no sided ticks.
    pub fn buy_pressure(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy = Self::buy_volume(ticks);
        let sell = Self::sell_volume(ticks);
        let total = buy + sell;
        if total.is_zero() {
            return None;
        }
        (buy / total).to_f64()
    }

    /// Mean notional value (price × quantity) per trade across the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn average_notional(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        Some(Self::total_notional(ticks) / Decimal::from(ticks.len() as u64))
    }

    /// Count of ticks with no known side (neither buy nor sell).
    pub fn count_neutral(ticks: &[NormalizedTick]) -> usize {
        ticks.iter().filter(|t| t.is_neutral()).count()
    }

    /// Returns the last `n` ticks from the slice.
    ///
    /// If `n >= ticks.len()`, returns the full slice.
    pub fn recent(ticks: &[NormalizedTick], n: usize) -> &[NormalizedTick] {
        let len = ticks.len();
        if n >= len { ticks } else { &ticks[len - n..] }
    }

    /// OLS linear regression slope of price over tick index.
    ///
    /// A positive slope indicates prices are rising across the slice.
    /// Returns `None` if the slice has fewer than 2 ticks.
    pub fn price_linear_slope(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 2 {
            return None;
        }
        let n_f = n as f64;
        let xs: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let ys: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if ys.len() < 2 {
            return None;
        }
        let x_mean = xs.iter().sum::<f64>() / n_f;
        let y_mean = ys.iter().sum::<f64>() / ys.len() as f64;
        let numerator: f64 = xs.iter().zip(ys.iter()).map(|(&x, &y)| (x - x_mean) * (y - y_mean)).sum();
        let denominator: f64 = xs.iter().map(|&x| (x - x_mean).powi(2)).sum();
        if denominator == 0.0 {
            return None;
        }
        Some(numerator / denominator)
    }

    /// Standard deviation of per-trade notional values (price × quantity).
    ///
    /// Returns `None` if the slice has fewer than 2 elements.
    pub fn notional_std_dev(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.value().to_f64()).collect();
        Self::sample_std_dev_f64(&vals)
    }

    /// Returns `true` if prices in the slice are non-decreasing (each ≥ previous).
    ///
    /// Returns `true` for slices with 0 or 1 ticks.
    pub fn monotone_up(ticks: &[NormalizedTick]) -> bool {
        ticks.windows(2).all(|w| w[1].price >= w[0].price)
    }

    /// Returns `true` if prices in the slice are non-increasing (each ≤ previous).
    ///
    /// Returns `true` for slices with 0 or 1 ticks.
    pub fn monotone_down(ticks: &[NormalizedTick]) -> bool {
        ticks.windows(2).all(|w| w[1].price <= w[0].price)
    }

    /// Total quantity traded at exactly `price`.
    pub fn volume_at_price(ticks: &[NormalizedTick], price: Decimal) -> Decimal {
        ticks.iter().filter(|t| t.price == price).map(|t| t.quantity).sum()
    }

    /// Price of the most recent tick in the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn last_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.last().map(|t| t.price)
    }

    /// Length of the longest consecutive run of buy-sided ticks.
    pub fn longest_buy_streak(ticks: &[NormalizedTick]) -> usize {
        let mut max = 0usize;
        let mut current = 0usize;
        for t in ticks {
            if t.is_buy() {
                current += 1;
                max = max.max(current);
            } else {
                current = 0;
            }
        }
        max
    }

    /// Length of the longest consecutive run of sell-sided ticks.
    pub fn longest_sell_streak(ticks: &[NormalizedTick]) -> usize {
        let mut max = 0usize;
        let mut current = 0usize;
        for t in ticks {
            if t.is_sell() {
                current += 1;
                max = max.max(current);
            } else {
                current = 0;
            }
        }
        max
    }

    /// Price level with the highest total traded quantity in the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn price_at_max_volume(ticks: &[NormalizedTick]) -> Option<Decimal> {
        use std::collections::HashMap;
        if ticks.is_empty() {
            return None;
        }
        let mut volume_by_price: HashMap<String, (Decimal, Decimal)> = HashMap::new();
        for t in ticks {
            let key = t.price.to_string();
            let entry = volume_by_price.entry(key).or_insert((t.price, Decimal::ZERO));
            entry.1 += t.quantity;
        }
        volume_by_price
            .values()
            .max_by(|a, b| a.1.cmp(&b.1))
            .map(|(price, _)| *price)
    }

    /// Total quantity of the last `n` ticks.
    ///
    /// If `n >= ticks.len()`, returns total volume of all ticks.
    pub fn recent_volume(ticks: &[NormalizedTick], n: usize) -> Decimal {
        Self::recent(ticks, n).iter().map(|t| t.quantity).sum()
    }

    /// Sample standard deviation of an `f64` slice.
    ///
    /// Returns `None` if `vals` has fewer than 2 elements.
    fn sample_std_dev_f64(vals: &[f64]) -> Option<f64> {
        let n = vals.len();
        if n < 2 {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n as f64;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
        Some(variance.sqrt())
    }

    /// Price of the first tick in the slice.
    ///
    /// Returns `None` if the slice is empty.
    pub fn first_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.first().map(|t| t.price)
    }

    /// Percentage return from first to last tick price: (last − first) / first.
    ///
    /// Returns `None` if the slice has fewer than 2 ticks or the first price is zero.
    pub fn price_return_pct(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 2 { return None; }
        let first = ticks[0].price;
        if first.is_zero() { return None; }
        ((ticks[n - 1].price - first) / first).to_f64()
    }

    /// Total quantity traded strictly above `price`.
    pub fn volume_above_price(ticks: &[NormalizedTick], price: Decimal) -> Decimal {
        ticks.iter().filter(|t| t.price > price).map(|t| t.quantity).sum()
    }

    /// Total quantity traded strictly below `price`.
    pub fn volume_below_price(ticks: &[NormalizedTick], price: Decimal) -> Decimal {
        ticks.iter().filter(|t| t.price < price).map(|t| t.quantity).sum()
    }

    /// Quantity-weighted average price (VWAP) across all ticks.
    ///
    /// Returns `None` if the slice is empty or total quantity is zero.
    pub fn quantity_weighted_avg_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let weighted: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum();
        Some(weighted / total_qty)
    }

    /// Number of ticks with price strictly above `price`.
    pub fn tick_count_above_price(ticks: &[NormalizedTick], price: Decimal) -> usize {
        ticks.iter().filter(|t| t.price > price).count()
    }

    /// Number of ticks with price strictly below `price`.
    pub fn tick_count_below_price(ticks: &[NormalizedTick], price: Decimal) -> usize {
        ticks.iter().filter(|t| t.price < price).count()
    }

    /// Approximate price at the given percentile (0.0–1.0) by sorting tick prices.
    ///
    /// Returns `None` if the slice is empty or `percentile` is outside [0, 1].
    pub fn price_at_percentile(ticks: &[NormalizedTick], percentile: f64) -> Option<Decimal> {
        if ticks.is_empty() || !(0.0..=1.0).contains(&percentile) {
            return None;
        }
        let mut prices: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        prices.sort();
        let idx = ((prices.len() - 1) as f64 * percentile).round() as usize;
        Some(prices[idx])
    }

    /// Number of distinct prices in the tick slice.
    pub fn unique_price_count(ticks: &[NormalizedTick]) -> usize {
        use std::collections::HashSet;
        ticks.iter().map(|t| t.price.to_string()).collect::<HashSet<_>>().len()
    }

    /// Average absolute price difference between consecutive ticks.
    ///
    /// Returns `None` if fewer than 2 ticks.
    pub fn avg_inter_tick_spread(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let sum: Decimal = ticks.windows(2).map(|w| (w[1].price - w[0].price).abs()).sum();
        (sum / Decimal::from((ticks.len() - 1) as u32)).to_f64()
    }

    /// Largest single-trade quantity among sell-side ticks.
    ///
    /// Returns `None` if there are no sell-side ticks.
    pub fn largest_sell(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().filter(|t| t.is_sell()).map(|t| t.quantity).reduce(Decimal::max)
    }

    /// Largest single-trade quantity among buy-side ticks.
    ///
    /// Returns `None` if there are no buy-side ticks.
    pub fn largest_buy(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().filter(|t| t.is_buy()).map(|t| t.quantity).reduce(Decimal::max)
    }

    /// Total number of ticks in the slice (alias for `slice.len()`).
    pub fn trade_count(ticks: &[NormalizedTick]) -> usize {
        ticks.len()
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

    // ── NormalizedTick::is_large_tick ─────────────────────────────────────────

    #[test]
    fn test_is_large_tick_true_when_above_threshold() {
        let mut tick = make_tick_at(0);
        tick.quantity = Decimal::from(10u32);
        assert!(tick.is_large_tick(Decimal::from(5u32)));
    }

    #[test]
    fn test_is_large_tick_false_when_at_threshold() {
        let mut tick = make_tick_at(0);
        tick.quantity = Decimal::from(5u32);
        assert!(!tick.is_large_tick(Decimal::from(5u32)));
    }

    #[test]
    fn test_is_large_tick_false_when_below_threshold() {
        let mut tick = make_tick_at(0);
        tick.quantity = Decimal::from(1u32);
        assert!(!tick.is_large_tick(Decimal::from(5u32)));
    }

    // ── NormalizedTick::is_away_from_price ───────────────────────────────────

    #[test]
    fn test_is_away_from_price_true_when_beyond_threshold() {
        let mut tick = make_tick_at(0);
        tick.price = Decimal::from(110u32);
        // |110 - 100| = 10 > 5
        assert!(tick.is_away_from_price(Decimal::from(100u32), Decimal::from(5u32)));
    }

    #[test]
    fn test_is_away_from_price_false_when_at_threshold() {
        let mut tick = make_tick_at(0);
        tick.price = Decimal::from(105u32);
        // |105 - 100| = 5, not > 5
        assert!(!tick.is_away_from_price(Decimal::from(100u32), Decimal::from(5u32)));
    }

    #[test]
    fn test_is_away_from_price_false_when_equal() {
        let mut tick = make_tick_at(0);
        tick.price = Decimal::from(100u32);
        assert!(!tick.is_away_from_price(Decimal::from(100u32), Decimal::from(1u32)));
    }

    // ── NormalizedTick::is_within_spread ──────────────────────────────────────

    #[test]
    fn test_is_within_spread_true_when_between() {
        let mut tick = make_tick_at(0);
        tick.price = Decimal::from(100u32);
        assert!(tick.is_within_spread(Decimal::from(99u32), Decimal::from(101u32)));
    }

    #[test]
    fn test_is_within_spread_false_when_at_bid() {
        let mut tick = make_tick_at(0);
        tick.price = Decimal::from(99u32);
        assert!(!tick.is_within_spread(Decimal::from(99u32), Decimal::from(101u32)));
    }

    #[test]
    fn test_is_within_spread_false_when_above_ask() {
        let mut tick = make_tick_at(0);
        tick.price = Decimal::from(102u32);
        assert!(!tick.is_within_spread(Decimal::from(99u32), Decimal::from(101u32)));
    }

    // ── NormalizedTick::is_recent ─────────────────────────────────────────────

    #[test]
    fn test_is_recent_true_when_within_threshold() {
        let tick = make_tick_at(9_500);
        // now=10000, threshold=1000 → age=500ms ≤ 1000ms
        assert!(tick.is_recent(1_000, 10_000));
    }

    #[test]
    fn test_is_recent_false_when_beyond_threshold() {
        let tick = make_tick_at(8_000);
        // now=10000, threshold=1000 → age=2000ms > 1000ms
        assert!(!tick.is_recent(1_000, 10_000));
    }

    #[test]
    fn test_is_recent_true_at_exact_threshold() {
        let tick = make_tick_at(9_000);
        // age=1000ms, threshold=1000ms → exactly at threshold
        assert!(tick.is_recent(1_000, 10_000));
    }

    // ── NormalizedTick::side_as_str ───────────────────────────────────────────

    #[test]
    fn test_side_as_str_buy() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Buy);
        assert_eq!(tick.side_as_str(), Some("buy"));
    }

    #[test]
    fn test_side_as_str_sell() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Sell);
        assert_eq!(tick.side_as_str(), Some("sell"));
    }

    #[test]
    fn test_side_as_str_none_when_unknown() {
        let mut tick = make_tick_at(0);
        tick.side = None;
        assert!(tick.side_as_str().is_none());
    }

    // ── is_above_price ────────────────────────────────────────────────────────

    #[test]
    fn test_is_above_price_true_when_strictly_above() {
        let tick = make_tick_at(0); // price=100
        assert!(tick.is_above_price(rust_decimal_macros::dec!(99)));
    }

    #[test]
    fn test_is_above_price_false_when_equal() {
        let tick = make_tick_at(0); // price=100
        assert!(!tick.is_above_price(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_is_above_price_false_when_below() {
        let tick = make_tick_at(0); // price=100
        assert!(!tick.is_above_price(rust_decimal_macros::dec!(101)));
    }

    // ── price_change_from ─────────────────────────────────────────────────────

    #[test]
    fn test_price_change_from_positive_when_above_reference() {
        let tick = make_tick_at(0); // price=100
        assert_eq!(tick.price_change_from(rust_decimal_macros::dec!(90)), rust_decimal_macros::dec!(10));
    }

    #[test]
    fn test_price_change_from_negative_when_below_reference() {
        let tick = make_tick_at(0); // price=100
        assert_eq!(tick.price_change_from(rust_decimal_macros::dec!(110)), rust_decimal_macros::dec!(-10));
    }

    #[test]
    fn test_price_change_from_zero_when_equal() {
        let tick = make_tick_at(0); // price=100
        assert_eq!(tick.price_change_from(rust_decimal_macros::dec!(100)), rust_decimal_macros::dec!(0));
    }

    // ── is_below_price ────────────────────────────────────────────────────────

    #[test]
    fn test_is_below_price_true_when_strictly_below() {
        let tick = make_tick_at(0); // price=100
        assert!(tick.is_below_price(rust_decimal_macros::dec!(101)));
    }

    #[test]
    fn test_is_below_price_false_when_equal() {
        let tick = make_tick_at(0); // price=100
        assert!(!tick.is_below_price(rust_decimal_macros::dec!(100)));
    }

    // ── quantity_above ────────────────────────────────────────────────────────

    #[test]
    fn test_quantity_above_true_when_quantity_exceeds_threshold() {
        let tick = make_tick_at(0); // quantity=1
        assert!(tick.quantity_above(rust_decimal_macros::dec!(0)));
    }

    #[test]
    fn test_quantity_above_false_when_quantity_equals_threshold() {
        let tick = make_tick_at(0); // quantity=1
        assert!(!tick.quantity_above(rust_decimal_macros::dec!(1)));
    }

    // ── is_at_price ───────────────────────────────────────────────────────────

    #[test]
    fn test_is_at_price_true_when_equal() {
        let tick = make_tick_at(0); // price=100
        assert!(tick.is_at_price(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_is_at_price_false_when_different() {
        let tick = make_tick_at(0); // price=100
        assert!(!tick.is_at_price(rust_decimal_macros::dec!(101)));
    }

    // ── is_round_number ───────────────────────────────────────────────────────

    #[test]
    fn test_is_round_number_true_when_divisible() {
        let tick = make_tick_at(0); // price=100
        assert!(tick.is_round_number(rust_decimal_macros::dec!(10)));
        assert!(tick.is_round_number(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_is_round_number_false_when_not_divisible() {
        let tick = make_tick_at(0); // price=100
        assert!(!tick.is_round_number(rust_decimal_macros::dec!(3)));
    }

    #[test]
    fn test_is_round_number_false_when_step_zero() {
        let tick = make_tick_at(0);
        assert!(!tick.is_round_number(rust_decimal_macros::dec!(0)));
    }

    // ── is_market_open_tick ───────────────────────────────────────────────────

    #[test]
    fn test_is_market_open_tick_true_when_within_session() {
        let tick = make_tick_at(500); // received at ms=500
        assert!(tick.is_market_open_tick(100, 1_000));
    }

    #[test]
    fn test_is_market_open_tick_false_when_before_session() {
        let tick = make_tick_at(50);
        assert!(!tick.is_market_open_tick(100, 1_000));
    }

    #[test]
    fn test_is_market_open_tick_false_when_at_session_end() {
        let tick = make_tick_at(1_000);
        assert!(!tick.is_market_open_tick(100, 1_000)); // exclusive end
    }

    // ── signed_quantity ───────────────────────────────────────────────────────

    #[test]
    fn test_signed_quantity_positive_for_buy() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Buy);
        assert!(tick.signed_quantity() > rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_signed_quantity_negative_for_sell() {
        let mut tick = make_tick_at(0);
        tick.side = Some(TradeSide::Sell);
        assert!(tick.signed_quantity() < rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_signed_quantity_zero_for_unknown() {
        let tick = make_tick_at(0); // side=None
        assert_eq!(tick.signed_quantity(), rust_decimal::Decimal::ZERO);
    }

    // ── as_price_level ────────────────────────────────────────────────────────

    #[test]
    fn test_as_price_level_returns_price_and_quantity() {
        let tick = make_tick_at(0); // price=100, qty=1
        let (p, q) = tick.as_price_level();
        assert_eq!(p, rust_decimal_macros::dec!(100));
        assert_eq!(q, rust_decimal_macros::dec!(1));
    }

    // ── buy_volume / sell_volume ───────────────────────────────────────────────

    fn make_sided_tick(qty: rust_decimal::Decimal, side: Option<TradeSide>) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            price: rust_decimal_macros::dec!(100),
            quantity: qty,
            side,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    #[test]
    fn test_buy_volume_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::buy_volume(&[]), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_buy_volume_sums_only_buy_ticks() {
        let buy1 = make_sided_tick(rust_decimal_macros::dec!(2), Some(TradeSide::Buy));
        let sell = make_sided_tick(rust_decimal_macros::dec!(3), Some(TradeSide::Sell));
        let buy2 = make_sided_tick(rust_decimal_macros::dec!(5), Some(TradeSide::Buy));
        let unknown = make_sided_tick(rust_decimal_macros::dec!(10), None);
        assert_eq!(
            NormalizedTick::buy_volume(&[buy1, sell, buy2, unknown]),
            rust_decimal_macros::dec!(7)
        );
    }

    #[test]
    fn test_sell_volume_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::sell_volume(&[]), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_sell_volume_sums_only_sell_ticks() {
        let buy = make_sided_tick(rust_decimal_macros::dec!(2), Some(TradeSide::Buy));
        let sell1 = make_sided_tick(rust_decimal_macros::dec!(3), Some(TradeSide::Sell));
        let sell2 = make_sided_tick(rust_decimal_macros::dec!(4), Some(TradeSide::Sell));
        assert_eq!(
            NormalizedTick::sell_volume(&[buy, sell1, sell2]),
            rust_decimal_macros::dec!(7)
        );
    }

    #[test]
    fn test_buy_sell_volumes_dont_include_unknown_side() {
        let buy = make_sided_tick(rust_decimal_macros::dec!(5), Some(TradeSide::Buy));
        let sell = make_sided_tick(rust_decimal_macros::dec!(3), Some(TradeSide::Sell));
        let unknown = make_sided_tick(rust_decimal_macros::dec!(2), None);
        let ticks = [buy, sell, unknown];
        let total: rust_decimal::Decimal = ticks.iter().map(|t| t.quantity).sum();
        let accounted = NormalizedTick::buy_volume(&ticks) + NormalizedTick::sell_volume(&ticks);
        // 5 + 3 = 8, total = 10 (unknown 2 not counted)
        assert_eq!(accounted, rust_decimal_macros::dec!(8));
        assert!(accounted < total);
    }

    // ── price_range / average_price ───────────────────────────────────────────

    fn make_tick_with_price(price: rust_decimal::Decimal) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            price,
            quantity: rust_decimal_macros::dec!(1),
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    #[test]
    fn test_price_range_none_for_empty_slice() {
        assert!(NormalizedTick::price_range(&[]).is_none());
    }

    #[test]
    fn test_price_range_zero_for_single_tick() {
        let tick = make_tick_with_price(rust_decimal_macros::dec!(100));
        assert_eq!(NormalizedTick::price_range(&[tick]), Some(rust_decimal_macros::dec!(0)));
    }

    #[test]
    fn test_price_range_correct_for_multiple_ticks() {
        let t1 = make_tick_with_price(rust_decimal_macros::dec!(95));
        let t2 = make_tick_with_price(rust_decimal_macros::dec!(105));
        let t3 = make_tick_with_price(rust_decimal_macros::dec!(100));
        assert_eq!(NormalizedTick::price_range(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(10)));
    }

    #[test]
    fn test_average_price_none_for_empty_slice() {
        assert!(NormalizedTick::average_price(&[]).is_none());
    }

    #[test]
    fn test_average_price_equals_price_for_single_tick() {
        let tick = make_tick_with_price(rust_decimal_macros::dec!(200));
        assert_eq!(NormalizedTick::average_price(&[tick]), Some(rust_decimal_macros::dec!(200)));
    }

    #[test]
    fn test_average_price_correct_for_multiple_ticks() {
        let t1 = make_tick_with_price(rust_decimal_macros::dec!(90));
        let t2 = make_tick_with_price(rust_decimal_macros::dec!(100));
        let t3 = make_tick_with_price(rust_decimal_macros::dec!(110));
        // (90 + 100 + 110) / 3 = 100
        assert_eq!(NormalizedTick::average_price(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(100)));
    }

    // ── vwap ─────────────────────────────────────────────────────────────────

    fn make_tick_pq(price: rust_decimal::Decimal, qty: rust_decimal::Decimal) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Binance,
            symbol: "BTCUSDT".into(),
            price,
            quantity: qty,
            side: None,
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: 0,
        }
    }

    #[test]
    fn test_vwap_none_for_empty_slice() {
        assert!(NormalizedTick::vwap(&[]).is_none());
    }

    #[test]
    fn test_vwap_equals_price_for_single_tick() {
        let tick = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(5));
        assert_eq!(NormalizedTick::vwap(&[tick]), Some(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_vwap_weighted_correctly() {
        // 100 × 1 + 200 × 3 = 700; total qty = 4; VWAP = 175
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(200), rust_decimal_macros::dec!(3));
        assert_eq!(NormalizedTick::vwap(&[t1, t2]), Some(rust_decimal_macros::dec!(175)));
    }

    #[test]
    fn test_vwap_none_for_zero_total_volume() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(0));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(200), rust_decimal_macros::dec!(0));
        assert!(NormalizedTick::vwap(&[t1, t2]).is_none());
    }

    // ── count_above_price / count_below_price ─────────────────────────────────

    #[test]
    fn test_count_above_price_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::count_above_price(&[], rust_decimal_macros::dec!(100)), 0);
    }

    #[test]
    fn test_count_above_price_correct() {
        let t1 = make_tick_with_price(rust_decimal_macros::dec!(90));
        let t2 = make_tick_with_price(rust_decimal_macros::dec!(100));
        let t3 = make_tick_with_price(rust_decimal_macros::dec!(110));
        assert_eq!(NormalizedTick::count_above_price(&[t1, t2, t3], rust_decimal_macros::dec!(100)), 1);
    }

    #[test]
    fn test_count_below_price_correct() {
        let t1 = make_tick_with_price(rust_decimal_macros::dec!(90));
        let t2 = make_tick_with_price(rust_decimal_macros::dec!(100));
        let t3 = make_tick_with_price(rust_decimal_macros::dec!(110));
        assert_eq!(NormalizedTick::count_below_price(&[t1, t2, t3], rust_decimal_macros::dec!(100)), 1);
    }

    #[test]
    fn test_count_above_at_threshold_excluded() {
        let tick = make_tick_with_price(rust_decimal_macros::dec!(100));
        assert_eq!(NormalizedTick::count_above_price(&[tick], rust_decimal_macros::dec!(100)), 0);
    }

    #[test]
    fn test_count_below_at_threshold_excluded() {
        let tick = make_tick_with_price(rust_decimal_macros::dec!(100));
        assert_eq!(NormalizedTick::count_below_price(&[tick], rust_decimal_macros::dec!(100)), 0);
    }

    // ── total_notional / buy_notional / sell_notional ─────────────────────────

    #[test]
    fn test_total_notional_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::total_notional(&[]), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_total_notional_sums_all_ticks() {
        // 100 × 2 + 200 × 3 = 200 + 600 = 800
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(200), rust_decimal_macros::dec!(3));
        assert_eq!(NormalizedTick::total_notional(&[t1, t2]), rust_decimal_macros::dec!(800));
    }

    #[test]
    fn test_buy_notional_only_includes_buy_side() {
        let buy = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2));
        let sell = make_tick_pq(rust_decimal_macros::dec!(200), rust_decimal_macros::dec!(3));
        let buy_with_side = NormalizedTick { side: Some(TradeSide::Buy), ..buy };
        let sell_with_side = NormalizedTick { side: Some(TradeSide::Sell), ..sell };
        // buy notional = 100 × 2 = 200
        assert_eq!(NormalizedTick::buy_notional(&[buy_with_side, sell_with_side]), rust_decimal_macros::dec!(200));
    }

    #[test]
    fn test_sell_notional_only_includes_sell_side() {
        let buy = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2));
        let sell = make_tick_pq(rust_decimal_macros::dec!(200), rust_decimal_macros::dec!(3));
        let buy_with_side = NormalizedTick { side: Some(TradeSide::Buy), ..buy };
        let sell_with_side = NormalizedTick { side: Some(TradeSide::Sell), ..sell };
        // sell notional = 200 × 3 = 600
        assert_eq!(NormalizedTick::sell_notional(&[buy_with_side, sell_with_side]), rust_decimal_macros::dec!(600));
    }

    // ── median_price ──────────────────────────────────────────────────────────

    #[test]
    fn test_median_price_none_for_empty_slice() {
        assert!(NormalizedTick::median_price(&[]).is_none());
    }

    #[test]
    fn test_median_price_single_tick() {
        let tick = make_tick_with_price(rust_decimal_macros::dec!(150));
        assert_eq!(NormalizedTick::median_price(&[tick]), Some(rust_decimal_macros::dec!(150)));
    }

    #[test]
    fn test_median_price_odd_count() {
        let t1 = make_tick_with_price(rust_decimal_macros::dec!(90));
        let t2 = make_tick_with_price(rust_decimal_macros::dec!(100));
        let t3 = make_tick_with_price(rust_decimal_macros::dec!(110));
        assert_eq!(NormalizedTick::median_price(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(100)));
    }

    #[test]
    fn test_median_price_even_count() {
        let t1 = make_tick_with_price(rust_decimal_macros::dec!(90));
        let t2 = make_tick_with_price(rust_decimal_macros::dec!(100));
        // median = (90+100)/2 = 95
        assert_eq!(NormalizedTick::median_price(&[t1, t2]), Some(rust_decimal_macros::dec!(95)));
    }

    // ── net_volume ────────────────────────────────────────────────────────────

    #[test]
    fn test_net_volume_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::net_volume(&[]), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn test_net_volume_positive_when_more_buys() {
        let buy = NormalizedTick {
            side: Some(TradeSide::Buy),
            quantity: rust_decimal_macros::dec!(5),
            ..make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(5))
        };
        let sell = NormalizedTick {
            side: Some(TradeSide::Sell),
            quantity: rust_decimal_macros::dec!(3),
            ..make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(3))
        };
        assert_eq!(NormalizedTick::net_volume(&[buy, sell]), rust_decimal_macros::dec!(2));
    }

    #[test]
    fn test_net_volume_negative_when_more_sells() {
        let buy = NormalizedTick {
            side: Some(TradeSide::Buy),
            quantity: rust_decimal_macros::dec!(2),
            ..make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2))
        };
        let sell = NormalizedTick {
            side: Some(TradeSide::Sell),
            quantity: rust_decimal_macros::dec!(7),
            ..make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(7))
        };
        assert_eq!(NormalizedTick::net_volume(&[buy, sell]), rust_decimal_macros::dec!(-5));
    }

    // ── average_quantity / max_quantity ───────────────────────────────────────

    #[test]
    fn test_average_quantity_none_for_empty_slice() {
        assert!(NormalizedTick::average_quantity(&[]).is_none());
    }

    #[test]
    fn test_average_quantity_single_tick() {
        let tick = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(5));
        assert_eq!(NormalizedTick::average_quantity(&[tick]), Some(rust_decimal_macros::dec!(5)));
    }

    #[test]
    fn test_average_quantity_multiple_ticks() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(4));
        // (2 + 4) / 2 = 3
        assert_eq!(NormalizedTick::average_quantity(&[t1, t2]), Some(rust_decimal_macros::dec!(3)));
    }

    #[test]
    fn test_max_quantity_none_for_empty_slice() {
        assert!(NormalizedTick::max_quantity(&[]).is_none());
    }

    #[test]
    fn test_max_quantity_returns_largest() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(10));
        let t3 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(5));
        assert_eq!(NormalizedTick::max_quantity(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(10)));
    }

    #[test]
    fn test_min_quantity_none_for_empty_slice() {
        assert!(NormalizedTick::min_quantity(&[]).is_none());
    }

    #[test]
    fn test_min_quantity_returns_smallest() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(5));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        let t3 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(3));
        assert_eq!(NormalizedTick::min_quantity(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(1)));
    }

    #[test]
    fn test_buy_count_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::buy_count(&[]), 0);
    }

    #[test]
    fn test_buy_count_counts_only_buys() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let neutral = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::buy_count(&[buy, sell, neutral]), 1);
    }

    #[test]
    fn test_sell_count_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::sell_count(&[]), 0);
    }

    #[test]
    fn test_sell_count_counts_only_sells() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell1 = make_tick_pq(dec!(100), dec!(1));
        sell1.side = Some(TradeSide::Sell);
        let mut sell2 = make_tick_pq(dec!(100), dec!(1));
        sell2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::sell_count(&[buy, sell1, sell2]), 2);
    }

    #[test]
    fn test_price_momentum_none_for_empty_slice() {
        assert!(NormalizedTick::price_momentum(&[]).is_none());
    }

    #[test]
    fn test_price_momentum_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::price_momentum(&[t]).is_none());
    }

    #[test]
    fn test_price_momentum_positive_when_price_rises() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let mom = NormalizedTick::price_momentum(&[t1, t2]).unwrap();
        assert!((mom - 0.1).abs() < 1e-9);
    }

    #[test]
    fn test_price_momentum_negative_when_price_falls() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(90), dec!(1));
        let mom = NormalizedTick::price_momentum(&[t1, t2]).unwrap();
        assert!(mom < 0.0);
    }

    #[test]
    fn test_min_price_none_for_empty_slice() {
        assert!(NormalizedTick::min_price(&[]).is_none());
    }

    #[test]
    fn test_min_price_returns_lowest() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(90), rust_decimal_macros::dec!(1));
        let t3 = make_tick_pq(rust_decimal_macros::dec!(110), rust_decimal_macros::dec!(1));
        assert_eq!(NormalizedTick::min_price(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(90)));
    }

    #[test]
    fn test_max_price_none_for_empty_slice() {
        assert!(NormalizedTick::max_price(&[]).is_none());
    }

    #[test]
    fn test_max_price_returns_highest() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(90), rust_decimal_macros::dec!(1));
        let t3 = make_tick_pq(rust_decimal_macros::dec!(110), rust_decimal_macros::dec!(1));
        assert_eq!(NormalizedTick::max_price(&[t1, t2, t3]), Some(rust_decimal_macros::dec!(110)));
    }

    #[test]
    fn test_price_std_dev_none_for_empty_slice() {
        assert!(NormalizedTick::price_std_dev(&[]).is_none());
    }

    #[test]
    fn test_price_std_dev_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::price_std_dev(&[t]).is_none());
    }

    #[test]
    fn test_price_std_dev_two_equal_prices_is_zero() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert_eq!(NormalizedTick::price_std_dev(&[t1, t2]), Some(0.0));
    }

    #[test]
    fn test_price_std_dev_positive_for_varying_prices() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(110), rust_decimal_macros::dec!(1));
        let t3 = make_tick_pq(rust_decimal_macros::dec!(90), rust_decimal_macros::dec!(1));
        let std = NormalizedTick::price_std_dev(&[t1, t2, t3]).unwrap();
        assert!(std > 0.0);
    }

    #[test]
    fn test_buy_sell_ratio_none_for_empty_slice() {
        assert!(NormalizedTick::buy_sell_ratio(&[]).is_none());
    }

    #[test]
    fn test_buy_sell_ratio_none_when_no_sells() {
        let mut t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        t.side = Some(TradeSide::Buy);
        assert!(NormalizedTick::buy_sell_ratio(&[t]).is_none());
    }

    #[test]
    fn test_buy_sell_ratio_two_to_one() {
        use rust_decimal_macros::dec;
        let mut buy1 = make_tick_pq(dec!(100), dec!(2));
        buy1.side = Some(TradeSide::Buy);
        let mut buy2 = make_tick_pq(dec!(100), dec!(2));
        buy2.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(2));
        sell.side = Some(TradeSide::Sell);
        let ratio = NormalizedTick::buy_sell_ratio(&[buy1, buy2, sell]).unwrap();
        assert!((ratio - 2.0).abs() < 1e-9);
    }

    #[test]
    fn test_largest_trade_none_for_empty_slice() {
        assert!(NormalizedTick::largest_trade(&[]).is_none());
    }

    #[test]
    fn test_largest_trade_returns_max_quantity_tick() {
        let t1 = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(2));
        let t2 = make_tick_pq(rust_decimal_macros::dec!(200), rust_decimal_macros::dec!(10));
        let t3 = make_tick_pq(rust_decimal_macros::dec!(150), rust_decimal_macros::dec!(5));
        let ticks = [t1, t2, t3];
        let largest = NormalizedTick::largest_trade(&ticks).unwrap();
        assert_eq!(largest.quantity, rust_decimal_macros::dec!(10));
    }

    #[test]
    fn test_large_trade_count_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::large_trade_count(&[], rust_decimal_macros::dec!(1)), 0);
    }

    #[test]
    fn test_large_trade_count_counts_trades_above_threshold() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(0.5));
        let t2 = make_tick_pq(dec!(100), dec!(5));
        let t3 = make_tick_pq(dec!(100), dec!(10));
        assert_eq!(NormalizedTick::large_trade_count(&[t1, t2, t3], dec!(1)), 2);
    }

    #[test]
    fn test_large_trade_count_strict_greater_than() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        // quantity == threshold → not counted (strict >)
        assert_eq!(NormalizedTick::large_trade_count(&[t], dec!(1)), 0);
    }

    #[test]
    fn test_price_iqr_none_for_small_slice() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::price_iqr(&[t.clone(), t.clone(), t]).is_none());
    }

    #[test]
    fn test_price_iqr_positive_for_varied_prices() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50), dec!(60), dec!(70), dec!(80)]
            .iter()
            .map(|&p| make_tick_pq(p, dec!(1)))
            .collect();
        let iqr = NormalizedTick::price_iqr(&ticks).unwrap();
        assert!(iqr > dec!(0));
    }

    #[test]
    fn test_fraction_buy_none_for_empty_slice() {
        assert!(NormalizedTick::fraction_buy(&[]).is_none());
    }

    #[test]
    fn test_fraction_buy_zero_when_no_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::fraction_buy(&[t]), Some(0.0));
    }

    #[test]
    fn test_fraction_buy_one_when_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        assert_eq!(NormalizedTick::fraction_buy(&[t]), Some(1.0));
    }

    #[test]
    fn test_fraction_buy_half_for_equal_mix() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let frac = NormalizedTick::fraction_buy(&[buy, sell]).unwrap();
        assert!((frac - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_std_quantity_none_for_empty_slice() {
        assert!(NormalizedTick::std_quantity(&[]).is_none());
    }

    #[test]
    fn test_std_quantity_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(5));
        assert!(NormalizedTick::std_quantity(&[t]).is_none());
    }

    #[test]
    fn test_std_quantity_zero_for_identical_quantities() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(5));
        let t2 = make_tick_pq(dec!(100), dec!(5));
        assert_eq!(NormalizedTick::std_quantity(&[t1, t2]), Some(0.0));
    }

    #[test]
    fn test_std_quantity_positive_for_varied_quantities() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(10));
        let std = NormalizedTick::std_quantity(&[t1, t2]).unwrap();
        assert!(std > 0.0);
    }

    #[test]
    fn test_buy_pressure_none_for_empty_slice() {
        assert!(NormalizedTick::buy_pressure(&[]).is_none());
    }

    #[test]
    fn test_buy_pressure_none_for_unsided_ticks() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::buy_pressure(&[t]).is_none());
    }

    #[test]
    fn test_buy_pressure_one_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let bp = NormalizedTick::buy_pressure(&[t]).unwrap();
        assert!((bp - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_buy_pressure_half_for_equal_volume() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(5));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(5));
        sell.side = Some(TradeSide::Sell);
        let bp = NormalizedTick::buy_pressure(&[buy, sell]).unwrap();
        assert!((bp - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_average_notional_none_for_empty_slice() {
        assert!(NormalizedTick::average_notional(&[]).is_none());
    }

    #[test]
    fn test_average_notional_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(2));
        assert_eq!(NormalizedTick::average_notional(&[t]), Some(dec!(200)));
    }

    #[test]
    fn test_average_notional_multiple_ticks() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1)); // notional = 100
        let t2 = make_tick_pq(dec!(200), dec!(1)); // notional = 200
        // avg = (100 + 200) / 2 = 150
        assert_eq!(NormalizedTick::average_notional(&[t1, t2]), Some(dec!(150)));
    }

    #[test]
    fn test_count_neutral_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::count_neutral(&[]), 0);
    }

    #[test]
    fn test_count_neutral_counts_sideless_ticks() {
        use rust_decimal_macros::dec;
        let neutral = make_tick_pq(dec!(100), dec!(1)); // side = None
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        assert_eq!(NormalizedTick::count_neutral(&[neutral, buy]), 1);
    }

    #[test]
    fn test_recent_returns_all_when_n_exceeds_len() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        assert_eq!(NormalizedTick::recent(&ticks, 10).len(), 2);
    }

    #[test]
    fn test_recent_returns_last_n() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(100), dec!(110), dec!(120), dec!(130)]
            .iter()
            .map(|&p| make_tick_pq(p, dec!(1)))
            .collect();
        let recent = NormalizedTick::recent(&ticks, 2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].price, dec!(120));
        assert_eq!(recent[1].price, dec!(130));
    }

    #[test]
    fn test_price_linear_slope_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::price_linear_slope(&[t]).is_none());
    }

    #[test]
    fn test_price_linear_slope_positive_for_rising_prices() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(100), dec!(110), dec!(120)]
            .iter()
            .map(|&p| make_tick_pq(p, dec!(1)))
            .collect();
        let slope = NormalizedTick::price_linear_slope(&ticks).unwrap();
        assert!(slope > 0.0);
    }

    #[test]
    fn test_price_linear_slope_negative_for_falling_prices() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(120), dec!(110), dec!(100)]
            .iter()
            .map(|&p| make_tick_pq(p, dec!(1)))
            .collect();
        let slope = NormalizedTick::price_linear_slope(&ticks).unwrap();
        assert!(slope < 0.0);
    }

    #[test]
    fn test_notional_std_dev_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::notional_std_dev(&[t]).is_none());
    }

    #[test]
    fn test_notional_std_dev_zero_for_identical_notionals() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1)); // notional=100
        let t2 = make_tick_pq(dec!(100), dec!(1)); // notional=100
        assert_eq!(NormalizedTick::notional_std_dev(&[t1, t2]), Some(0.0));
    }

    #[test]
    fn test_notional_std_dev_positive_for_varied_notionals() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1)); // notional=100
        let t2 = make_tick_pq(dec!(200), dec!(2)); // notional=400
        let std = NormalizedTick::notional_std_dev(&[t1, t2]).unwrap();
        assert!(std > 0.0);
    }

    #[test]
    fn test_monotone_up_true_for_empty_slice() {
        assert!(NormalizedTick::monotone_up(&[]));
    }

    #[test]
    fn test_monotone_up_true_for_non_decreasing_prices() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(100), dec!(100), dec!(110), dec!(120)]
            .iter().map(|&p| make_tick_pq(p, dec!(1))).collect();
        assert!(NormalizedTick::monotone_up(&ticks));
    }

    #[test]
    fn test_monotone_up_false_for_any_decrease() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(100), dec!(110), dec!(105)]
            .iter().map(|&p| make_tick_pq(p, dec!(1))).collect();
        assert!(!NormalizedTick::monotone_up(&ticks));
    }

    #[test]
    fn test_monotone_down_true_for_non_increasing_prices() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(120), dec!(110), dec!(110), dec!(100)]
            .iter().map(|&p| make_tick_pq(p, dec!(1))).collect();
        assert!(NormalizedTick::monotone_down(&ticks));
    }

    #[test]
    fn test_monotone_down_false_for_any_increase() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(100), dec!(90), dec!(95)]
            .iter().map(|&p| make_tick_pq(p, dec!(1))).collect();
        assert!(!NormalizedTick::monotone_down(&ticks));
    }

    #[test]
    fn test_volume_at_price_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::volume_at_price(&[], rust_decimal_macros::dec!(100)), rust_decimal_macros::dec!(0));
    }

    #[test]
    fn test_volume_at_price_sums_matching_ticks() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(2));
        let t2 = make_tick_pq(dec!(100), dec!(3));
        let t3 = make_tick_pq(dec!(110), dec!(5));
        assert_eq!(NormalizedTick::volume_at_price(&[t1, t2, t3], dec!(100)), dec!(5));
    }

    #[test]
    fn test_last_price_none_for_empty_slice() {
        assert!(NormalizedTick::last_price(&[]).is_none());
    }

    #[test]
    fn test_last_price_returns_last_tick_price() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        assert_eq!(NormalizedTick::last_price(&[t1, t2]), Some(dec!(110)));
    }

    #[test]
    fn test_longest_buy_streak_zero_for_empty() {
        assert_eq!(NormalizedTick::longest_buy_streak(&[]), 0);
    }

    #[test]
    fn test_longest_buy_streak_counts_consecutive_buys() {
        use rust_decimal_macros::dec;
        let mut b1 = make_tick_pq(dec!(100), dec!(1)); b1.side = Some(TradeSide::Buy);
        let mut b2 = make_tick_pq(dec!(100), dec!(1)); b2.side = Some(TradeSide::Buy);
        let mut s  = make_tick_pq(dec!(100), dec!(1)); s.side = Some(TradeSide::Sell);
        let mut b3 = make_tick_pq(dec!(100), dec!(1)); b3.side = Some(TradeSide::Buy);
        // streaks: [2, 1] → max = 2
        assert_eq!(NormalizedTick::longest_buy_streak(&[b1, b2, s, b3]), 2);
    }

    #[test]
    fn test_longest_sell_streak_zero_for_no_sells() {
        use rust_decimal_macros::dec;
        let mut b = make_tick_pq(dec!(100), dec!(1)); b.side = Some(TradeSide::Buy);
        assert_eq!(NormalizedTick::longest_sell_streak(&[b]), 0);
    }

    #[test]
    fn test_longest_sell_streak_correct() {
        use rust_decimal_macros::dec;
        let mut b  = make_tick_pq(dec!(100), dec!(1)); b.side = Some(TradeSide::Buy);
        let mut s1 = make_tick_pq(dec!(100), dec!(1)); s1.side = Some(TradeSide::Sell);
        let mut s2 = make_tick_pq(dec!(100), dec!(1)); s2.side = Some(TradeSide::Sell);
        let mut s3 = make_tick_pq(dec!(100), dec!(1)); s3.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::longest_sell_streak(&[b, s1, s2, s3]), 3);
    }

    #[test]
    fn test_price_at_max_volume_none_for_empty() {
        assert!(NormalizedTick::price_at_max_volume(&[]).is_none());
    }

    #[test]
    fn test_price_at_max_volume_returns_dominant_price() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(200), dec!(5));
        let t3 = make_tick_pq(dec!(200), dec!(3));
        // price 200 has total vol 8 > price 100 vol 1
        assert_eq!(NormalizedTick::price_at_max_volume(&[t1, t2, t3]), Some(dec!(200)));
    }

    #[test]
    fn test_recent_volume_zero_for_empty_slice() {
        assert_eq!(NormalizedTick::recent_volume(&[], 5), rust_decimal_macros::dec!(0));
    }

    #[test]
    fn test_recent_volume_sums_last_n_ticks() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)]
            .iter().map(|&q| make_tick_pq(dec!(100), q)).collect();
        // last 3 ticks: qty 3+4+5 = 12
        assert_eq!(NormalizedTick::recent_volume(&ticks, 3), dec!(12));
    }

    // ── NormalizedTick::first_price ───────────────────────────────────────────

    #[test]
    fn test_first_price_none_for_empty_slice() {
        assert!(NormalizedTick::first_price(&[]).is_none());
    }

    #[test]
    fn test_first_price_returns_first_tick_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(50), dec!(1)), make_tick_pq(dec!(60), dec!(1))];
        assert_eq!(NormalizedTick::first_price(&ticks), Some(dec!(50)));
    }

    // ── NormalizedTick::price_return_pct ─────────────────────────────────────

    #[test]
    fn test_price_return_pct_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_return_pct(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_return_pct_positive_for_rising_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(110), dec!(1))];
        let pct = NormalizedTick::price_return_pct(&ticks).unwrap();
        assert!((pct - 0.1).abs() < 1e-9);
    }

    #[test]
    fn test_price_return_pct_negative_for_falling_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(90), dec!(1))];
        let pct = NormalizedTick::price_return_pct(&ticks).unwrap();
        assert!((pct - (-0.1)).abs() < 1e-9);
    }

    // ── NormalizedTick::volume_above_price / volume_below_price ──────────────

    #[test]
    fn test_volume_above_price_zero_for_empty_slice() {
        use rust_decimal_macros::dec;
        assert_eq!(NormalizedTick::volume_above_price(&[], dec!(100)), dec!(0));
    }

    #[test]
    fn test_volume_above_price_sums_above_threshold() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(5)),
            make_tick_pq(dec!(100), dec!(10)),
            make_tick_pq(dec!(110), dec!(3)),
        ];
        // only price=110 is above 100
        assert_eq!(NormalizedTick::volume_above_price(&ticks, dec!(100)), dec!(3));
    }

    #[test]
    fn test_volume_below_price_sums_below_threshold() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(5)),
            make_tick_pq(dec!(100), dec!(10)),
            make_tick_pq(dec!(110), dec!(3)),
        ];
        // only price=90 is below 100
        assert_eq!(NormalizedTick::volume_below_price(&ticks, dec!(100)), dec!(5));
    }

    // ── NormalizedTick::quantity_weighted_avg_price ───────────────────────────

    #[test]
    fn test_qwap_none_for_empty_slice() {
        assert!(NormalizedTick::quantity_weighted_avg_price(&[]).is_none());
    }

    #[test]
    fn test_qwap_correct_for_equal_quantities() {
        use rust_decimal_macros::dec;
        // equal qty → simple average of prices
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(200), dec!(1))];
        assert_eq!(NormalizedTick::quantity_weighted_avg_price(&ticks), Some(dec!(150)));
    }

    #[test]
    fn test_qwap_weighted_towards_higher_volume() {
        use rust_decimal_macros::dec;
        // price=100 qty=1, price=200 qty=3 → (100+600)/4 = 175
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(200), dec!(3))];
        assert_eq!(NormalizedTick::quantity_weighted_avg_price(&ticks), Some(dec!(175)));
    }

    // ── NormalizedTick::tick_count_above_price / tick_count_below_price ───────

    #[test]
    fn test_tick_count_above_price_zero_for_empty_slice() {
        use rust_decimal_macros::dec;
        assert_eq!(NormalizedTick::tick_count_above_price(&[], dec!(100)), 0);
    }

    #[test]
    fn test_tick_count_above_price_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
        ];
        assert_eq!(NormalizedTick::tick_count_above_price(&ticks, dec!(100)), 2);
    }

    #[test]
    fn test_tick_count_below_price_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        assert_eq!(NormalizedTick::tick_count_below_price(&ticks, dec!(100)), 1);
    }

    // ── NormalizedTick::price_at_percentile ──────────────────────────────────

    #[test]
    fn test_price_at_percentile_none_for_empty_slice() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_at_percentile(&[], 0.5).is_none());
    }

    #[test]
    fn test_price_at_percentile_none_for_out_of_range() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::price_at_percentile(&ticks, 1.5).is_none());
    }

    #[test]
    fn test_price_at_percentile_median_for_sorted_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(10), dec!(1)),
            make_tick_pq(dec!(20), dec!(1)),
            make_tick_pq(dec!(30), dec!(1)),
            make_tick_pq(dec!(40), dec!(1)),
            make_tick_pq(dec!(50), dec!(1)),
        ];
        // 50th percentile → index 2 → price=30
        assert_eq!(NormalizedTick::price_at_percentile(&ticks, 0.5), Some(dec!(30)));
    }

    // ── NormalizedTick::unique_price_count ────────────────────────────────────

    #[test]
    fn test_unique_price_count_zero_for_empty() {
        assert_eq!(NormalizedTick::unique_price_count(&[]), 0);
    }

    #[test]
    fn test_unique_price_count_counts_distinct_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
        ];
        assert_eq!(NormalizedTick::unique_price_count(&ticks), 3);
    }

    // ── NormalizedTick::sell_volume / buy_volume ──────────────────────────────

    #[test]
    fn test_sell_volume_zero_for_empty() {
        assert_eq!(NormalizedTick::sell_volume(&[]), rust_decimal_macros::dec!(0));
    }

    #[test]
    fn test_sell_volume_sums_sell_side_only() {
        use rust_decimal_macros::dec;
        let mut buy_tick = make_tick_pq(dec!(100), dec!(5));
        buy_tick.side = Some(TradeSide::Buy);
        let mut sell_tick = make_tick_pq(dec!(100), dec!(3));
        sell_tick.side = Some(TradeSide::Sell);
        let no_side_tick = make_tick_pq(dec!(100), dec!(10));
        let ticks = [buy_tick, sell_tick, no_side_tick];
        assert_eq!(NormalizedTick::sell_volume(&ticks), dec!(3));
        assert_eq!(NormalizedTick::buy_volume(&ticks), dec!(5));
    }

    // ── NormalizedTick::avg_inter_tick_spread ─────────────────────────────────

    #[test]
    fn test_avg_inter_tick_spread_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_inter_tick_spread(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_avg_inter_tick_spread_correct_for_uniform_moves() {
        use rust_decimal_macros::dec;
        // prices: 100, 102, 104 → diffs: 2, 2 → avg = 2.0
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(104), dec!(1)),
        ];
        let spread = NormalizedTick::avg_inter_tick_spread(&ticks).unwrap();
        assert!((spread - 2.0).abs() < 1e-9);
    }

    // ── NormalizedTick::price_range ───────────────────────────────────────────

    #[test]
    fn test_price_range_none_for_empty() {
        assert!(NormalizedTick::price_range(&[]).is_none());
    }

    #[test]
    fn test_price_range_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        assert_eq!(NormalizedTick::price_range(&ticks), Some(dec!(20)));
    }

    // ── NormalizedTick::median_price ──────────────────────────────────────────

    #[test]
    fn test_median_price_none_for_empty() {
        assert!(NormalizedTick::median_price(&[]).is_none());
    }

    #[test]
    fn test_median_price_returns_middle_value() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(10), dec!(1)),
            make_tick_pq(dec!(30), dec!(1)),
            make_tick_pq(dec!(20), dec!(1)),
        ];
        // sorted: 10,20,30 → idx 1 = 20
        assert_eq!(NormalizedTick::median_price(&ticks), Some(dec!(20)));
    }

    // ── NormalizedTick::largest_sell / largest_buy ────────────────────────────

    #[test]
    fn test_largest_sell_none_for_no_sell_ticks() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(TradeSide::Buy);
        assert!(NormalizedTick::largest_sell(&[t]).is_none());
    }

    #[test]
    fn test_largest_sell_returns_max_sell_qty() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(3));
        t1.side = Some(TradeSide::Sell);
        let mut t2 = make_tick_pq(dec!(100), dec!(7));
        t2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::largest_sell(&[t1, t2]), Some(dec!(7)));
    }

    #[test]
    fn test_largest_buy_returns_max_buy_qty() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(2));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(9));
        t2.side = Some(TradeSide::Buy);
        assert_eq!(NormalizedTick::largest_buy(&[t1, t2]), Some(dec!(9)));
    }

    // ── NormalizedTick::trade_count ───────────────────────────────────────────

    #[test]
    fn test_trade_count_zero_for_empty() {
        assert_eq!(NormalizedTick::trade_count(&[]), 0);
    }

    #[test]
    fn test_trade_count_matches_slice_length() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(2))];
        assert_eq!(NormalizedTick::trade_count(&ticks), 2);
    }
}
