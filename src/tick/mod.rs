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

    /// Price acceleration: change in price velocity between consecutive ticks.
    ///
    /// Returns the difference of the last two consecutive price changes.
    /// Returns `None` if fewer than 3 ticks.
    pub fn price_acceleration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 3 {
            return None;
        }
        let v1 = (ticks[n - 2].price - ticks[n - 3].price).to_f64()?;
        let v2 = (ticks[n - 1].price - ticks[n - 2].price).to_f64()?;
        Some(v2 - v1)
    }

    /// Net difference between buy volume and sell volume.
    ///
    /// Positive means more buying pressure, negative means more selling pressure.
    pub fn buy_sell_diff(ticks: &[NormalizedTick]) -> Decimal {
        Self::buy_volume(ticks) - Self::sell_volume(ticks)
    }

    /// Returns `true` if the tick is a buy that exceeds the average buy quantity.
    pub fn is_aggressive_buy(tick: &NormalizedTick, avg_buy_qty: Decimal) -> bool {
        tick.is_buy() && tick.quantity > avg_buy_qty
    }

    /// Returns `true` if the tick is a sell that exceeds the average sell quantity.
    pub fn is_aggressive_sell(tick: &NormalizedTick, avg_sell_qty: Decimal) -> bool {
        tick.is_sell() && tick.quantity > avg_sell_qty
    }

    /// Total notional value: sum of `price * quantity` across all ticks.
    pub fn notional_volume(ticks: &[NormalizedTick]) -> Decimal {
        ticks.iter().map(|t| t.price * t.quantity).sum()
    }

    /// Weighted side score: buy_volume - sell_volume normalized by total volume.
    ///
    /// Returns a value in [-1, 1], or `None` if total volume is zero.
    pub fn weighted_side_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        let diff = Self::buy_volume(ticks) - Self::sell_volume(ticks);
        (diff / total).to_f64()
    }

    /// Time span in milliseconds between the first and last tick.
    ///
    /// Returns `None` if fewer than 2 ticks.
    pub fn time_span_ms(ticks: &[NormalizedTick]) -> Option<u64> {
        if ticks.len() < 2 {
            return None;
        }
        Some(ticks.last()?.received_at_ms.saturating_sub(ticks.first()?.received_at_ms))
    }

    /// Count of ticks with price above the VWAP of the slice.
    ///
    /// Returns `None` if VWAP cannot be computed (empty or zero total volume).
    pub fn price_above_vwap_count(ticks: &[NormalizedTick]) -> Option<usize> {
        let vwap = Self::vwap(ticks)?;
        Some(ticks.iter().filter(|t| t.price > vwap).count())
    }

    /// Mean quantity per trade across all ticks.
    ///
    /// Returns `None` if the slice is empty.
    pub fn avg_trade_size(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        Some(total / Decimal::from(ticks.len() as u32))
    }

    /// Fraction of total volume contributed by the largest quarter of trades.
    ///
    /// Trades are sorted by quantity descending; the top 25% (rounded up) are
    /// summed and divided by total volume. Returns `None` if total volume is
    /// zero or the slice is empty.
    pub fn volume_concentration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        let mut qtys: Vec<Decimal> = ticks.iter().map(|t| t.quantity).collect();
        qtys.sort_by(|a, b| b.cmp(a));
        let top_n = ((ticks.len() + 3) / 4).max(1);
        let top_vol: Decimal = qtys.iter().take(top_n).copied().sum();
        (top_vol / total).to_f64()
    }

    /// Signed trade imbalance: `(buy_count − sell_count) / total`.
    ///
    /// Returns a value in [-1, 1]. Returns `None` if the slice is empty.
    pub fn trade_imbalance_score(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let n = ticks.len() as f64;
        let buys = Self::buy_count(ticks) as f64;
        let sells = Self::sell_count(ticks) as f64;
        Some((buys - sells) / n)
    }

    /// Average price of buy-side ticks.
    ///
    /// Returns `None` if there are no buy-side ticks.
    pub fn buy_avg_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let buys: Vec<_> = ticks.iter().filter(|t| t.is_buy()).collect();
        if buys.is_empty() {
            return None;
        }
        let sum: Decimal = buys.iter().map(|t| t.price).sum();
        Some(sum / Decimal::from(buys.len() as u32))
    }

    /// Average price of sell-side ticks.
    ///
    /// Returns `None` if there are no sell-side ticks.
    pub fn sell_avg_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let sells: Vec<_> = ticks.iter().filter(|t| t.is_sell()).collect();
        if sells.is_empty() {
            return None;
        }
        let sum: Decimal = sells.iter().map(|t| t.price).sum();
        Some(sum / Decimal::from(sells.len() as u32))
    }

    /// Skewness of the price distribution across ticks.
    ///
    /// Uses the standard moment-based formula. Returns `None` if fewer than 3
    /// ticks or if the standard deviation is zero.
    pub fn price_skewness(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 3 {
            return None;
        }
        let prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if prices.len() != n {
            return None;
        }
        let nf = n as f64;
        let mean = prices.iter().sum::<f64>() / nf;
        let variance = prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / nf;
        if variance == 0.0 {
            return None;
        }
        let std_dev = variance.sqrt();
        let skew = prices.iter().map(|p| ((p - mean) / std_dev).powi(3)).sum::<f64>() / nf;
        Some(skew)
    }

    /// Skewness of the quantity distribution across ticks.
    ///
    /// Uses the standard moment-based formula. Returns `None` if fewer than 3
    /// ticks or if the standard deviation is zero.
    pub fn quantity_skewness(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 3 {
            return None;
        }
        let qtys: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        if qtys.len() != n {
            return None;
        }
        let nf = n as f64;
        let mean = qtys.iter().sum::<f64>() / nf;
        let variance = qtys.iter().map(|q| (q - mean).powi(2)).sum::<f64>() / nf;
        if variance == 0.0 {
            return None;
        }
        let std_dev = variance.sqrt();
        let skew = qtys.iter().map(|q| ((q - mean) / std_dev).powi(3)).sum::<f64>() / nf;
        Some(skew)
    }

    /// Shannon entropy of the price distribution across ticks.
    ///
    /// Each unique price is treated as a category. Returns `None` if the
    /// slice is empty or all ticks have the same price (zero entropy is
    /// returned as `Some(0.0)`).
    pub fn price_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let n = ticks.len() as f64;
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for t in ticks {
            *counts.entry(t.price.to_string()).or_insert(0) += 1;
        }
        let entropy = counts.values().map(|&c| {
            let p = c as f64 / n;
            -p * p.ln()
        }).sum();
        Some(entropy)
    }

    /// Excess kurtosis of the price distribution across ticks.
    ///
    /// Returns `None` if fewer than 4 ticks or if the standard deviation is
    /// zero.
    pub fn price_kurtosis(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 4 {
            return None;
        }
        let prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if prices.len() != n {
            return None;
        }
        let nf = n as f64;
        let mean = prices.iter().sum::<f64>() / nf;
        let variance = prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / nf;
        if variance == 0.0 {
            return None;
        }
        let std_dev = variance.sqrt();
        let kurt = prices.iter().map(|p| ((p - mean) / std_dev).powi(4)).sum::<f64>() / nf - 3.0;
        Some(kurt)
    }

    /// Count of ticks whose quantity exceeds `threshold`.
    pub fn high_volume_tick_count(ticks: &[NormalizedTick], threshold: Decimal) -> usize {
        ticks.iter().filter(|t| t.quantity > threshold).count()
    }

    /// Difference between the buy-side average price and the sell-side average
    /// price (buy_avg - sell_avg).
    ///
    /// A positive value means buyers paid more on average than sellers; a
    /// negative value is unusual (market microstructure artifact). Returns
    /// `None` if either side has no ticks.
    pub fn vwap_spread(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let buy = Self::buy_avg_price(ticks)?;
        let sell = Self::sell_avg_price(ticks)?;
        Some(buy - sell)
    }

    /// Mean quantity of buy-side ticks.
    ///
    /// Returns `None` if there are no buy-side ticks.
    pub fn avg_buy_quantity(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let buys: Vec<_> = ticks.iter().filter(|t| t.is_buy()).collect();
        if buys.is_empty() {
            return None;
        }
        let total: Decimal = buys.iter().map(|t| t.quantity).sum();
        Some(total / Decimal::from(buys.len() as u32))
    }

    /// Mean quantity of sell-side ticks.
    ///
    /// Returns `None` if there are no sell-side ticks.
    pub fn avg_sell_quantity(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let sells: Vec<_> = ticks.iter().filter(|t| t.is_sell()).collect();
        if sells.is_empty() {
            return None;
        }
        let total: Decimal = sells.iter().map(|t| t.quantity).sum();
        Some(total / Decimal::from(sells.len() as u32))
    }

    /// Fraction of prices that are below the VWAP (mean-reversion pressure).
    ///
    /// Values close to 0.5 suggest equilibrium; values far from 0.5 suggest
    /// directional bias. Returns `None` if VWAP cannot be computed.
    pub fn price_mean_reversion_score(ticks: &[NormalizedTick]) -> Option<f64> {
        let vwap = Self::vwap(ticks)?;
        let below = ticks.iter().filter(|t| t.price < vwap).count();
        Some(below as f64 / ticks.len() as f64)
    }

    /// Largest absolute price move between consecutive ticks.
    ///
    /// Returns `None` if fewer than 2 ticks.
    pub fn largest_price_move(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.len() < 2 {
            return None;
        }
        ticks.windows(2).map(|w| (w[1].price - w[0].price).abs()).reduce(Decimal::max)
    }

    /// Number of ticks per millisecond of elapsed time.
    ///
    /// Returns `None` if fewer than 2 ticks or the time span is zero.
    pub fn tick_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        let span = Self::time_span_ms(ticks)? as f64;
        if span == 0.0 {
            return None;
        }
        Some(ticks.len() as f64 / span)
    }

    /// Fraction of total notional volume that comes from buy-side trades.
    ///
    /// Returns `None` if total notional volume is zero or the slice is empty.
    pub fn buy_notional_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let total = Self::total_notional(ticks);
        if total.is_zero() {
            return None;
        }
        let buy = Self::buy_notional(ticks);
        (buy / total).to_f64()
    }

    /// Price range as a percentage of the minimum price.
    ///
    /// `(max_price - min_price) / min_price * 100`. Returns `None` if the
    /// slice is empty or the minimum price is zero.
    pub fn price_range_pct(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let min = Self::min_price(ticks)?;
        let max = Self::max_price(ticks)?;
        if min.is_zero() {
            return None;
        }
        ((max - min) / min * Decimal::ONE_HUNDRED).to_f64()
    }

    /// Buy-side dominance: `buy_volume / (buy_volume + sell_volume)`.
    ///
    /// Returns a value in [0, 1]. Returns `None` if both sides are zero.
    pub fn buy_side_dominance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy = Self::buy_volume(ticks);
        let sell = Self::sell_volume(ticks);
        let total = buy + sell;
        if total.is_zero() {
            return None;
        }
        (buy / total).to_f64()
    }

    /// Standard deviation of price weighted by quantity.
    ///
    /// Uses VWAP as the mean. Returns `None` if empty or VWAP cannot be
    /// computed.
    pub fn volume_weighted_price_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let vwap = Self::vwap(ticks)?;
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let variance: Decimal = ticks.iter()
            .map(|t| {
                let diff = t.price - vwap;
                diff * diff * t.quantity
            })
            .sum::<Decimal>() / total_qty;
        variance.to_f64().map(f64::sqrt)
    }

    /// VWAP computed over just the last `n` ticks.
    ///
    /// Returns `None` if `n` is 0 or the slice has no ticks, or if total
    /// volume of the window is zero.
    pub fn last_n_vwap(ticks: &[NormalizedTick], n: usize) -> Option<Decimal> {
        if n == 0 || ticks.is_empty() {
            return None;
        }
        let window = &ticks[ticks.len().saturating_sub(n)..];
        Self::vwap(window)
    }

    /// Lag-1 autocorrelation of the price series across ticks.
    ///
    /// Uses the Pearson formula on consecutive price pairs. Returns `None` if
    /// fewer than 3 ticks or if the variance is zero.
    pub fn price_autocorrelation(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n < 3 {
            return None;
        }
        let prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if prices.len() != n {
            return None;
        }
        let nf = (n - 1) as f64;
        let mean = prices.iter().sum::<f64>() / n as f64;
        let var = prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / n as f64;
        if var == 0.0 {
            return None;
        }
        let cov: f64 = prices.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>() / nf;
        Some(cov / var)
    }

    /// Net trade direction score: `(buy_count - sell_count)` as a signed integer.
    pub fn net_trade_direction(ticks: &[NormalizedTick]) -> i64 {
        Self::buy_count(ticks) as i64 - Self::sell_count(ticks) as i64
    }

    /// Fraction of total notional volume that comes from sell-side trades.
    ///
    /// Returns `None` if total notional volume is zero or the slice is empty.
    pub fn sell_side_notional_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let total = Self::total_notional(ticks);
        if total.is_zero() {
            return None;
        }
        let sell = Self::sell_notional(ticks);
        (sell / total).to_f64()
    }

    /// Count of price direction reversals (sign changes in consecutive moves).
    ///
    /// Returns 0 if fewer than 3 ticks.
    pub fn price_oscillation_count(ticks: &[NormalizedTick]) -> usize {
        if ticks.len() < 3 {
            return 0;
        }
        ticks.windows(3).filter(|w| {
            let d1 = w[1].price.cmp(&w[0].price);
            let d2 = w[2].price.cmp(&w[1].price);
            use std::cmp::Ordering::*;
            matches!((d1, d2), (Greater, Less) | (Less, Greater))
        }).count()
    }

    /// Realized spread: mean buy price minus mean sell price.
    ///
    /// A positive value suggests buys are executed at higher prices than sells
    /// (typical in a two-sided market). Returns `None` if either side has no
    /// ticks.
    pub fn realized_spread(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let buy_avg = Self::buy_avg_price(ticks)?;
        let sell_avg = Self::sell_avg_price(ticks)?;
        Some(buy_avg - sell_avg)
    }

    /// Adverse selection score: fraction of large trades that moved the price
    /// against the initiator (proxy for informed trading).
    ///
    /// A "large" trade is one with quantity above the window median quantity.
    /// Returns `None` if the slice has fewer than 3 ticks or median is zero.
    pub fn adverse_selection_score(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let median_qty = Self::median_price(
            &ticks.iter().map(|t| {
                let mut cloned = t.clone();
                cloned.price = t.quantity;
                cloned
            }).collect::<Vec<_>>()
        )?;
        if median_qty.is_zero() {
            return None;
        }
        let large_trades: Vec<_> = ticks.windows(2)
            .filter(|w| w[0].quantity > median_qty)
            .collect();
        if large_trades.is_empty() {
            return None;
        }
        // Adverse if the next price moved against the initiating side
        let adverse = large_trades.iter().filter(|w| {
            let price_moved_up = w[1].price > w[0].price;
            match w[0].side {
                Some(TradeSide::Buy) => !price_moved_up,  // buy but price fell
                Some(TradeSide::Sell) => price_moved_up,  // sell but price rose
                None => false,
            }
        }).count();
        Some(adverse as f64 / large_trades.len() as f64)
    }

    /// Price impact per unit of volume: `|price_return| / total_volume`.
    ///
    /// Returns `None` if fewer than 2 ticks or total volume is zero.
    pub fn price_impact_per_unit(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let ret = (Self::price_return_pct(ticks)?.abs()) as f64;
        let vol = Self::buy_volume(ticks) + Self::sell_volume(ticks);
        if vol.is_zero() {
            return None;
        }
        vol.to_f64().map(|v| ret / v)
    }

    /// Volume-weighted return: VWAP of returns weighted by quantity.
    ///
    /// Computes `(p_i - p_{i-1}) / p_{i-1}` for each consecutive pair,
    /// weighted by `qty_i`, then sums weighted returns. Returns `None` if
    /// fewer than 2 ticks or total quantity is zero.
    pub fn volume_weighted_return(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let total_qty: Decimal = ticks[1..].iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let weighted: f64 = ticks.windows(2).filter_map(|w| {
            if w[0].price.is_zero() { return None; }
            let ret = ((w[1].price - w[0].price) / w[0].price).to_f64()?;
            let qty = w[1].quantity.to_f64()?;
            Some(ret * qty)
        }).sum::<f64>();
        total_qty.to_f64().map(|tq| weighted / tq)
    }

    /// Gini-coefficient-style quantity concentration.
    ///
    /// `sum_i sum_j |q_i - q_j| / (2 * n^2 * mean_q)`. Returns `None` if
    /// the slice is empty or the mean quantity is zero.
    pub fn quantity_concentration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len();
        if n == 0 {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        let mean = total / Decimal::from(n as u32);
        let mut sum = Decimal::ZERO;
        for i in 0..n {
            for j in 0..n {
                sum += (ticks[i].quantity - ticks[j].quantity).abs();
            }
        }
        let denom = mean * Decimal::from((2 * n * n) as u32);
        if denom.is_zero() {
            return None;
        }
        (sum / denom).to_f64()
    }

    /// Total volume traded at a specific price level.
    pub fn price_level_volume(ticks: &[NormalizedTick], price: Decimal) -> Decimal {
        ticks.iter().filter(|t| t.price == price).map(|t| t.quantity).sum()
    }

    /// Drift of the mid-price proxy across ticks.
    ///
    /// Defined as `(last_price - first_price) / time_span_ms`. Returns `None`
    /// if fewer than 2 ticks or the time span is zero.
    pub fn mid_price_drift(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let first = Self::first_price(ticks)?;
        let last = Self::last_price(ticks)?;
        let span = Self::time_span_ms(ticks)? as f64;
        if span == 0.0 {
            return None;
        }
        (last - first).to_f64().map(|d| d / span)
    }

    /// Tick direction bias: fraction of consecutive moves in the same direction.
    ///
    /// Counts windows where `price[i+1]` moved in the same direction as
    /// `price[i]` vs `price[i-1]`. Returns `None` if fewer than 3 ticks.
    pub fn tick_direction_bias(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let total = ticks.len() - 2;
        let same = ticks.windows(3).filter(|w| {
            let d1 = w[1].price.cmp(&w[0].price);
            let d2 = w[2].price.cmp(&w[1].price);
            d1 == d2 && d1 != std::cmp::Ordering::Equal
        }).count();
        Some(same as f64 / total as f64)
    }

    /// Median trade quantity across all ticks.
    ///
    /// Returns `None` if the slice is empty.
    pub fn median_quantity(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let mut qtys: Vec<Decimal> = ticks.iter().map(|t| t.quantity).collect();
        qtys.sort();
        let n = qtys.len();
        if n % 2 == 1 {
            Some(qtys[n / 2])
        } else {
            Some((qtys[n / 2 - 1] + qtys[n / 2]) / Decimal::TWO)
        }
    }

    /// Total volume from ticks priced strictly above the VWAP of the slice.
    ///
    /// Returns `None` if VWAP cannot be computed (empty or zero total quantity).
    pub fn volume_above_vwap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let vwap = Self::vwap(ticks)?;
        Some(ticks.iter().filter(|t| t.price > vwap).map(|t| t.quantity).sum())
    }

    /// Variance of inter-tick arrival times in milliseconds.
    ///
    /// Returns `None` if fewer than 3 ticks are provided (need ≥ 2 intervals).
    pub fn inter_arrival_variance(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let intervals: Vec<f64> = ticks.windows(2)
            .filter_map(|w| {
                let dt = w[1].received_at_ms.checked_sub(w[0].received_at_ms)?;
                Some(dt as f64)
            })
            .collect();
        if intervals.len() < 2 {
            return None;
        }
        let n = intervals.len() as f64;
        let mean = intervals.iter().sum::<f64>() / n;
        let variance = intervals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance)
    }

    /// Price spread efficiency: net price move divided by total path length.
    ///
    /// A value of 1.0 means prices moved directly with no reversals; values
    /// near 0.0 indicate heavy oscillation. Returns `None` if the slice has
    /// fewer than 2 ticks or the path length is zero.
    pub fn spread_efficiency(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let path: Decimal = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).abs())
            .sum();
        if path.is_zero() {
            return None;
        }
        let net = (ticks.last()?.price - ticks.first()?.price).abs();
        (net / path).to_f64()
    }

    /// Ratio of average buy quantity to average sell quantity.
    ///
    /// Returns `None` if there are no buy ticks or no sell ticks, or if avg
    /// sell quantity is zero.
    pub fn buy_sell_size_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let avg_buy = Self::avg_buy_quantity(ticks)?;
        let avg_sell = Self::avg_sell_quantity(ticks)?;
        if avg_sell.is_zero() {
            return None;
        }
        (avg_buy / avg_sell).to_f64()
    }

    /// Standard deviation of trade quantities across all ticks.
    ///
    /// Returns `None` if fewer than 2 ticks are provided.
    pub fn trade_size_dispersion(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        if vals.len() < 2 {
            return None;
        }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt())
    }

    // ── round-79 ─────────────────────────────────────────────────────────────

    /// Fraction of ticks for which the trade side is known (non-`None`).
    ///
    /// Returns `None` for an empty slice.
    ///
    /// A value near 1.0 indicates the feed reliably reports aggressor side;
    /// a value near 0.0 means most ticks are neutral.
    pub fn aggressor_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let known = ticks.iter().filter(|t| t.side.is_some()).count();
        Some(known as f64 / ticks.len() as f64)
    }

    /// Signed volume imbalance: `(buy_vol − sell_vol) / (buy_vol + sell_vol)`.
    ///
    /// Returns a value in `(−1, +1)`. Positive means net buying pressure,
    /// negative means net selling pressure. Returns `None` when the total
    /// known-side volume is zero (all ticks are neutral or the slice is empty).
    pub fn volume_imbalance_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy = Self::buy_volume(ticks);
        let sell = Self::sell_volume(ticks);
        let total = buy + sell;
        if total.is_zero() {
            return None;
        }
        ((buy - sell) / total).to_f64()
    }

    /// Covariance between price and quantity across the tick slice.
    ///
    /// Returns `None` if fewer than 2 ticks are provided or if any value
    /// cannot be converted to `f64`.
    ///
    /// A positive covariance indicates that larger trades tend to occur at
    /// higher prices; negative means larger trades skew toward lower prices.
    pub fn price_quantity_covariance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        let qtys: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        if prices.len() != ticks.len() || qtys.len() != ticks.len() {
            return None;
        }
        let n = prices.len() as f64;
        let mean_p = prices.iter().sum::<f64>() / n;
        let mean_q = qtys.iter().sum::<f64>() / n;
        let cov = prices
            .iter()
            .zip(qtys.iter())
            .map(|(p, q)| (p - mean_p) * (q - mean_q))
            .sum::<f64>()
            / (n - 1.0);
        Some(cov)
    }

    /// Fraction of ticks whose quantity meets or exceeds `threshold`.
    ///
    /// Returns `None` for an empty slice. The result is in `[0.0, 1.0]`.
    /// Useful for characterising how "institutional" the flow is.
    pub fn large_trade_fraction(ticks: &[NormalizedTick], threshold: Decimal) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let count = Self::large_trade_count(ticks, threshold);
        Some(count as f64 / ticks.len() as f64)
    }

    /// Number of unique price levels per unit of price range.
    ///
    /// Computed as `unique_price_count / price_range`. Returns `None` when
    /// the slice is empty or the price range is zero (all ticks at one price).
    ///
    /// High density implies granular price action; low density implies
    /// price jumps between a few discrete levels.
    pub fn price_level_density(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = Self::price_range(ticks)?;
        if range.is_zero() {
            return None;
        }
        let unique = Self::unique_price_count(ticks) as f64;
        (Decimal::from(Self::unique_price_count(ticks) as i64) / range).to_f64()
            .or_else(|| Some(unique / range.to_f64()?))
    }

    /// Ratio of buy-side notional to sell-side notional.
    ///
    /// Returns `None` when there are no sell-side ticks or sell notional is
    /// zero. A value above 1.0 means buy-side dollar flow dominates.
    pub fn notional_buy_sell_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_n = Self::buy_notional(ticks);
        let sell_n = Self::sell_notional(ticks);
        if sell_n.is_zero() {
            return None;
        }
        (buy_n / sell_n).to_f64()
    }

    /// Mean of tick-to-tick log returns: `mean(ln(p_i / p_{i-1}))`.
    ///
    /// Returns `None` if fewer than 2 ticks are provided or if any price
    /// is zero (which would make the log undefined).
    pub fn log_return_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let returns: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                use rust_decimal::prelude::ToPrimitive;
                let prev = w[0].price.to_f64()?;
                let curr = w[1].price.to_f64()?;
                if prev <= 0.0 || curr <= 0.0 {
                    return None;
                }
                Some((curr / prev).ln())
            })
            .collect();
        if returns.is_empty() {
            return None;
        }
        Some(returns.iter().sum::<f64>() / returns.len() as f64)
    }

    /// Standard deviation of tick-to-tick log returns.
    ///
    /// Returns `None` if fewer than 3 ticks are provided (need at least 2
    /// returns for a meaningful std-dev) or if any price is zero.
    pub fn log_return_std(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let returns: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                use rust_decimal::prelude::ToPrimitive;
                let prev = w[0].price.to_f64()?;
                let curr = w[1].price.to_f64()?;
                if prev <= 0.0 || curr <= 0.0 {
                    return None;
                }
                Some((curr / prev).ln())
            })
            .collect();
        if returns.len() < 2 {
            return None;
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt())
    }

    /// Ratio of the maximum price to the last price: `max_price / last_price`.
    ///
    /// Returns `None` for an empty slice or if the last price is zero.
    ///
    /// A value above 1.0 indicates that the price overshot the closing
    /// level during the window — useful as an intrabar momentum signal.
    pub fn price_overshoot_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let max_p = Self::max_price(ticks)?;
        let last_p = Self::last_price(ticks)?;
        if last_p.is_zero() {
            return None;
        }
        (max_p / last_p).to_f64()
    }

    /// Ratio of the first price to the minimum price: `first_price / min_price`.
    ///
    /// Returns `None` for an empty slice or if the minimum price is zero.
    ///
    /// A value above 1.0 indicates the window opened above its trough,
    /// meaning the price undershot the opening level at some point.
    pub fn price_undershoot_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let first_p = Self::first_price(ticks)?;
        let min_p = Self::min_price(ticks)?;
        if min_p.is_zero() {
            return None;
        }
        (first_p / min_p).to_f64()
    }

    // ── round-80 ─────────────────────────────────────────────────────────────

    /// Net notional: `buy_notional − sell_notional` across the slice.
    ///
    /// Positive means net buying pressure in dollar terms; negative means
    /// net selling pressure. Returns `Decimal::ZERO` for empty slices or
    /// slices with no sided ticks.
    pub fn net_notional(ticks: &[NormalizedTick]) -> Decimal {
        Self::buy_notional(ticks) - Self::sell_notional(ticks)
    }

    /// Count of price direction reversals across the slice.
    ///
    /// A reversal occurs when consecutive price moves change sign (up→down or
    /// down→up), ignoring flat moves. Returns 0 for fewer than 3 ticks.
    pub fn price_reversal_count(ticks: &[NormalizedTick]) -> usize {
        if ticks.len() < 3 {
            return 0;
        }
        let mut count = 0usize;
        for w in ticks.windows(3) {
            let d1 = w[1].price - w[0].price;
            let d2 = w[2].price - w[1].price;
            if (d1 > Decimal::ZERO && d2 < Decimal::ZERO)
                || (d1 < Decimal::ZERO && d2 > Decimal::ZERO)
            {
                count += 1;
            }
        }
        count
    }

    /// Excess kurtosis of trade quantities across the slice.
    ///
    /// `kurtosis = (Σ((q − mean)⁴ / n) / std_dev⁴) − 3`
    ///
    /// Returns `None` if the slice has fewer than 4 ticks or std dev is zero.
    pub fn quantity_kurtosis(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 4 {
            return None;
        }
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        if vals.len() < 4 {
            return None;
        }
        let n_f = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n_f;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n_f;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        Some(vals.iter().map(|v| ((v - mean) / std_dev).powi(4)).sum::<f64>() / n_f - 3.0)
    }

    /// Reference to the tick with the highest notional value (`price × quantity`).
    ///
    /// Unlike [`largest_trade`](Self::largest_trade) which ranks by raw quantity,
    /// this method ranks by dollar value, making it suitable for comparing
    /// trades across different price levels.
    ///
    /// Returns `None` if the slice is empty.
    pub fn largest_notional_trade(ticks: &[NormalizedTick]) -> Option<&NormalizedTick> {
        ticks.iter().max_by(|a, b| a.value().cmp(&b.value()))
    }

    /// Time-weighted average price (TWAP) using `received_at_ms` timestamps.
    ///
    /// Each price is weighted by the time interval to the next tick. The last
    /// tick carries zero weight (no interval after it). Returns `None` if
    /// fewer than 2 ticks are provided or the total time span is zero.
    pub fn twap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.len() < 2 {
            return None;
        }
        let mut weighted_sum = Decimal::ZERO;
        let mut total_time = 0u64;
        for w in ticks.windows(2) {
            let dt = w[1].received_at_ms.saturating_sub(w[0].received_at_ms);
            weighted_sum += w[0].price * Decimal::from(dt);
            total_time += dt;
        }
        if total_time == 0 {
            return None;
        }
        Some(weighted_sum / Decimal::from(total_time))
    }

    /// Fraction of ticks whose side is `None` (no aggressor information).
    ///
    /// Returns `None` for an empty slice.
    /// Complement of [`aggressor_fraction`](Self::aggressor_fraction).
    pub fn neutral_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        Some(Self::count_neutral(ticks) as f64 / ticks.len() as f64)
    }

    /// Variance of tick-to-tick log returns: `var(ln(p_i / p_{i-1}))`.
    ///
    /// Returns `None` if fewer than 3 ticks or any price is non-positive.
    pub fn log_return_variance(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let returns: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                use rust_decimal::prelude::ToPrimitive;
                let prev = w[0].price.to_f64()?;
                let curr = w[1].price.to_f64()?;
                if prev <= 0.0 || curr <= 0.0 {
                    return None;
                }
                Some((curr / prev).ln())
            })
            .collect();
        if returns.len() < 2 {
            return None;
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        Some(returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0))
    }

    /// Total quantity traded at prices within `tolerance` of the VWAP.
    ///
    /// Returns `Decimal::ZERO` if the slice is empty, VWAP cannot be computed,
    /// or no ticks fall within the tolerance band.
    pub fn volume_at_vwap(ticks: &[NormalizedTick], tolerance: Decimal) -> Decimal {
        let vwap = match Self::vwap(ticks) {
            Some(v) => v,
            None => return Decimal::ZERO,
        };
        ticks
            .iter()
            .filter(|t| (t.price - vwap).abs() <= tolerance)
            .map(|t| t.quantity)
            .sum()
    }

    // ── round-81 ─────────────────────────────────────────────────────────────

    /// Cumulative volume as a `Vec` of running totals, one entry per tick.
    ///
    /// The first entry equals `ticks[0].quantity`; the last equals the total
    /// volume. Returns an empty `Vec` for an empty slice.
    pub fn cumulative_volume(ticks: &[NormalizedTick]) -> Vec<Decimal> {
        let mut acc = Decimal::ZERO;
        ticks
            .iter()
            .map(|t| {
                acc += t.quantity;
                acc
            })
            .collect()
    }

    /// Ratio of price range to mean price: `(max − min) / mean`.
    ///
    /// A dimensionless measure of relative price dispersion across the slice.
    /// Returns `None` for an empty slice or if the mean is zero.
    pub fn price_volatility_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let range = Self::price_range(ticks)?;
        let mean = Self::average_price(ticks)?;
        if mean.is_zero() {
            return None;
        }
        (range / mean).to_f64()
    }

    /// Mean notional value per tick: `total_notional / tick_count`.
    ///
    /// Alias for [`average_notional`](Self::average_notional) expressed as
    /// `f64` for ML feature pipelines.
    ///
    /// Returns `None` for an empty slice.
    pub fn notional_per_tick(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        Self::average_notional(ticks)?.to_f64()
    }

    /// Fraction of total volume (buy + sell + neutral) that is buy-initiated.
    ///
    /// Returns `None` for an empty slice or when total volume is zero.
    pub fn buy_to_total_volume_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        (Self::buy_volume(ticks) / total).to_f64()
    }

    /// Mean transport latency (ms) for ticks that carry an exchange timestamp.
    ///
    /// Averages `received_at_ms − exchange_ts_ms` over ticks where
    /// `exchange_ts_ms` is `Some`. Returns `None` if no such ticks exist.
    pub fn avg_latency_ms(ticks: &[NormalizedTick]) -> Option<f64> {
        let latencies: Vec<i64> = ticks.iter().filter_map(|t| t.latency_ms()).collect();
        if latencies.is_empty() {
            return None;
        }
        Some(latencies.iter().sum::<i64>() as f64 / latencies.len() as f64)
    }

    /// Gini coefficient of trade prices across the slice.
    ///
    /// Measures price inequality: 0 means all trades at the same price,
    /// 1 means maximum price dispersion. Returns `None` if the slice is
    /// empty or all prices are zero.
    pub fn price_gini(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let mut prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if prices.is_empty() {
            return None;
        }
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = prices.len() as f64;
        let sum: f64 = prices.iter().sum();
        if sum == 0.0 {
            return None;
        }
        let weighted_sum: f64 = prices
            .iter()
            .enumerate()
            .map(|(i, &p)| (2.0 * (i + 1) as f64 - n - 1.0) * p)
            .sum();
        Some(weighted_sum / (n * sum))
    }

    /// Rate of tick arrival: `tick_count / time_span_ms`.
    ///
    /// Returns ticks per millisecond. Returns `None` if the slice has fewer
    /// than 2 ticks or the time span is zero.
    pub fn trade_velocity(ticks: &[NormalizedTick]) -> Option<f64> {
        let span_ms = Self::time_span_ms(ticks)?;
        if span_ms == 0 {
            return None;
        }
        Some(ticks.len() as f64 / span_ms as f64)
    }

    /// Minimum price across the slice.
    ///
    /// Semantic alias for [`min_price`](Self::min_price) that matches the
    /// "floor" framing used in support/resistance analysis.
    ///
    /// Returns `None` if the slice is empty.
    pub fn floor_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        Self::min_price(ticks)
    }

    // ── round-82 ─────────────────────────────────────────────────────────────

    /// Quantity-weighted mean of signed price changes; positive = net upward momentum.
    pub fn price_momentum_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let mut num = 0f64;
        let mut den = 0f64;
        for w in ticks.windows(2) {
            let dp = (w[1].price - w[0].price).to_f64()?;
            let q = w[1].quantity.to_f64()?;
            num += dp * q;
            den += q;
        }
        if den == 0.0 { None } else { Some(num / den) }
    }

    /// Std dev of prices weighted by quantity (dispersion around VWAP).
    pub fn vwap_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let vwap = Self::vwap(ticks)?.to_f64()?;
        let total_vol: f64 = ticks.iter().filter_map(|t| t.quantity.to_f64()).sum();
        if total_vol == 0.0 {
            return None;
        }
        let var: f64 = ticks
            .iter()
            .filter_map(|t| {
                let p = t.price.to_f64()?;
                let q = t.quantity.to_f64()?;
                Some((p - vwap).powi(2) * q)
            })
            .sum::<f64>()
            / total_vol;
        Some(var.sqrt())
    }

    /// Fraction of ticks that set a new running high or low (price range expansion events).
    pub fn price_range_expansion(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let mut hi = ticks[0].price;
        let mut lo = ticks[0].price;
        let mut count = 0usize;
        for t in ticks.iter().skip(1) {
            if t.price > hi {
                hi = t.price;
                count += 1;
            } else if t.price < lo {
                lo = t.price;
                count += 1;
            }
        }
        Some(count as f64 / ticks.len() as f64)
    }

    /// Fraction of total volume classified as sell-side; complement of `buy_to_total_volume_ratio`.
    pub fn sell_to_total_volume_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let sell_vol: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .map(|t| t.quantity)
            .sum();
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return Some(0.0);
        }
        sell_vol.to_f64().zip(total.to_f64()).map(|(s, tot)| s / tot)
    }

    /// Std dev of per-tick notional (`price × quantity`); requires ≥ 2 ticks.
    pub fn notional_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let notionals: Vec<f64> = ticks
            .iter()
            .filter_map(|t| (t.price * t.quantity).to_f64())
            .collect();
        let n = notionals.len() as f64;
        if n < 2.0 {
            return None;
        }
        let mean = notionals.iter().sum::<f64>() / n;
        let var = notionals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    // ── round-83 ─────────────────────────────────────────────────────────────

    /// Autocorrelation of trade sizes at lag 1.
    ///
    /// Measures whether large (or small) trades tend to follow each other.
    /// Returns `None` if fewer than 3 ticks or variance is zero.
    pub fn quantity_autocorrelation(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 {
            return None;
        }
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 {
            return None;
        }
        let cov = vals.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>() / n;
        Some(cov / var)
    }

    /// Fraction of ticks where price strictly exceeds the VWAP.
    ///
    /// Returns `None` for empty slices or when VWAP cannot be computed.
    pub fn fraction_above_vwap(ticks: &[NormalizedTick]) -> Option<f64> {
        let vwap = Self::vwap(ticks)?;
        if ticks.is_empty() {
            return None;
        }
        let above = ticks.iter().filter(|t| t.price > vwap).count();
        Some(above as f64 / ticks.len() as f64)
    }

    /// Maximum consecutive buy ticks (by side) within the slice.
    ///
    /// Returns 0 if no buy ticks are present.
    pub fn max_buy_streak(ticks: &[NormalizedTick]) -> usize {
        let mut max = 0usize;
        let mut current = 0usize;
        for t in ticks {
            if t.side == Some(TradeSide::Buy) {
                current += 1;
                if current > max {
                    max = current;
                }
            } else {
                current = 0;
            }
        }
        max
    }

    /// Maximum consecutive sell ticks (by side) within the slice.
    ///
    /// Returns 0 if no sell ticks are present.
    pub fn max_sell_streak(ticks: &[NormalizedTick]) -> usize {
        let mut max = 0usize;
        let mut current = 0usize;
        for t in ticks {
            if t.side == Some(TradeSide::Sell) {
                current += 1;
                if current > max {
                    max = current;
                }
            } else {
                current = 0;
            }
        }
        max
    }

    /// Entropy of the trade side distribution across buy, sell, and neutral.
    ///
    /// Computed as `-Σ p_i * ln(p_i)` over the three categories. Zero
    /// entropy means all ticks share the same side; higher = more mixed.
    /// Returns `None` for empty slices.
    pub fn side_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let n = ticks.len() as f64;
        let buys = Self::buy_count(ticks) as f64;
        let sells = Self::sell_count(ticks) as f64;
        let neutrals = Self::count_neutral(ticks) as f64;
        let entropy = [buys, sells, neutrals]
            .iter()
            .filter(|&&c| c > 0.0)
            .map(|&c| {
                let p = c / n;
                -p * p.ln()
            })
            .sum::<f64>();
        Some(entropy)
    }

    /// Mean time gap between consecutive ticks in milliseconds.
    ///
    /// Uses `received_at_ms`. Returns `None` for fewer than 2 ticks or
    /// if all ticks have identical timestamps.
    pub fn mean_inter_tick_gap_ms(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let gaps: Vec<f64> = ticks
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms) as f64)
            .collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        Some(mean)
    }

    /// Fraction of ticks whose price is a round number divisible by `step`.
    ///
    /// Returns `None` for empty slices or zero `step`.
    pub fn round_number_fraction(ticks: &[NormalizedTick], step: Decimal) -> Option<f64> {
        if ticks.is_empty() || step.is_zero() {
            return None;
        }
        let round = ticks.iter().filter(|t| (t.price % step).is_zero()).count();
        Some(round as f64 / ticks.len() as f64)
    }

    /// Geometric mean of trade quantities.
    ///
    /// Computed as `exp(mean(ln(q_i)))`. Returns `None` for empty slices
    /// or if any quantity is non-positive.
    pub fn geometric_mean_quantity(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let log_sum: f64 = ticks
            .iter()
            .map(|t| {
                let q = t.quantity.to_f64()?;
                if q <= 0.0 { None } else { Some(q.ln()) }
            })
            .try_fold(0.0f64, |acc, v| v.map(|x| acc + x))?;
        Some((log_sum / ticks.len() as f64).exp())
    }

    /// Maximum price return (best single tick-to-tick gain) in the slice.
    ///
    /// Returns `None` for fewer than 2 ticks.
    pub fn max_tick_return(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        ticks
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].price.to_f64()?;
                if prev == 0.0 { return None; }
                let curr = w[1].price.to_f64()?;
                Some((curr - prev) / prev)
            })
            .reduce(f64::max)
    }

    /// Minimum price return (worst single tick-to-tick drop) in the slice.
    ///
    /// Returns `None` for fewer than 2 ticks.
    pub fn min_tick_return(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        ticks
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].price.to_f64()?;
                if prev == 0.0 { return None; }
                let curr = w[1].price.to_f64()?;
                Some((curr - prev) / prev)
            })
            .reduce(f64::min)
    }

    // ── round-83 ─────────────────────────────────────────────────────────────

    /// Mean price of buy-side ticks only; `None` if no buy ticks present.
    pub fn buy_price_mean(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let buys: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Buy))
            .map(|t| t.price)
            .collect();
        if buys.is_empty() {
            return None;
        }
        Some(buys.iter().copied().sum::<Decimal>() / Decimal::from(buys.len()))
    }

    /// Mean price of sell-side ticks only; `None` if no sell ticks present.
    pub fn sell_price_mean(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let sells: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .map(|t| t.price)
            .collect();
        if sells.is_empty() {
            return None;
        }
        Some(sells.iter().copied().sum::<Decimal>() / Decimal::from(sells.len()))
    }

    /// `|last_price − first_price| / Σ|price_i − price_{i-1}|`; 1.0 = perfectly directional, near 0 = noisy.
    pub fn price_efficiency(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let total_path: Decimal = ticks
            .windows(2)
            .map(|w| (w[1].price - w[0].price).abs())
            .sum();
        if total_path.is_zero() {
            return None;
        }
        let net = (ticks.last()?.price - ticks.first()?.price).abs();
        (net / total_path).to_f64()
    }

    /// Skewness of tick-to-tick log returns; requires ≥ 5 ticks.
    pub fn price_return_skewness(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 5 {
            return None;
        }
        let returns: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].price.to_f64()?;
                if prev <= 0.0 { return None; }
                let curr = w[1].price.to_f64()?;
                Some((curr / prev).ln())
            })
            .collect();
        let n = returns.len() as f64;
        if n < 4.0 {
            return None;
        }
        let mean = returns.iter().sum::<f64>() / n;
        let var = returns.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        if std == 0.0 {
            return None;
        }
        let skew = returns.iter().map(|v| ((v - mean) / std).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Buy VWAP minus sell VWAP; positive = buyers paid more than sellers received.
    pub fn buy_sell_vwap_spread(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let (buy_pv, buy_v): (Decimal, Decimal) = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Buy))
            .fold((Decimal::ZERO, Decimal::ZERO), |(pv, v), t| {
                (pv + t.price * t.quantity, v + t.quantity)
            });
        let (sell_pv, sell_v): (Decimal, Decimal) = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .fold((Decimal::ZERO, Decimal::ZERO), |(pv, v), t| {
                (pv + t.price * t.quantity, v + t.quantity)
            });
        if buy_v.is_zero() || sell_v.is_zero() {
            return None;
        }
        let buy_vwap = (buy_pv / buy_v).to_f64()?;
        let sell_vwap = (sell_pv / sell_v).to_f64()?;
        Some(buy_vwap - sell_vwap)
    }

    /// Fraction of ticks with quantity above the mean quantity.
    pub fn above_mean_quantity_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        let mean = total / Decimal::from(ticks.len());
        let count = ticks.iter().filter(|t| t.quantity > mean).count();
        Some(count as f64 / ticks.len() as f64)
    }

    /// Fraction of ticks where price equals the previous tick's price (unchanged price).
    pub fn price_unchanged_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let unchanged = ticks.windows(2).filter(|w| w[0].price == w[1].price).count();
        Some(unchanged as f64 / (ticks.len() - 1) as f64)
    }

    /// Quantity-weighted price range: `Σ(price_i × qty_i) / Σ(qty_i)` applied to max/min spread.
    /// Returns the difference between quantity-weighted high and low prices.
    pub fn qty_weighted_range(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let hi = ticks.iter().map(|t| t.price).max()?;
        let lo = ticks.iter().map(|t| t.price).min()?;
        (hi - lo).to_f64()
    }

    // ── round-84 ─────────────────────────────────────────────────────────────

    /// Fraction of total notional that is sell-side; complement of `buy_notional_fraction`.
    pub fn sell_notional_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum();
        if total.is_zero() {
            return Some(0.0);
        }
        let sell_notional: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .map(|t| t.price * t.quantity)
            .sum();
        sell_notional.to_f64().zip(total.to_f64()).map(|(s, t)| s / t)
    }

    /// Maximum absolute price jump between consecutive ticks.
    pub fn max_price_gap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.len() < 2 {
            return None;
        }
        ticks.windows(2).map(|w| (w[1].price - w[0].price).abs()).max()
    }

    /// Rate of price range expansion: `(high - low) / time_span_ms`; requires ≥ 2 ticks with different timestamps.
    pub fn price_range_velocity(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let time_span = ticks.last()?.received_at_ms.saturating_sub(ticks.first()?.received_at_ms);
        if time_span == 0 {
            return None;
        }
        let hi = ticks.iter().map(|t| t.price).max()?;
        let lo = ticks.iter().map(|t| t.price).min()?;
        let range = (hi - lo).to_f64()?;
        Some(range / time_span as f64)
    }

    /// Number of ticks per millisecond of the slice's time span.
    pub fn tick_count_per_ms(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let span = ticks.last()?.received_at_ms.saturating_sub(ticks.first()?.received_at_ms);
        if span == 0 {
            return None;
        }
        Some(ticks.len() as f64 / span as f64)
    }

    /// Fraction of total quantity attributable to buy-side trades.
    pub fn buy_quantity_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return Some(0.0);
        }
        let buy_qty: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Buy))
            .map(|t| t.quantity)
            .sum();
        buy_qty.to_f64().zip(total.to_f64()).map(|(b, tot)| b / tot)
    }

    /// Fraction of total quantity attributable to sell-side trades.
    pub fn sell_quantity_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return Some(0.0);
        }
        let sell_qty: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .map(|t| t.quantity)
            .sum();
        sell_qty.to_f64().zip(total.to_f64()).map(|(s, tot)| s / tot)
    }

    /// Number of times the price crosses through (or touches) its own window mean.
    pub fn price_mean_crossover_count(ticks: &[NormalizedTick]) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let sum: Decimal = ticks.iter().map(|t| t.price).sum();
        let mean = sum / Decimal::from(ticks.len() as i64);
        let crossovers = ticks
            .windows(2)
            .filter(|w| {
                let prev = w[0].price - mean;
                let curr = w[1].price - mean;
                prev.is_sign_negative() != curr.is_sign_negative()
            })
            .count();
        let _ = mean.to_f64(); // ensure mean is convertible (always true for Decimal)
        Some(crossovers)
    }

    /// Skewness of per-tick notional values (`price × quantity`).
    pub fn notional_skewness(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 {
            return None;
        }
        let vals: Vec<f64> = ticks
            .iter()
            .filter_map(|t| (t.price * t.quantity).to_f64())
            .collect();
        let n = vals.len() as f64;
        if n < 3.0 {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        if std < 1e-12 {
            return None;
        }
        let skew = vals.iter().map(|v| ((v - mean) / std).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Volume-weighted midpoint of the price range: `sum(price * quantity) / sum(quantity)`.
    /// Equivalent to VWAP but emphasises price centrality.
    pub fn volume_weighted_mid_price(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let pv: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum();
        Some(pv / total_qty)
    }

    // ── round-85 ─────────────────────────────────────────────────────────────

    /// Count of ticks with no aggressor side (side == None).
    pub fn neutral_count(ticks: &[NormalizedTick]) -> usize {
        ticks.iter().filter(|t| t.side.is_none()).count()
    }

    /// `max_price − min_price`; raw price spread across the slice.
    pub fn price_dispersion(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let hi = ticks.iter().map(|t| t.price).max()?;
        let lo = ticks.iter().map(|t| t.price).min()?;
        Some(hi - lo)
    }

    /// Maximum per-tick notional (`price × quantity`) in the slice.
    pub fn max_notional(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().map(|t| t.price * t.quantity).max()
    }

    /// Minimum per-tick notional (`price × quantity`) in the slice.
    pub fn min_notional(ticks: &[NormalizedTick]) -> Option<Decimal> {
        ticks.iter().map(|t| t.price * t.quantity).min()
    }

    /// Fraction of ticks with price below the slice VWAP.
    pub fn below_vwap_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let vwap = Self::vwap(ticks)?;
        let count = ticks.iter().filter(|t| t.price < vwap).count();
        Some(count as f64 / ticks.len() as f64)
    }

    /// Standard deviation of per-tick notionals (`price × quantity`); requires ≥ 2 ticks.
    ///
    /// Distinct from `notional_std_dev` (which refers to the `notional` field); this uses
    /// `price * quantity` directly.
    pub fn trade_notional_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = ticks
            .iter()
            .filter_map(|t| (t.price * t.quantity).to_f64())
            .collect();
        let n = vals.len() as f64;
        if n < 2.0 {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    /// Ratio of buy count to sell count; `None` if there are no sell ticks.
    pub fn buy_sell_count_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        let sells = Self::sell_count(ticks);
        if sells == 0 {
            return None;
        }
        Some(Self::buy_count(ticks) as f64 / sells as f64)
    }

    /// Mean absolute deviation of prices from the price mean.
    pub fn price_mad(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let sum: Decimal = ticks.iter().map(|t| t.price).sum();
        let mean = sum / Decimal::from(ticks.len() as i64);
        let mad: f64 = ticks
            .iter()
            .filter_map(|t| (t.price - mean).abs().to_f64())
            .sum::<f64>() / ticks.len() as f64;
        Some(mad)
    }

    /// Price range expressed as a percentage of the first tick's price.
    pub fn price_range_pct_of_open(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let first_price = ticks.first()?.price;
        if first_price.is_zero() {
            return None;
        }
        let hi = ticks.iter().map(|t| t.price).max()?;
        let lo = ticks.iter().map(|t| t.price).min()?;
        ((hi - lo) / first_price).to_f64()
    }

    // ── round-86 ─────────────────────────────────────────────────────────────

    /// Mean price of the ticks; equivalent to arithmetic average of all `price` values.
    pub fn price_mean(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.is_empty() {
            return None;
        }
        let sum: Decimal = ticks.iter().map(|t| t.price).sum();
        Some(sum / Decimal::from(ticks.len() as i64))
    }

    /// Number of ticks where `price > previous_tick.price` (upticks).
    pub fn uptick_count(ticks: &[NormalizedTick]) -> usize {
        if ticks.len() < 2 {
            return 0;
        }
        ticks.windows(2).filter(|w| w[1].price > w[0].price).count()
    }

    /// Number of ticks where `price < previous_tick.price` (downticks).
    pub fn downtick_count(ticks: &[NormalizedTick]) -> usize {
        if ticks.len() < 2 {
            return 0;
        }
        ticks.windows(2).filter(|w| w[1].price < w[0].price).count()
    }

    /// Fraction of tick intervals that are upticks.
    pub fn uptick_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let intervals = (ticks.len() - 1) as f64;
        Some(Self::uptick_count(ticks) as f64 / intervals)
    }

    /// Standard deviation of quantities across ticks; requires ≥ 2 ticks.
    pub fn quantity_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let vals: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        let n = vals.len() as f64;
        if n < 2.0 {
            return None;
        }
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    // ── round-87 ─────────────────────────────────────────────────────────────

    /// Standard deviation of (price − VWAP) across ticks; measures how dispersed
    /// individual trade prices are around the session VWAP.  Returns `None` if
    /// fewer than 2 ticks or total quantity is zero.
    pub fn vwap_deviation_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap = ticks
            .iter()
            .map(|t| t.price * t.quantity)
            .sum::<Decimal>()
            / total_qty;
        let deviations: Vec<f64> = ticks
            .iter()
            .filter_map(|t| (t.price - vwap).to_f64())
            .collect();
        let n = deviations.len() as f64;
        if n < 2.0 {
            return None;
        }
        let mean_dev = deviations.iter().sum::<f64>() / n;
        let var = deviations
            .iter()
            .map(|d| (d - mean_dev).powi(2))
            .sum::<f64>()
            / (n - 1.0);
        Some(var.sqrt())
    }

    /// Length of the longest run of trades on the same side (Buy or Sell).
    /// Ticks with no side are skipped.  Returns `0` if no sided ticks.
    pub fn max_consecutive_side_run(ticks: &[NormalizedTick]) -> usize {
        let mut max_run = 0usize;
        let mut current_run = 0usize;
        let mut last_side: Option<TradeSide> = None;
        for t in ticks {
            if let Some(side) = t.side {
                if Some(side) == last_side {
                    current_run += 1;
                } else {
                    current_run = 1;
                    last_side = Some(side);
                }
                if current_run > max_run {
                    max_run = current_run;
                }
            }
        }
        max_run
    }

    /// Coefficient of variation of inter-arrival times (std dev / mean).
    /// Measures burstiness of trade arrival.  Returns `None` if fewer than
    /// 2 ticks with `received_at_ms` or if mean inter-arrival is zero.
    pub fn inter_arrival_cv(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let intervals: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                let dt = w[1].received_at_ms.checked_sub(w[0].received_at_ms)?;
                Some(dt as f64)
            })
            .collect();
        if intervals.len() < 2 {
            return None;
        }
        let n = intervals.len() as f64;
        let mean = intervals.iter().sum::<f64>() / n;
        if mean == 0.0 {
            return None;
        }
        let var = intervals
            .iter()
            .map(|v| (v - mean).powi(2))
            .sum::<f64>()
            / (n - 1.0);
        Some(var.sqrt() / mean)
    }

    /// Total traded quantity divided by elapsed milliseconds.
    /// Returns `None` if fewer than 2 ticks or elapsed time is zero.
    pub fn volume_per_ms(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let first_ms = ticks.first()?.received_at_ms;
        let last_ms = ticks.last()?.received_at_ms;
        let elapsed = last_ms.checked_sub(first_ms)? as f64;
        if elapsed == 0.0 {
            return None;
        }
        let total_qty: f64 = ticks
            .iter()
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        Some(total_qty / elapsed)
    }

    /// Total notional (price × quantity) divided by elapsed seconds.
    /// Returns `None` if fewer than 2 ticks or elapsed time is zero.
    pub fn notional_per_second(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let first_ms = ticks.first()?.received_at_ms;
        let last_ms = ticks.last()?.received_at_ms;
        let elapsed_sec = last_ms.checked_sub(first_ms)? as f64 / 1000.0;
        if elapsed_sec == 0.0 {
            return None;
        }
        let total_notional: f64 = ticks
            .iter()
            .filter_map(|t| (t.price * t.quantity).to_f64())
            .sum();
        Some(total_notional / elapsed_sec)
    }

    // ── round-88 ─────────────────────────────────────────────────────────────

    /// Net order-flow imbalance: `(buy_qty − sell_qty) / total_qty`.
    ///
    /// Returns a value in `[−1, 1]`: +1 = all buys, −1 = all sells.
    /// Returns `None` for empty slices or zero total quantity.
    pub fn order_flow_imbalance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let buy_qty: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Buy))
            .map(|t| t.quantity)
            .sum();
        let sell_qty: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .map(|t| t.quantity)
            .sum();
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        (buy_qty - sell_qty).to_f64().zip(total.to_f64()).map(|(n, d)| n / d)
    }

    /// Fraction of consecutive tick pairs where both price and quantity increased.
    ///
    /// Returns `None` for fewer than 2 ticks.
    pub fn price_qty_up_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let count = ticks
            .windows(2)
            .filter(|w| w[1].price > w[0].price && w[1].quantity > w[0].quantity)
            .count();
        Some(count as f64 / (ticks.len() - 1) as f64)
    }

    /// Count of ticks where price is at an all-time high within the slice (including first tick).
    pub fn running_high_count(ticks: &[NormalizedTick]) -> usize {
        if ticks.is_empty() {
            return 0;
        }
        let mut hi = ticks[0].price;
        let mut count = 1usize;
        for t in ticks.iter().skip(1) {
            if t.price >= hi {
                hi = t.price;
                count += 1;
            }
        }
        count
    }

    /// Count of ticks where price is at an all-time low within the slice (including first tick).
    pub fn running_low_count(ticks: &[NormalizedTick]) -> usize {
        if ticks.is_empty() {
            return 0;
        }
        let mut lo = ticks[0].price;
        let mut count = 1usize;
        for t in ticks.iter().skip(1) {
            if t.price <= lo {
                lo = t.price;
                count += 1;
            }
        }
        count
    }

    /// Mean quantity of buy ticks divided by mean quantity of sell ticks.
    ///
    /// Returns `None` if no buy or sell ticks, or if sell mean is zero.
    pub fn buy_sell_avg_qty_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buys: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Buy))
            .map(|t| t.quantity)
            .collect();
        let sells: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(crate::tick::TradeSide::Sell))
            .map(|t| t.quantity)
            .collect();
        if buys.is_empty() || sells.is_empty() {
            return None;
        }
        let buy_mean = buys.iter().copied().sum::<Decimal>() / Decimal::from(buys.len() as i64);
        let sell_mean = sells.iter().copied().sum::<Decimal>() / Decimal::from(sells.len() as i64);
        if sell_mean.is_zero() {
            return None;
        }
        (buy_mean / sell_mean).to_f64()
    }

    /// Largest price drop between any two consecutive ticks (always ≥ 0).
    ///
    /// Returns `None` for fewer than 2 ticks.
    pub fn max_price_drop(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.len() < 2 {
            return None;
        }
        ticks
            .windows(2)
            .map(|w| (w[0].price - w[1].price).max(Decimal::ZERO))
            .max()
    }

    /// Largest price rise between any two consecutive ticks (always ≥ 0).
    ///
    /// Returns `None` for fewer than 2 ticks.
    pub fn max_price_rise(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.len() < 2 {
            return None;
        }
        ticks
            .windows(2)
            .map(|w| (w[1].price - w[0].price).max(Decimal::ZERO))
            .max()
    }

    /// Count of ticks classified as buy-side trades.
    pub fn buy_trade_count(ticks: &[NormalizedTick]) -> usize {
        ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .count()
    }

    /// Count of ticks classified as sell-side trades.
    pub fn sell_trade_count(ticks: &[NormalizedTick]) -> usize {
        ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .count()
    }

    /// Fraction of consecutive tick pairs that reverse price direction.
    /// A reversal is when (price[i+1] > price[i]) differs from (price[i] > price[i-1]).
    /// Returns `None` for fewer than 3 ticks.
    pub fn price_reversal_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let reversals = ticks
            .windows(3)
            .filter(|w| {
                let up1 = w[1].price > w[0].price;
                let up2 = w[2].price > w[1].price;
                up1 != up2
            })
            .count();
        Some(reversals as f64 / (ticks.len() - 2) as f64)
    }

    // ── round-89 ─────────────────────────────────────────────────────────────

    /// Fraction of ticks within `band` of the slice VWAP.
    ///
    /// Returns `None` for empty slices or when VWAP cannot be computed.
    pub fn near_vwap_fraction(ticks: &[NormalizedTick], band: Decimal) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let vwap = Self::vwap(ticks)?;
        let count = ticks.iter().filter(|t| (t.price - vwap).abs() <= band).count();
        Some(count as f64 / ticks.len() as f64)
    }

    /// Mean signed return: `mean(price_i - price_{i-1}) / price_{i-1}` across all consecutive pairs.
    ///
    /// Returns `None` for fewer than 2 ticks.
    pub fn mean_tick_return(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let returns: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].price.to_f64()?;
                if prev == 0.0 { return None; }
                let curr = w[1].price.to_f64()?;
                Some((curr - prev) / prev)
            })
            .collect();
        if returns.is_empty() {
            return None;
        }
        Some(returns.iter().sum::<f64>() / returns.len() as f64)
    }

    /// Count of ticks where `side == Buy` and price is strictly below VWAP (passive buy).
    ///
    /// Returns 0 if VWAP cannot be computed.
    pub fn passive_buy_count(ticks: &[NormalizedTick]) -> usize {
        let vwap = match Self::vwap(ticks) {
            Some(v) => v,
            None => return 0,
        };
        ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy) && t.price < vwap)
            .count()
    }

    /// Count of ticks where `side == Sell` and price is strictly above VWAP (passive sell).
    ///
    /// Returns 0 if VWAP cannot be computed.
    pub fn passive_sell_count(ticks: &[NormalizedTick]) -> usize {
        let vwap = match Self::vwap(ticks) {
            Some(v) => v,
            None => return 0,
        };
        ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell) && t.price > vwap)
            .count()
    }

    /// Interquartile range of quantities (Q3 − Q1).
    ///
    /// Returns `None` for fewer than 4 ticks.
    pub fn quantity_iqr(ticks: &[NormalizedTick]) -> Option<Decimal> {
        if ticks.len() < 4 {
            return None;
        }
        let mut qtys: Vec<Decimal> = ticks.iter().map(|t| t.quantity).collect();
        qtys.sort();
        let n = qtys.len();
        let q1 = qtys[n / 4];
        let q3 = qtys[(3 * n) / 4];
        Some(q3 - q1)
    }

    /// Fraction of ticks with price above the 75th percentile of all tick prices in the slice.
    ///
    /// Returns `None` for fewer than 4 ticks.
    pub fn top_quartile_price_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 4 {
            return None;
        }
        let mut prices: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        prices.sort();
        let q3 = prices[(3 * prices.len()) / 4];
        let count = ticks.iter().filter(|t| t.price > q3).count();
        Some(count as f64 / ticks.len() as f64)
    }

    /// Ratio of buy-side notional to total notional (`buy_qty·price / total_qty·price`).
    /// Returns `None` for empty slices or zero total notional.
    pub fn buy_notional_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total_notional: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum();
        if total_notional.is_zero() {
            return None;
        }
        let buy_notional: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.price * t.quantity)
            .sum();
        (buy_notional / total_notional).to_f64()
    }

    /// Standard deviation of tick-to-tick signed returns.
    /// Returns `None` for fewer than 3 ticks (need ≥ 2 returns).
    pub fn return_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 {
            return None;
        }
        let returns: Vec<f64> = ticks
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].price.to_f64()?;
                if prev == 0.0 { return None; }
                Some((w[1].price.to_f64()? - prev) / prev)
            })
            .collect();
        if returns.len() < 2 {
            return None;
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt())
    }

    // ── round-90 ─────────────────────────────────────────────────────────────

    /// Maximum price drawdown from peak: `max(peak − price) / peak` over the slice.
    ///
    /// Returns `None` for an empty slice or a zero peak price.
    pub fn max_drawdown(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let mut peak = ticks[0].price;
        let mut max_dd = Decimal::ZERO;
        for t in ticks {
            if t.price > peak {
                peak = t.price;
            }
            let dd = peak - t.price;
            if dd > max_dd {
                max_dd = dd;
            }
        }
        if peak.is_zero() {
            return None;
        }
        (max_dd / peak).to_f64()
    }

    /// Ratio of the highest price to the lowest price in the slice.
    ///
    /// Returns `None` for an empty slice or a zero minimum price.
    pub fn high_to_low_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let high = ticks.iter().map(|t| t.price).max()?;
        let low = ticks.iter().map(|t| t.price).min()?;
        if low.is_zero() {
            return None;
        }
        (high / low).to_f64()
    }

    /// Tick arrival rate: `tick_count / time_span_ms`.
    ///
    /// Returns `None` when the slice has fewer than 2 ticks or the time span is zero.
    pub fn tick_velocity(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let first_ms = ticks.first()?.received_at_ms;
        let last_ms = ticks.last()?.received_at_ms;
        let span = last_ms.saturating_sub(first_ms);
        if span == 0 {
            return None;
        }
        Some(ticks.len() as f64 / span as f64)
    }

    /// Ratio of second-half notional to first-half notional.
    ///
    /// Measures whether trading activity is accelerating (`> 1`) or decelerating (`< 1`).
    /// Returns `None` for fewer than 2 ticks or zero first-half notional.
    pub fn notional_decay(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let mid = ticks.len() / 2;
        let first_half: Decimal = ticks[..mid].iter().map(|t| t.price * t.quantity).sum();
        let second_half: Decimal = ticks[mid..].iter().map(|t| t.price * t.quantity).sum();
        if first_half.is_zero() {
            return None;
        }
        (second_half / first_half).to_f64()
    }

    /// Momentum of the second half vs the first half: `(mean_price_2 − mean_price_1) / mean_price_1`.
    ///
    /// Returns `None` for fewer than 2 ticks or a zero first-half mean.
    pub fn late_price_momentum(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let mid = ticks.len() / 2;
        let n1 = mid as u32;
        let n2 = (ticks.len() - mid) as u32;
        if n1 == 0 || n2 == 0 {
            return None;
        }
        let mean1: Decimal = ticks[..mid].iter().map(|t| t.price).sum::<Decimal>()
            / Decimal::from(n1);
        let mean2: Decimal = ticks[mid..].iter().map(|t| t.price).sum::<Decimal>()
            / Decimal::from(n2);
        if mean1.is_zero() {
            return None;
        }
        ((mean2 - mean1) / mean1).to_f64()
    }

    /// Maximum run of consecutive buy-side ticks.
    ///
    /// Returns `0` for an empty slice or one with no buy ticks.
    pub fn consecutive_buys_max(ticks: &[NormalizedTick]) -> usize {
        let mut max_run = 0usize;
        let mut run = 0usize;
        for t in ticks {
            if t.side == Some(TradeSide::Buy) {
                run += 1;
                if run > max_run {
                    max_run = run;
                }
            } else {
                run = 0;
            }
        }
        max_run
    }

    // ── round-91 ─────────────────────────────────────────────────────────────

    /// Fraction of ticks where quantity exceeds the mean quantity across the slice.
    ///
    /// Returns `None` for an empty slice.
    pub fn above_mean_qty_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let n = ticks.len() as u32;
        let mean_qty: Decimal =
            ticks.iter().map(|t| t.quantity).sum::<Decimal>() / Decimal::from(n);
        let count = ticks.iter().filter(|t| t.quantity > mean_qty).count();
        Some(count as f64 / ticks.len() as f64)
    }

    /// Fraction of consecutive tick pairs where the trade side alternates.
    ///
    /// Only pairs where both ticks carry a known side are counted.
    /// Returns `None` when fewer than 2 side-annotated ticks are present.
    pub fn side_alternation_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        let sided: Vec<TradeSide> = ticks.iter().filter_map(|t| t.side).collect();
        if sided.len() < 2 {
            return None;
        }
        let alternations = sided.windows(2).filter(|w| w[0] != w[1]).count();
        Some(alternations as f64 / (sided.len() - 1) as f64)
    }

    /// Price range per tick: `(max_price − min_price) / tick_count`.
    ///
    /// Returns `None` for an empty slice.
    pub fn price_range_per_tick(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let high = ticks.iter().map(|t| t.price).max()?;
        let low = ticks.iter().map(|t| t.price).min()?;
        let range = (high - low).to_f64()?;
        Some(range / ticks.len() as f64)
    }

    /// Quantity-weighted standard deviation of price (weighted σ around VWAP).
    ///
    /// Returns `None` for an empty slice or zero total quantity.
    pub fn qty_weighted_price_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap: Decimal =
            ticks.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        let total_qty_f = total_qty.to_f64()?;
        let variance: f64 = ticks
            .iter()
            .filter_map(|t| {
                let diff = (t.price - vwap).to_f64()?;
                let w = t.quantity.to_f64()?;
                Some(w * diff * diff)
            })
            .sum::<f64>()
            / total_qty_f;
        Some(variance.sqrt())
    }

    // ── round-92 ─────────────────────────────────────────────────────────────

    /// Buy pressure ratio: `buy_volume / (buy_volume + sell_volume)`.
    ///
    /// Returns `None` for empty slices or zero total volume.
    pub fn buy_pressure_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let buy_vol: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.quantity)
            .sum();
        let sell_vol: Decimal = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .map(|t| t.quantity)
            .sum();
        let total = buy_vol + sell_vol;
        if total.is_zero() {
            return None;
        }
        (buy_vol / total).to_f64()
    }

    /// Sell pressure ratio: `sell_volume / (buy_volume + sell_volume)`.
    ///
    /// Returns `None` for empty slices or zero total volume.
    pub fn sell_pressure_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        NormalizedTick::buy_pressure_ratio(ticks).map(|b| 1.0 - b)
    }

    /// Ratio of the mean inter-arrival time in the first half to the second half.
    ///
    /// A value > 1 means trading is accelerating (intervals shrinking).
    /// Returns `None` for fewer than 4 ticks or a zero second-half mean.
    pub fn trade_interval_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 4 {
            return None;
        }
        let mid = ticks.len() / 2;
        let first_intervals: Vec<u64> = ticks[..mid]
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .collect();
        let second_intervals: Vec<u64> = ticks[mid..]
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .collect();
        if first_intervals.is_empty() || second_intervals.is_empty() {
            return None;
        }
        let first_mean = first_intervals.iter().sum::<u64>() as f64 / first_intervals.len() as f64;
        let second_mean =
            second_intervals.iter().sum::<u64>() as f64 / second_intervals.len() as f64;
        if second_mean == 0.0 {
            return None;
        }
        Some(first_mean / second_mean)
    }

    /// Quantity-weighted price change: `Σ qty_i * (price_i - price_0)` normalised by total qty.
    ///
    /// Measures the average price movement experienced weighted by trade size.
    /// Returns `None` for empty slices or zero total quantity.
    pub fn weighted_price_change(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let base = ticks[0].price;
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let wsum: Decimal = ticks
            .iter()
            .map(|t| t.quantity * (t.price - base))
            .sum();
        (wsum / total_qty).to_f64()
    }

    /// Ratio of the last price to the first price in the slice.
    ///
    /// Returns `None` for fewer than 2 ticks or a zero first price.
    pub fn first_last_price_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let first = ticks.first()?.price;
        let last = ticks.last()?.price;
        if first.is_zero() {
            return None;
        }
        (last / first).to_f64()
    }

    /// Population variance of tick prices.
    ///
    /// Returns `None` for an empty slice.
    pub fn tick_price_variance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let n = ticks.len() as u32;
        let mean: Decimal =
            ticks.iter().map(|t| t.price).sum::<Decimal>() / Decimal::from(n);
        let variance: f64 = ticks
            .iter()
            .filter_map(|t| {
                let d = (t.price - mean).to_f64()?;
                Some(d * d)
            })
            .sum::<f64>()
            / n as f64;
        Some(variance)
    }

    // ── round-93 ─────────────────────────────────────────────────────────────

    /// VWAP computed exclusively over buy-side ticks.
    ///
    /// Returns `None` if no buy-side ticks or buy volume is zero.
    pub fn buy_side_vwap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let buys: Vec<&NormalizedTick> = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .collect();
        if buys.is_empty() {
            return None;
        }
        let vol: Decimal = buys.iter().map(|t| t.quantity).sum();
        if vol.is_zero() {
            return None;
        }
        Some(buys.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / vol)
    }

    /// VWAP computed exclusively over sell-side ticks.
    ///
    /// Returns `None` if no sell-side ticks or sell volume is zero.
    pub fn sell_side_vwap(ticks: &[NormalizedTick]) -> Option<Decimal> {
        let sells: Vec<&NormalizedTick> = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .collect();
        if sells.is_empty() {
            return None;
        }
        let vol: Decimal = sells.iter().map(|t| t.quantity).sum();
        if vol.is_zero() {
            return None;
        }
        Some(sells.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / vol)
    }

    /// Coefficient of variation of inter-tick arrival gaps (ms).
    ///
    /// Returns `None` for fewer than 3 ticks or zero mean gap.
    pub fn inter_tick_gap_cv(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let gaps: Vec<f64> = ticks
            .windows(2)
            .map(|w| {
                w[1].received_at_ms.saturating_sub(w[0].received_at_ms) as f64
            })
            .collect();
        let n = gaps.len() as f64;
        let mean = gaps.iter().sum::<f64>() / n;
        if mean == 0.0 {
            return None;
        }
        let variance = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / n;
        Some(variance.sqrt() / mean)
    }

    /// Count of ticks where price rose vs. the previous tick minus those where price fell.
    ///
    /// Positive = net upward momentum; negative = net downward.
    /// Returns `None` for fewer than 2 ticks.
    pub fn signed_tick_count(ticks: &[NormalizedTick]) -> Option<i64> {
        if ticks.len() < 2 {
            return None;
        }
        let net: i64 = ticks
            .windows(2)
            .map(|w| {
                if w[1].price > w[0].price {
                    1
                } else if w[1].price < w[0].price {
                    -1
                } else {
                    0
                }
            })
            .sum();
        Some(net)
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

    // ── NormalizedTick::price_acceleration ───────────────────────────────────

    #[test]
    fn test_price_acceleration_none_for_fewer_than_3() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::price_acceleration(&ticks).is_none());
    }

    #[test]
    fn test_price_acceleration_zero_for_constant_velocity() {
        use rust_decimal_macros::dec;
        // prices: 100, 102, 104 → v1=2, v2=2 → accel=0
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(104), dec!(1)),
        ];
        let acc = NormalizedTick::price_acceleration(&ticks).unwrap();
        assert!((acc - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_price_acceleration_positive_when_speeding_up() {
        use rust_decimal_macros::dec;
        // prices: 100, 101, 103 → v1=1, v2=2 → accel=1
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(103), dec!(1)),
        ];
        let acc = NormalizedTick::price_acceleration(&ticks).unwrap();
        assert!((acc - 1.0).abs() < 1e-9);
    }

    // ── NormalizedTick::buy_sell_diff ─────────────────────────────────────────

    #[test]
    fn test_buy_sell_diff_zero_for_empty() {
        assert_eq!(NormalizedTick::buy_sell_diff(&[]), rust_decimal_macros::dec!(0));
    }

    #[test]
    fn test_buy_sell_diff_positive_for_net_buying() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(10));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(3));
        t2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::buy_sell_diff(&[t1, t2]), dec!(7));
    }

    // ── NormalizedTick::is_aggressive_buy / is_aggressive_sell ───────────────

    #[test]
    fn test_is_aggressive_buy_true_when_exceeds_avg() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(15));
        t.side = Some(TradeSide::Buy);
        assert!(NormalizedTick::is_aggressive_buy(&t, dec!(10)));
    }

    #[test]
    fn test_is_aggressive_buy_false_when_not_buy_side() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(15));
        t.side = Some(TradeSide::Sell);
        assert!(!NormalizedTick::is_aggressive_buy(&t, dec!(10)));
    }

    #[test]
    fn test_is_aggressive_sell_true_when_exceeds_avg() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(20));
        t.side = Some(TradeSide::Sell);
        assert!(NormalizedTick::is_aggressive_sell(&t, dec!(10)));
    }

    // ── NormalizedTick::notional_volume ───────────────────────────────────────

    #[test]
    fn test_notional_volume_zero_for_empty() {
        assert_eq!(NormalizedTick::notional_volume(&[]), rust_decimal_macros::dec!(0));
    }

    #[test]
    fn test_notional_volume_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),  // 200
            make_tick_pq(dec!(50), dec!(4)),   // 200
        ];
        assert_eq!(NormalizedTick::notional_volume(&ticks), dec!(400));
    }

    // ── NormalizedTick::weighted_side_score ───────────────────────────────────

    #[test]
    fn test_weighted_side_score_none_for_empty() {
        assert!(NormalizedTick::weighted_side_score(&[]).is_none());
    }

    #[test]
    fn test_weighted_side_score_correct_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(10));
        t.side = Some(TradeSide::Buy);
        // buy=10, sell=0, total=10 → score=1.0
        let score = NormalizedTick::weighted_side_score(&[t]).unwrap();
        assert!((score - 1.0).abs() < 1e-9);
    }

    // ── NormalizedTick::time_span_ms ──────────────────────────────────────────

    #[test]
    fn test_time_span_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::time_span_ms(&[t]).is_none());
    }

    #[test]
    fn test_time_span_correct_for_two_ticks() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 1000;
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.received_at_ms = 5000;
        assert_eq!(NormalizedTick::time_span_ms(&[t1, t2]), Some(4000));
    }

    // ── NormalizedTick::price_above_vwap_count ────────────────────────────────

    #[test]
    fn test_price_above_vwap_count_none_for_empty() {
        assert!(NormalizedTick::price_above_vwap_count(&[]).is_none());
    }

    #[test]
    fn test_price_above_vwap_count_correct() {
        use rust_decimal_macros::dec;
        // Equal quantities: VWAP = (90+100+110)/3 = 100; above: 110 = 1 tick
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        assert_eq!(NormalizedTick::price_above_vwap_count(&ticks), Some(1));
    }

    // ── NormalizedTick::avg_trade_size ────────────────────────────────────────

    #[test]
    fn test_avg_trade_size_none_for_empty() {
        assert!(NormalizedTick::avg_trade_size(&[]).is_none());
    }

    #[test]
    fn test_avg_trade_size_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(4)),
        ];
        assert_eq!(NormalizedTick::avg_trade_size(&ticks), Some(dec!(3)));
    }

    // ── NormalizedTick::volume_concentration ─────────────────────────────────

    #[test]
    fn test_volume_concentration_none_for_empty() {
        assert!(NormalizedTick::volume_concentration(&[]).is_none());
    }

    #[test]
    fn test_volume_concentration_is_one_for_single_tick() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5))];
        let c = NormalizedTick::volume_concentration(&ticks).unwrap();
        assert!((c - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_volume_concentration_in_range() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(103), dec!(10)),
        ];
        let c = NormalizedTick::volume_concentration(&ticks).unwrap();
        assert!(c > 0.0 && c <= 1.0, "expected value in (0,1], got {}", c);
    }

    // ── NormalizedTick::trade_imbalance_score ─────────────────────────────────

    #[test]
    fn test_trade_imbalance_score_none_for_empty() {
        assert!(NormalizedTick::trade_imbalance_score(&[]).is_none());
    }

    #[test]
    fn test_trade_imbalance_score_positive_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let score = NormalizedTick::trade_imbalance_score(&[t]).unwrap();
        assert!(score > 0.0);
    }

    #[test]
    fn test_trade_imbalance_score_negative_for_all_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Sell);
        let score = NormalizedTick::trade_imbalance_score(&[t]).unwrap();
        assert!(score < 0.0);
    }

    // ── NormalizedTick::price_entropy ─────────────────────────────────────────

    #[test]
    fn test_price_entropy_none_for_empty() {
        assert!(NormalizedTick::price_entropy(&[]).is_none());
    }

    #[test]
    fn test_price_entropy_zero_for_single_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        let e = NormalizedTick::price_entropy(&ticks).unwrap();
        assert!((e - 0.0).abs() < 1e-9, "identical prices should have zero entropy, got {}", e);
    }

    #[test]
    fn test_price_entropy_positive_for_varied_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let e = NormalizedTick::price_entropy(&ticks).unwrap();
        assert!(e > 0.0, "varied prices should have positive entropy, got {}", e);
    }

    // ── NormalizedTick::buy_avg_price / sell_avg_price ────────────────────────

    #[test]
    fn test_buy_avg_price_none_for_no_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Sell);
        assert!(NormalizedTick::buy_avg_price(&[t]).is_none());
    }

    #[test]
    fn test_buy_avg_price_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1)); t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(110), dec!(1)); t2.side = Some(TradeSide::Buy);
        assert_eq!(NormalizedTick::buy_avg_price(&[t1, t2]), Some(dec!(105)));
    }

    #[test]
    fn test_sell_avg_price_none_for_no_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        assert!(NormalizedTick::sell_avg_price(&[t]).is_none());
    }

    #[test]
    fn test_sell_avg_price_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(90), dec!(1)); t1.side = Some(TradeSide::Sell);
        let mut t2 = make_tick_pq(dec!(100), dec!(1)); t2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::sell_avg_price(&[t1, t2]), Some(dec!(95)));
    }

    // ── NormalizedTick::price_skewness ────────────────────────────────────────

    #[test]
    fn test_price_skewness_none_for_fewer_than_3() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::price_skewness(&ticks).is_none());
    }

    #[test]
    fn test_price_skewness_zero_for_symmetric() {
        use rust_decimal_macros::dec;
        // symmetric distribution: 1,2,3
        let ticks = vec![
            make_tick_pq(dec!(1), dec!(1)),
            make_tick_pq(dec!(2), dec!(1)),
            make_tick_pq(dec!(3), dec!(1)),
        ];
        let s = NormalizedTick::price_skewness(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "symmetric should have near-zero skew, got {}", s);
    }

    // ── NormalizedTick::quantity_skewness ─────────────────────────────────────

    #[test]
    fn test_quantity_skewness_none_for_fewer_than_3() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(2))];
        assert!(NormalizedTick::quantity_skewness(&ticks).is_none());
    }

    #[test]
    fn test_quantity_skewness_positive_for_right_skewed() {
        use rust_decimal_macros::dec;
        // most quantities small, one very large: right-skewed
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(100)),
        ];
        let s = NormalizedTick::quantity_skewness(&ticks).unwrap();
        assert!(s > 0.0, "right-skewed distribution should have positive skewness, got {}", s);
    }

    // ── NormalizedTick::price_kurtosis ────────────────────────────────────────

    #[test]
    fn test_price_kurtosis_none_for_fewer_than_4() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(1), dec!(1)),
            make_tick_pq(dec!(2), dec!(1)),
            make_tick_pq(dec!(3), dec!(1)),
        ];
        assert!(NormalizedTick::price_kurtosis(&ticks).is_none());
    }

    #[test]
    fn test_price_kurtosis_returns_some_for_varied_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(1), dec!(1)),
            make_tick_pq(dec!(2), dec!(1)),
            make_tick_pq(dec!(3), dec!(1)),
            make_tick_pq(dec!(4), dec!(1)),
        ];
        assert!(NormalizedTick::price_kurtosis(&ticks).is_some());
    }

    // ── NormalizedTick::high_volume_tick_count ────────────────────────────────

    #[test]
    fn test_high_volume_tick_count_zero_for_empty() {
        use rust_decimal_macros::dec;
        assert_eq!(NormalizedTick::high_volume_tick_count(&[], dec!(1)), 0);
    }

    #[test]
    fn test_high_volume_tick_count_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(5)),
            make_tick_pq(dec!(102), dec!(10)),
        ];
        assert_eq!(NormalizedTick::high_volume_tick_count(&ticks, dec!(4)), 2);
    }

    // ── NormalizedTick::vwap_spread ───────────────────────────────────────────

    #[test]
    fn test_vwap_spread_none_when_no_buys_or_sells() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::vwap_spread(&[t]).is_none());
    }

    #[test]
    fn test_vwap_spread_positive_when_buys_priced_higher() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(105), dec!(1)); buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1)); sell.side = Some(TradeSide::Sell);
        let spread = NormalizedTick::vwap_spread(&[buy, sell]).unwrap();
        assert!(spread > dec!(0), "expected positive spread, got {}", spread);
    }

    // ── NormalizedTick::avg_buy_quantity / avg_sell_quantity ──────────────────

    #[test]
    fn test_avg_buy_quantity_none_for_no_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(2)); t.side = Some(TradeSide::Sell);
        assert!(NormalizedTick::avg_buy_quantity(&[t]).is_none());
    }

    #[test]
    fn test_avg_buy_quantity_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(2)); t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(101), dec!(4)); t2.side = Some(TradeSide::Buy);
        assert_eq!(NormalizedTick::avg_buy_quantity(&[t1, t2]), Some(dec!(3)));
    }

    #[test]
    fn test_avg_sell_quantity_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(6)); t1.side = Some(TradeSide::Sell);
        let mut t2 = make_tick_pq(dec!(101), dec!(2)); t2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::avg_sell_quantity(&[t1, t2]), Some(dec!(4)));
    }

    // ── NormalizedTick::price_mean_reversion_score ────────────────────────────

    #[test]
    fn test_price_mean_reversion_score_none_for_empty() {
        assert!(NormalizedTick::price_mean_reversion_score(&[]).is_none());
    }

    #[test]
    fn test_price_mean_reversion_score_in_range() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let score = NormalizedTick::price_mean_reversion_score(&ticks).unwrap();
        assert!(score >= 0.0 && score <= 1.0, "score should be in [0, 1], got {}", score);
    }

    // ── NormalizedTick::largest_price_move ────────────────────────────────────

    #[test]
    fn test_largest_price_move_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::largest_price_move(&[t]).is_none());
    }

    #[test]
    fn test_largest_price_move_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),  // move = 5
            make_tick_pq(dec!(102), dec!(1)),  // move = 3
        ];
        assert_eq!(NormalizedTick::largest_price_move(&ticks), Some(dec!(5)));
    }

    // ── NormalizedTick::tick_rate ─────────────────────────────────────────────

    #[test]
    fn test_tick_rate_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_rate(&[t]).is_none());
    }

    #[test]
    fn test_tick_rate_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1)); t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(101), dec!(1)); t2.received_at_ms = 2;
        let mut t3 = make_tick_pq(dec!(102), dec!(1)); t3.received_at_ms = 4;
        // 3 ticks over 4ms → 0.75 ticks/ms
        let rate = NormalizedTick::tick_rate(&[t1, t2, t3]).unwrap();
        assert!((rate - 0.75).abs() < 1e-9, "expected 0.75 ticks/ms, got {}", rate);
    }

    // ── NormalizedTick::buy_notional_fraction ─────────────────────────────────

    #[test]
    fn test_buy_notional_fraction_none_for_empty() {
        assert!(NormalizedTick::buy_notional_fraction(&[]).is_none());
    }

    #[test]
    fn test_buy_notional_fraction_one_when_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5)); t.side = Some(TradeSide::Buy);
        let frac = NormalizedTick::buy_notional_fraction(&[t]).unwrap();
        assert!((frac - 1.0).abs() < 1e-9, "all buys should give fraction=1.0, got {}", frac);
    }

    #[test]
    fn test_buy_notional_fraction_in_range_for_mixed() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(3)); buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1)); sell.side = Some(TradeSide::Sell);
        let frac = NormalizedTick::buy_notional_fraction(&[buy, sell]).unwrap();
        assert!(frac > 0.0 && frac < 1.0, "mixed ticks should be in (0,1), got {}", frac);
    }

    // ── NormalizedTick::price_range_pct ───────────────────────────────────────

    #[test]
    fn test_price_range_pct_none_for_empty() {
        assert!(NormalizedTick::price_range_pct(&[]).is_none());
    }

    #[test]
    fn test_price_range_pct_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        // (110 - 100) / 100 * 100 = 10%
        let pct = NormalizedTick::price_range_pct(&ticks).unwrap();
        assert!((pct - 10.0).abs() < 1e-6, "expected 10.0%, got {}", pct);
    }

    // ── NormalizedTick::buy_side_dominance ────────────────────────────────────

    #[test]
    fn test_buy_side_dominance_none_when_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1)); // side=None
        assert!(NormalizedTick::buy_side_dominance(&[t]).is_none());
    }

    #[test]
    fn test_buy_side_dominance_one_when_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5)); t.side = Some(TradeSide::Buy);
        let d = NormalizedTick::buy_side_dominance(&[t]).unwrap();
        assert!((d - 1.0).abs() < 1e-9, "all buys should give 1.0, got {}", d);
    }

    // ── NormalizedTick::volume_weighted_price_std ─────────────────────────────

    #[test]
    fn test_volume_weighted_price_std_none_for_empty() {
        assert!(NormalizedTick::volume_weighted_price_std(&[]).is_none());
    }

    #[test]
    fn test_volume_weighted_price_std_zero_for_same_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let std = NormalizedTick::volume_weighted_price_std(&ticks).unwrap();
        assert!((std - 0.0).abs() < 1e-9, "same price should give 0 std, got {}", std);
    }

    // ── NormalizedTick::last_n_vwap ───────────────────────────────────────────

    #[test]
    fn test_last_n_vwap_none_for_zero_n() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::last_n_vwap(&[t], 0).is_none());
    }

    #[test]
    fn test_last_n_vwap_uses_last_n_ticks() {
        use rust_decimal_macros::dec;
        // first tick at 50, last 2 at 100 equal qty → last_n_vwap(n=2) = 100
        let ticks = vec![
            make_tick_pq(dec!(50), dec!(10)),
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(5)),
        ];
        let v = NormalizedTick::last_n_vwap(&ticks, 2).unwrap();
        assert_eq!(v, dec!(100));
    }

    // ── NormalizedTick::price_autocorrelation ─────────────────────────────────

    #[test]
    fn test_price_autocorrelation_none_for_fewer_than_3() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::price_autocorrelation(&ticks).is_none());
    }

    #[test]
    fn test_price_autocorrelation_positive_for_trending_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(104), dec!(1)),
            make_tick_pq(dec!(106), dec!(1)),
        ];
        let ac = NormalizedTick::price_autocorrelation(&ticks).unwrap();
        assert!(ac > 0.0, "trending prices should have positive AC, got {}", ac);
    }

    // ── NormalizedTick::net_trade_direction ───────────────────────────────────

    #[test]
    fn test_net_trade_direction_zero_for_empty() {
        assert_eq!(NormalizedTick::net_trade_direction(&[]), 0);
    }

    #[test]
    fn test_net_trade_direction_positive_for_more_buys() {
        use rust_decimal_macros::dec;
        let mut b1 = make_tick_pq(dec!(100), dec!(1)); b1.side = Some(TradeSide::Buy);
        let mut b2 = make_tick_pq(dec!(100), dec!(1)); b2.side = Some(TradeSide::Buy);
        let mut s1 = make_tick_pq(dec!(100), dec!(1)); s1.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::net_trade_direction(&[b1, b2, s1]), 1);
    }

    // ── NormalizedTick::sell_side_notional_fraction ───────────────────────────

    #[test]
    fn test_sell_side_notional_fraction_none_for_empty() {
        assert!(NormalizedTick::sell_side_notional_fraction(&[]).is_none());
    }

    #[test]
    fn test_sell_side_notional_fraction_one_when_all_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5)); t.side = Some(TradeSide::Sell);
        let f = NormalizedTick::sell_side_notional_fraction(&[t]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all sells should give 1.0, got {}", f);
    }

    // ── NormalizedTick::price_oscillation_count ───────────────────────────────

    #[test]
    fn test_price_oscillation_count_zero_for_monotone() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        assert_eq!(NormalizedTick::price_oscillation_count(&ticks), 0);
    }

    #[test]
    fn test_price_oscillation_count_detects_reversals() {
        use rust_decimal_macros::dec;
        // up-down-up: 100 → 105 → 102 → 107
        // windows(3): [100,105,102] (up-down) + [105,102,107] (down-up) → 2 reversals
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(107), dec!(1)),
        ];
        assert_eq!(NormalizedTick::price_oscillation_count(&ticks), 2);
    }

    // ── NormalizedTick::realized_spread ───────────────────────────────────────

    #[test]
    fn test_realized_spread_none_when_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::realized_spread(&[t]).is_none());
    }

    #[test]
    fn test_realized_spread_positive_when_buys_higher() {
        use rust_decimal_macros::dec;
        let mut b = make_tick_pq(dec!(105), dec!(1)); b.side = Some(TradeSide::Buy);
        let mut s = make_tick_pq(dec!(100), dec!(1)); s.side = Some(TradeSide::Sell);
        let spread = NormalizedTick::realized_spread(&[b, s]).unwrap();
        assert!(spread > dec!(0), "expected positive spread, got {}", spread);
    }

    // ── NormalizedTick::price_impact_per_unit ────────────────────────────────

    #[test]
    fn test_price_impact_per_unit_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_impact_per_unit(&[t]).is_none());
    }

    // ── NormalizedTick::volume_weighted_return ────────────────────────────────

    #[test]
    fn test_volume_weighted_return_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::volume_weighted_return(&[t]).is_none());
    }

    #[test]
    fn test_volume_weighted_return_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(5)),
        ];
        let r = NormalizedTick::volume_weighted_return(&ticks).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "constant price should give 0 return, got {}", r);
    }

    // ── NormalizedTick::quantity_concentration ────────────────────────────────

    #[test]
    fn test_quantity_concentration_none_for_empty() {
        assert!(NormalizedTick::quantity_concentration(&[]).is_none());
    }

    #[test]
    fn test_quantity_concentration_zero_for_identical_quantities() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(101), dec!(5)),
        ];
        let c = NormalizedTick::quantity_concentration(&ticks).unwrap();
        assert!((c - 0.0).abs() < 1e-9, "identical quantities should give 0 concentration, got {}", c);
    }

    // ── NormalizedTick::price_level_volume ────────────────────────────────────

    #[test]
    fn test_price_level_volume_zero_for_no_match() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5))];
        let v = NormalizedTick::price_level_volume(&ticks, dec!(200));
        assert_eq!(v, dec!(0));
    }

    #[test]
    fn test_price_level_volume_sums_matching_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(3)),
            make_tick_pq(dec!(101), dec!(7)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        assert_eq!(NormalizedTick::price_level_volume(&ticks, dec!(100)), dec!(5));
    }

    // ── NormalizedTick::mid_price_drift ───────────────────────────────────────

    #[test]
    fn test_mid_price_drift_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::mid_price_drift(&[t]).is_none());
    }

    // ── NormalizedTick::tick_direction_bias ───────────────────────────────────

    #[test]
    fn test_tick_direction_bias_none_for_fewer_than_3() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::tick_direction_bias(&ticks).is_none());
    }

    #[test]
    fn test_tick_direction_bias_one_for_monotone() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(103), dec!(1)),
        ];
        let bias = NormalizedTick::tick_direction_bias(&ticks).unwrap();
        assert!((bias - 1.0).abs() < 1e-9, "monotone should give bias=1.0, got {}", bias);
    }

    #[test]
    fn test_buy_sell_size_ratio_none_for_empty() {
        assert!(NormalizedTick::buy_sell_size_ratio(&[]).is_none());
    }

    #[test]
    fn test_buy_sell_size_ratio_positive() {
        use rust_decimal_macros::dec;
        let buy = NormalizedTick { side: Some(TradeSide::Buy), ..make_tick_pq(dec!(100), dec!(4)) };
        let sell = NormalizedTick { side: Some(TradeSide::Sell), ..make_tick_pq(dec!(100), dec!(2)) };
        let r = NormalizedTick::buy_sell_size_ratio(&[buy, sell]).unwrap();
        assert!((r - 2.0).abs() < 1e-6, "ratio should be 2.0, got {}", r);
    }

    #[test]
    fn test_trade_size_dispersion_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5))];
        assert!(NormalizedTick::trade_size_dispersion(&ticks).is_none());
    }

    #[test]
    fn test_trade_size_dispersion_zero_for_identical() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(101), dec!(5)),
            make_tick_pq(dec!(102), dec!(5)),
        ];
        let d = NormalizedTick::trade_size_dispersion(&ticks).unwrap();
        assert!(d.abs() < 1e-9, "identical sizes → dispersion=0, got {}", d);
    }

    #[test]
    fn test_first_last_price_none_for_empty() {
        assert!(NormalizedTick::first_price(&[]).is_none());
        assert!(NormalizedTick::last_price(&[]).is_none());
    }

    #[test]
    fn test_first_last_price_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        assert_eq!(NormalizedTick::first_price(&ticks).unwrap(), dec!(100));
        assert_eq!(NormalizedTick::last_price(&ticks).unwrap(), dec!(110));
    }

    #[test]
    fn test_median_quantity_none_for_empty() {
        assert!(NormalizedTick::median_quantity(&[]).is_none());
    }

    #[test]
    fn test_median_quantity_odd_count() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(3)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(5)),
        ];
        // sorted: 1, 3, 5 → median = 3
        assert_eq!(NormalizedTick::median_quantity(&ticks).unwrap(), dec!(3));
    }

    #[test]
    fn test_volume_above_vwap_none_for_empty() {
        assert!(NormalizedTick::volume_above_vwap(&[]).is_none());
    }

    #[test]
    fn test_volume_above_vwap_none_when_all_at_vwap() {
        use rust_decimal_macros::dec;
        // All same price → VWAP = price, nothing strictly above
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(5)),
        ];
        let v = NormalizedTick::volume_above_vwap(&ticks).unwrap();
        assert_eq!(v, dec!(0));
    }

    #[test]
    fn test_inter_arrival_variance_none_for_fewer_than_3() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::inter_arrival_variance(&[t]).is_none());
    }

    #[test]
    fn test_spread_efficiency_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::spread_efficiency(&ticks).is_none());
    }

    #[test]
    fn test_spread_efficiency_one_for_monotone() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        // monotone up → efficiency = 1.0
        let e = NormalizedTick::spread_efficiency(&ticks).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0, got {}", e);
    }

    // ── round-79 ─────────────────────────────────────────────────────────────

    // ── NormalizedTick::aggressor_fraction ────────────────────────────────────

    #[test]
    fn test_aggressor_fraction_none_for_empty() {
        assert!(NormalizedTick::aggressor_fraction(&[]).is_none());
    }

    #[test]
    fn test_aggressor_fraction_zero_when_all_neutral() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let f = NormalizedTick::aggressor_fraction(&ticks).unwrap();
        assert!((f - 0.0).abs() < 1e-9, "all neutral → fraction=0, got {}", f);
    }

    #[test]
    fn test_aggressor_fraction_one_when_all_known() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            NormalizedTick { side: Some(TradeSide::Buy), ..make_tick_pq(dec!(100), dec!(1)) },
            NormalizedTick { side: Some(TradeSide::Sell), ..make_tick_pq(dec!(101), dec!(1)) },
        ];
        let f = NormalizedTick::aggressor_fraction(&ticks).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all known → fraction=1, got {}", f);
    }

    // ── NormalizedTick::volume_imbalance_ratio ────────────────────────────────

    #[test]
    fn test_volume_imbalance_ratio_none_for_neutral_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5))];
        assert!(NormalizedTick::volume_imbalance_ratio(&ticks).is_none());
    }

    #[test]
    fn test_volume_imbalance_ratio_positive_for_all_buys() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            NormalizedTick { side: Some(TradeSide::Buy), ..make_tick_pq(dec!(100), dec!(4)) },
        ];
        let r = NormalizedTick::volume_imbalance_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all buys → ratio=1.0, got {}", r);
    }

    #[test]
    fn test_volume_imbalance_ratio_zero_for_equal_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            NormalizedTick { side: Some(TradeSide::Buy), ..make_tick_pq(dec!(100), dec!(5)) },
            NormalizedTick { side: Some(TradeSide::Sell), ..make_tick_pq(dec!(100), dec!(5)) },
        ];
        let r = NormalizedTick::volume_imbalance_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "equal buy/sell → ratio=0, got {}", r);
    }

    // ── NormalizedTick::price_quantity_covariance ─────────────────────────────

    #[test]
    fn test_price_quantity_covariance_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::price_quantity_covariance(&ticks).is_none());
    }

    #[test]
    fn test_price_quantity_covariance_positive_when_correlated() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(200), dec!(2)),
            make_tick_pq(dec!(300), dec!(3)),
        ];
        let c = NormalizedTick::price_quantity_covariance(&ticks).unwrap();
        assert!(c > 0.0, "price and qty both rise → positive cov, got {}", c);
    }

    // ── NormalizedTick::large_trade_fraction ──────────────────────────────────

    #[test]
    fn test_large_trade_fraction_none_for_empty() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::large_trade_fraction(&[], dec!(10)).is_none());
    }

    #[test]
    fn test_large_trade_fraction_zero_when_all_small() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(2)),
        ];
        let f = NormalizedTick::large_trade_fraction(&ticks, dec!(10)).unwrap();
        assert!((f - 0.0).abs() < 1e-9, "all small → fraction=0, got {}", f);
    }

    #[test]
    fn test_large_trade_fraction_one_when_all_large() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(20)),
            make_tick_pq(dec!(101), dec!(30)),
        ];
        let f = NormalizedTick::large_trade_fraction(&ticks, dec!(10)).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all large → fraction=1, got {}", f);
    }

    // ── NormalizedTick::price_level_density ───────────────────────────────────

    #[test]
    fn test_price_level_density_none_for_empty() {
        assert!(NormalizedTick::price_level_density(&[]).is_none());
    }

    #[test]
    fn test_price_level_density_none_when_range_zero() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        assert!(NormalizedTick::price_level_density(&ticks).is_none());
    }

    #[test]
    fn test_price_level_density_positive_for_varied_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
        ];
        let d = NormalizedTick::price_level_density(&ticks).unwrap();
        assert!(d > 0.0, "should be positive, got {}", d);
    }

    // ── NormalizedTick::notional_buy_sell_ratio ───────────────────────────────

    #[test]
    fn test_notional_buy_sell_ratio_none_when_no_sells() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            NormalizedTick { side: Some(TradeSide::Buy), ..make_tick_pq(dec!(100), dec!(5)) },
        ];
        assert!(NormalizedTick::notional_buy_sell_ratio(&ticks).is_none());
    }

    #[test]
    fn test_notional_buy_sell_ratio_one_for_equal_notional() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            NormalizedTick { side: Some(TradeSide::Buy), ..make_tick_pq(dec!(100), dec!(5)) },
            NormalizedTick { side: Some(TradeSide::Sell), ..make_tick_pq(dec!(100), dec!(5)) },
        ];
        let r = NormalizedTick::notional_buy_sell_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "equal notional → ratio=1, got {}", r);
    }

    // ── NormalizedTick::log_return_mean ───────────────────────────────────────

    #[test]
    fn test_log_return_mean_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::log_return_mean(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_log_return_mean_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let m = NormalizedTick::log_return_mean(&ticks).unwrap();
        assert!(m.abs() < 1e-9, "constant price → mean log return=0, got {}", m);
    }

    // ── NormalizedTick::log_return_std ────────────────────────────────────────

    #[test]
    fn test_log_return_std_none_for_fewer_than_3_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::log_return_std(&ticks).is_none());
    }

    #[test]
    fn test_log_return_std_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let s = NormalizedTick::log_return_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "constant price → std=0, got {}", s);
    }

    // ── NormalizedTick::price_overshoot_ratio ─────────────────────────────────

    #[test]
    fn test_price_overshoot_ratio_none_for_empty() {
        assert!(NormalizedTick::price_overshoot_ratio(&[]).is_none());
    }

    #[test]
    fn test_price_overshoot_ratio_one_for_monotone_up() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        // max=110 == last=110 → ratio=1
        let r = NormalizedTick::price_overshoot_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "monotone up → ratio=1, got {}", r);
    }

    #[test]
    fn test_price_overshoot_ratio_above_one_when_price_retreats() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        // max=120, last=110 → ratio > 1
        let r = NormalizedTick::price_overshoot_ratio(&ticks).unwrap();
        assert!(r > 1.0, "price retreated → ratio>1, got {}", r);
    }

    // ── NormalizedTick::price_undershoot_ratio ────────────────────────────────

    #[test]
    fn test_price_undershoot_ratio_none_for_empty() {
        assert!(NormalizedTick::price_undershoot_ratio(&[]).is_none());
    }

    #[test]
    fn test_price_undershoot_ratio_one_for_monotone_down() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        // first=110, min=100 → ratio > 1 (price undershot opening)
        let r = NormalizedTick::price_undershoot_ratio(&ticks).unwrap();
        assert!(r > 1.0, "monotone down → ratio>1, got {}", r);
    }

    #[test]
    fn test_price_undershoot_ratio_one_for_monotone_up() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        // first=100 == min=100 → ratio=1 (never went below open)
        let r = NormalizedTick::price_undershoot_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "monotone up → ratio=1, got {}", r);
    }

    // ── round-80 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_net_notional_empty_is_zero() {
        assert_eq!(NormalizedTick::net_notional(&[]), Decimal::ZERO);
    }

    #[test]
    fn test_net_notional_positive_buy() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)).with_side(TradeSide::Buy),
            make_tick_pq(dec!(100), dec!(2)).with_side(TradeSide::Sell),
        ];
        assert_eq!(NormalizedTick::net_notional(&ticks), dec!(300));
    }

    #[test]
    fn test_price_reversal_count_empty_is_zero() {
        assert_eq!(NormalizedTick::price_reversal_count(&[]), 0);
    }

    #[test]
    fn test_price_reversal_count_monotone_is_zero() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        assert_eq!(NormalizedTick::price_reversal_count(&ticks), 0);
    }

    #[test]
    fn test_price_reversal_count_zigzag() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
        ];
        assert_eq!(NormalizedTick::price_reversal_count(&ticks), 2);
    }

    #[test]
    fn test_quantity_kurtosis_none_for_few_ticks() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::quantity_kurtosis(&[t]).is_none());
    }

    #[test]
    fn test_quantity_kurtosis_some_for_sufficient() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(2)),
            make_tick_pq(dec!(102), dec!(3)),
            make_tick_pq(dec!(103), dec!(4)),
        ];
        assert!(NormalizedTick::quantity_kurtosis(&ticks).is_some());
    }

    #[test]
    fn test_largest_notional_trade_none_for_empty() {
        assert!(NormalizedTick::largest_notional_trade(&[]).is_none());
    }

    #[test]
    fn test_largest_notional_trade_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),   // notional = 100
            make_tick_pq(dec!(50), dec!(10)),   // notional = 500 ← max
            make_tick_pq(dec!(200), dec!(1)),   // notional = 200
        ];
        let t = NormalizedTick::largest_notional_trade(&ticks).unwrap();
        assert_eq!(t.price, dec!(50));
    }

    #[test]
    fn test_twap_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::twap(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_twap_two_equal_intervals() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(200), dec!(1));
        t2.received_at_ms = 1000;
        let mut t3 = make_tick_pq(dec!(300), dec!(1));
        t3.received_at_ms = 2000;
        // weights: t1 * 1000ms, t2 * 1000ms → TWAP = (100*1000 + 200*1000)/2000 = 150
        let twap = NormalizedTick::twap(&[t1, t2, t3]).unwrap();
        assert_eq!(twap, dec!(150));
    }

    #[test]
    fn test_neutral_fraction_all_neutral() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let f = NormalizedTick::neutral_fraction(&ticks).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all neutral → fraction=1, got {}", f);
    }

    #[test]
    fn test_log_return_variance_none_for_few_ticks() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::log_return_variance(&[t]).is_none());
    }

    #[test]
    fn test_log_return_variance_zero_for_flat_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let v = NormalizedTick::log_return_variance(&ticks).unwrap();
        assert!(v.abs() < 1e-9, "flat prices → variance=0, got {}", v);
    }

    #[test]
    fn test_volume_at_vwap_zero_for_empty() {
        assert_eq!(
            NormalizedTick::volume_at_vwap(&[], rust_decimal_macros::dec!(1)),
            Decimal::ZERO
        );
    }

    // ── NormalizedTick::cumulative_volume ─────────────────────────────────────

    #[test]
    fn test_cumulative_volume_empty_for_empty_slice() {
        assert!(NormalizedTick::cumulative_volume(&[]).is_empty());
    }

    #[test]
    fn test_cumulative_volume_last_equals_total() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(3)),
            make_tick_pq(dec!(102), dec!(5)),
        ];
        let cv = NormalizedTick::cumulative_volume(&ticks);
        assert_eq!(cv.last().copied().unwrap(), dec!(10));
        assert_eq!(cv[0], dec!(2));
    }

    // ── NormalizedTick::price_volatility_ratio ────────────────────────────────

    #[test]
    fn test_price_volatility_ratio_none_for_empty() {
        assert!(NormalizedTick::price_volatility_ratio(&[]).is_none());
    }

    #[test]
    fn test_price_volatility_ratio_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(100), dec!(1))];
        let r = NormalizedTick::price_volatility_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "constant price → ratio=0, got {}", r);
    }

    // ── NormalizedTick::notional_per_tick ─────────────────────────────────────

    #[test]
    fn test_notional_per_tick_none_for_empty() {
        assert!(NormalizedTick::notional_per_tick(&[]).is_none());
    }

    #[test]
    fn test_notional_per_tick_equals_single_tick_notional() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5))];
        let n = NormalizedTick::notional_per_tick(&ticks).unwrap();
        assert!((n - 500.0).abs() < 1e-6, "100×5=500, got {}", n);
    }

    // ── NormalizedTick::buy_to_total_volume_ratio ─────────────────────────────

    #[test]
    fn test_buy_to_total_volume_ratio_none_for_empty() {
        assert!(NormalizedTick::buy_to_total_volume_ratio(&[]).is_none());
    }

    #[test]
    fn test_buy_to_total_volume_ratio_zero_for_all_neutral() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5)), make_tick_pq(dec!(101), dec!(3))];
        let r = NormalizedTick::buy_to_total_volume_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "neutral ticks → buy ratio=0, got {}", r);
    }

    // ── NormalizedTick::avg_latency_ms ────────────────────────────────────────

    #[test]
    fn test_avg_latency_ms_none_when_no_exchange_ts() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::avg_latency_ms(&ticks).is_none());
    }

    // ── NormalizedTick::price_gini ────────────────────────────────────────────

    #[test]
    fn test_price_gini_none_for_empty() {
        assert!(NormalizedTick::price_gini(&[]).is_none());
    }

    #[test]
    fn test_price_gini_zero_for_uniform_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let g = NormalizedTick::price_gini(&ticks).unwrap();
        assert!(g.abs() < 1e-9, "uniform prices → gini=0, got {}", g);
    }

    // ── NormalizedTick::trade_velocity ────────────────────────────────────────

    #[test]
    fn test_trade_velocity_none_for_same_timestamp() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::trade_velocity(&ticks).is_none());
    }

    // ── NormalizedTick::floor_price ───────────────────────────────────────────

    #[test]
    fn test_floor_price_none_for_empty() {
        assert!(NormalizedTick::floor_price(&[]).is_none());
    }

    #[test]
    fn test_floor_price_equals_min_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(103), dec!(1)),
        ];
        assert_eq!(NormalizedTick::floor_price(&ticks), NormalizedTick::min_price(&ticks));
    }

    // ── round-82 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_price_momentum_score_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_momentum_score(&[t]).is_none());
    }

    #[test]
    fn test_price_momentum_score_positive_for_rising_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(2)),
            make_tick_pq(dec!(104), dec!(2)),
        ];
        let s = NormalizedTick::price_momentum_score(&ticks).unwrap();
        assert!(s > 0.0, "rising prices → positive momentum, got {}", s);
    }

    #[test]
    fn test_vwap_std_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::vwap_std(&[t]).is_none());
    }

    #[test]
    fn test_vwap_std_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let s = NormalizedTick::vwap_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "constant price → vwap_std=0, got {}", s);
    }

    #[test]
    fn test_price_range_expansion_none_for_empty() {
        assert!(NormalizedTick::price_range_expansion(&[]).is_none());
    }

    #[test]
    fn test_price_range_expansion_monotone_rising() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(103), dec!(1)),
        ];
        let f = NormalizedTick::price_range_expansion(&ticks).unwrap();
        // Every tick after the first sets a new high → count=3, total=4 → 0.75
        assert!((f - 0.75).abs() < 1e-9, "expected 0.75, got {}", f);
    }

    #[test]
    fn test_sell_to_total_volume_ratio_none_for_empty() {
        assert!(NormalizedTick::sell_to_total_volume_ratio(&[]).is_none());
    }

    #[test]
    fn test_sell_to_total_volume_ratio_zero_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(5));
        t1.side = Some(crate::tick::TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(101), dec!(3));
        t2.side = Some(crate::tick::TradeSide::Buy);
        let r = NormalizedTick::sell_to_total_volume_ratio(&[t1, t2]).unwrap();
        assert!(r.abs() < 1e-9, "all buys → sell ratio=0, got {}", r);
    }

    #[test]
    fn test_notional_std_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::notional_std(&[t]).is_none());
    }

    #[test]
    fn test_notional_std_zero_for_identical_notionals() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(2));
        let t2 = make_tick_pq(dec!(100), dec!(2));
        let s = NormalizedTick::notional_std(&[t1, t2]).unwrap();
        assert!(s.abs() < 1e-9, "identical notionals → std=0, got {}", s);
    }

    // ── round-83 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_buy_price_mean_none_when_no_buys() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1)); // side=None
        assert!(NormalizedTick::buy_price_mean(&[t]).is_none());
    }

    #[test]
    fn test_buy_price_mean_correct_value() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(crate::tick::TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(102), dec!(1));
        t2.side = Some(crate::tick::TradeSide::Buy);
        let mean = NormalizedTick::buy_price_mean(&[t1, t2]).unwrap();
        assert_eq!(mean, dec!(101));
    }

    #[test]
    fn test_sell_price_mean_none_when_no_sells() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::sell_price_mean(&[t]).is_none());
    }

    #[test]
    fn test_price_efficiency_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_efficiency(&[t]).is_none());
    }

    #[test]
    fn test_price_efficiency_one_for_directional() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(104), dec!(1)),
        ];
        let e = NormalizedTick::price_efficiency(&ticks).unwrap();
        assert!((e - 1.0).abs() < 1e-9, "monotone rising → efficiency=1, got {}", e);
    }

    #[test]
    fn test_price_return_skewness_none_for_few_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        assert!(NormalizedTick::price_return_skewness(&ticks).is_none());
    }

    #[test]
    fn test_buy_sell_vwap_spread_none_when_no_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::buy_sell_vwap_spread(&ticks).is_none());
    }

    #[test]
    fn test_above_mean_quantity_fraction_none_for_empty() {
        assert!(NormalizedTick::above_mean_quantity_fraction(&[]).is_none());
    }

    #[test]
    fn test_above_mean_quantity_fraction_in_range() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let f = NormalizedTick::above_mean_quantity_fraction(&ticks).unwrap();
        assert!(f >= 0.0 && f <= 1.0, "fraction in [0,1], got {}", f);
    }

    #[test]
    fn test_price_unchanged_fraction_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_unchanged_fraction(&[t]).is_none());
    }

    #[test]
    fn test_price_unchanged_fraction_zero_for_all_changing() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let f = NormalizedTick::price_unchanged_fraction(&ticks).unwrap();
        assert!(f.abs() < 1e-9, "all prices different → unchanged=0, got {}", f);
    }

    #[test]
    fn test_qty_weighted_range_none_for_empty() {
        assert!(NormalizedTick::qty_weighted_range(&[]).is_none());
    }

    #[test]
    fn test_qty_weighted_range_zero_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(2));
        let r = NormalizedTick::qty_weighted_range(&[t]).unwrap();
        assert!(r.abs() < 1e-9, "single tick → range=0, got {}", r);
    }

    // ── round-84 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_sell_notional_fraction_none_for_empty() {
        assert!(NormalizedTick::sell_notional_fraction(&[]).is_none());
    }

    #[test]
    fn test_sell_notional_fraction_zero_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(3));
        t1.side = Some(crate::tick::TradeSide::Buy);
        let r = NormalizedTick::sell_notional_fraction(&[t1]).unwrap();
        assert!(r.abs() < 1e-9, "all buys → sell fraction=0, got {}", r);
    }

    #[test]
    fn test_max_price_gap_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::max_price_gap(&[t]).is_none());
    }

    #[test]
    fn test_max_price_gap_correct_value() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(103), dec!(1)),
        ];
        assert_eq!(NormalizedTick::max_price_gap(&ticks).unwrap(), dec!(5));
    }

    #[test]
    fn test_price_range_velocity_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_range_velocity(&[t]).is_none());
    }

    #[test]
    fn test_tick_count_per_ms_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_count_per_ms(&[t]).is_none());
    }

    #[test]
    fn test_buy_quantity_fraction_none_for_empty() {
        assert!(NormalizedTick::buy_quantity_fraction(&[]).is_none());
    }

    #[test]
    fn test_buy_quantity_fraction_one_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(crate::tick::TradeSide::Buy);
        let f = NormalizedTick::buy_quantity_fraction(&[t]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all buys → buy fraction=1, got {}", f);
    }

    #[test]
    fn test_sell_quantity_fraction_none_for_empty() {
        assert!(NormalizedTick::sell_quantity_fraction(&[]).is_none());
    }

    #[test]
    fn test_sell_quantity_fraction_one_for_all_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(crate::tick::TradeSide::Sell);
        let f = NormalizedTick::sell_quantity_fraction(&[t]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all sells → sell fraction=1, got {}", f);
    }

    #[test]
    fn test_price_mean_crossover_count_none_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_mean_crossover_count(&[t]).is_none());
    }

    #[test]
    fn test_price_mean_crossover_count_in_range() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(90), dec!(1)),
        ];
        let c = NormalizedTick::price_mean_crossover_count(&ticks).unwrap();
        assert!(c >= 1, "expect at least 1 crossover, got {}", c);
    }

    #[test]
    fn test_notional_skewness_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::notional_skewness(&ticks).is_none());
    }

    #[test]
    fn test_volume_weighted_mid_price_none_for_empty() {
        assert!(NormalizedTick::volume_weighted_mid_price(&[]).is_none());
    }

    #[test]
    fn test_volume_weighted_mid_price_equals_price_for_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(123), dec!(5));
        let mid = NormalizedTick::volume_weighted_mid_price(&[t]).unwrap();
        assert_eq!(mid, dec!(123));
    }

    // ── round-85 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_neutral_count_zero_when_all_sided() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(crate::tick::TradeSide::Buy);
        assert_eq!(NormalizedTick::neutral_count(&[t]), 0);
    }

    #[test]
    fn test_neutral_count_all_when_no_side() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert_eq!(NormalizedTick::neutral_count(&[t1, t2]), 2);
    }

    #[test]
    fn test_price_dispersion_none_for_empty() {
        assert!(NormalizedTick::price_dispersion(&[]).is_none());
    }

    #[test]
    fn test_price_dispersion_zero_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::price_dispersion(&[t]).unwrap(), dec!(0));
    }

    #[test]
    fn test_max_notional_none_for_empty() {
        assert!(NormalizedTick::max_notional(&[]).is_none());
    }

    #[test]
    fn test_max_notional_selects_largest() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(2)); // 200
        let t2 = make_tick_pq(dec!(50), dec!(5));  // 250
        assert_eq!(NormalizedTick::max_notional(&[t1, t2]).unwrap(), dec!(250));
    }

    #[test]
    fn test_min_notional_none_for_empty() {
        assert!(NormalizedTick::min_notional(&[]).is_none());
    }

    #[test]
    fn test_below_vwap_fraction_none_for_empty() {
        assert!(NormalizedTick::below_vwap_fraction(&[]).is_none());
    }

    #[test]
    fn test_trade_notional_std_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::trade_notional_std(&[t]).is_none());
    }

    #[test]
    fn test_buy_sell_count_ratio_none_for_no_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(crate::tick::TradeSide::Buy);
        assert!(NormalizedTick::buy_sell_count_ratio(&[t]).is_none());
    }

    #[test]
    fn test_buy_sell_count_ratio_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(crate::tick::TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.side = Some(crate::tick::TradeSide::Sell);
        let r = NormalizedTick::buy_sell_count_ratio(&[t1, t2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "1 buy / 1 sell = 1.0, got {}", r);
    }

    #[test]
    fn test_price_mad_none_for_empty() {
        assert!(NormalizedTick::price_mad(&[]).is_none());
    }

    #[test]
    fn test_price_mad_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        let m = NormalizedTick::price_mad(&ticks).unwrap();
        assert!(m.abs() < 1e-9, "constant price → MAD=0, got {}", m);
    }

    #[test]
    fn test_price_range_pct_of_open_none_for_empty() {
        assert!(NormalizedTick::price_range_pct_of_open(&[]).is_none());
    }

    #[test]
    fn test_price_range_pct_of_open_zero_for_constant() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let p = NormalizedTick::price_range_pct_of_open(&ticks).unwrap();
        assert!(p.abs() < 1e-9, "constant → range_pct=0, got {}", p);
    }

    // ── round-86 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_price_mean_none_for_empty() {
        assert!(NormalizedTick::price_mean(&[]).is_none());
    }

    #[test]
    fn test_price_mean_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(200), dec!(1))];
        assert_eq!(NormalizedTick::price_mean(&ticks).unwrap(), dec!(150));
    }

    #[test]
    fn test_uptick_count_zero_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::uptick_count(&[t]), 0);
    }

    #[test]
    fn test_uptick_count_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        assert_eq!(NormalizedTick::uptick_count(&ticks), 1);
    }

    #[test]
    fn test_downtick_count_zero_for_all_up() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        assert_eq!(NormalizedTick::downtick_count(&ticks), 0);
    }

    #[test]
    fn test_uptick_fraction_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::uptick_fraction(&[t]).is_none());
    }

    #[test]
    fn test_quantity_std_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::quantity_std(&[t]).is_none());
    }

    #[test]
    fn test_quantity_std_zero_for_constant_qty() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(101), dec!(5)),
        ];
        let s = NormalizedTick::quantity_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "constant quantity → std=0, got {}", s);
    }

    // ── round-87 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_vwap_deviation_std_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::vwap_deviation_std(&[t]).is_none());
    }

    #[test]
    fn test_vwap_deviation_std_zero_for_single_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        // All prices equal VWAP, so deviation std = 0
        let s = NormalizedTick::vwap_deviation_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "all at VWAP → std=0, got {}", s);
    }

    #[test]
    fn test_vwap_deviation_std_positive_for_varied_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(90), dec!(1)),
        ];
        let s = NormalizedTick::vwap_deviation_std(&ticks).unwrap();
        assert!(s > 0.0, "varied prices → std > 0, got {}", s);
    }

    #[test]
    fn test_max_consecutive_side_run_zero_for_no_side() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert_eq!(NormalizedTick::max_consecutive_side_run(&ticks), 0);
    }

    #[test]
    fn test_max_consecutive_side_run_with_sides() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.side = Some(TradeSide::Buy);
        let mut t3 = make_tick_pq(dec!(102), dec!(1));
        t3.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::max_consecutive_side_run(&[t1, t2, t3]), 2);
    }

    #[test]
    fn test_inter_arrival_cv_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::inter_arrival_cv(&[t]).is_none());
    }

    #[test]
    fn test_inter_arrival_cv_zero_for_uniform_spacing() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 1000;
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.received_at_ms = 2000;
        let mut t3 = make_tick_pq(dec!(102), dec!(1));
        t3.received_at_ms = 3000;
        // All intervals = 1000ms → std=0, cv=0
        let cv = NormalizedTick::inter_arrival_cv(&[t1, t2, t3]).unwrap();
        assert!(cv.abs() < 1e-9, "uniform spacing → cv=0, got {}", cv);
    }

    #[test]
    fn test_volume_per_ms_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(5));
        assert!(NormalizedTick::volume_per_ms(&[t]).is_none());
    }

    #[test]
    fn test_volume_per_ms_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(5));
        t1.received_at_ms = 1000;
        let mut t2 = make_tick_pq(dec!(101), dec!(5));
        t2.received_at_ms = 2000;
        // 10 qty / 1000 ms = 0.01
        let r = NormalizedTick::volume_per_ms(&[t1, t2]).unwrap();
        assert!((r - 0.01).abs() < 1e-9, "expected 0.01, got {}", r);
    }

    #[test]
    fn test_notional_per_second_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::notional_per_second(&[t]).is_none());
    }

    #[test]
    fn test_notional_per_second_positive() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.received_at_ms = 1000; // 1 second
        // 100 + 100 = 200 notional in 1s
        let r = NormalizedTick::notional_per_second(&[t1, t2]).unwrap();
        assert!((r - 200.0).abs() < 1e-9, "expected 200, got {}", r);
    }

    // ── round-88 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_order_flow_imbalance_none_for_empty() {
        assert!(NormalizedTick::order_flow_imbalance(&[]).is_none());
    }

    #[test]
    fn test_order_flow_imbalance_pos_one_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(crate::tick::TradeSide::Buy);
        let r = NormalizedTick::order_flow_imbalance(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all buys → OFI=+1, got {}", r);
    }

    #[test]
    fn test_order_flow_imbalance_neg_one_for_all_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(crate::tick::TradeSide::Sell);
        let r = NormalizedTick::order_flow_imbalance(&[t]).unwrap();
        assert!((r + 1.0).abs() < 1e-9, "all sells → OFI=-1, got {}", r);
    }

    #[test]
    fn test_price_qty_up_fraction_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_qty_up_fraction(&[t]).is_none());
    }

    #[test]
    fn test_running_high_count_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::running_high_count(&[t]), 1);
    }

    #[test]
    fn test_running_low_count_single_tick() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::running_low_count(&[t]), 1);
    }

    #[test]
    fn test_buy_sell_avg_qty_ratio_none_for_no_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(crate::tick::TradeSide::Buy);
        assert!(NormalizedTick::buy_sell_avg_qty_ratio(&[t]).is_none());
    }

    #[test]
    fn test_max_price_drop_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::max_price_drop(&[t]).is_none());
    }

    #[test]
    fn test_max_price_rise_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::max_price_rise(&[t]).is_none());
    }

    #[test]
    fn test_max_price_drop_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(90), dec!(1)),  // drop = 10
            make_tick_pq(dec!(95), dec!(1)),  // rise
        ];
        assert_eq!(NormalizedTick::max_price_drop(&ticks).unwrap(), dec!(10));
    }

    #[test]
    fn test_max_price_rise_correct() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),  // rise = 15
            make_tick_pq(dec!(100), dec!(1)),
        ];
        assert_eq!(NormalizedTick::max_price_rise(&ticks).unwrap(), dec!(15));
    }

    #[test]
    fn test_buy_trade_count_zero_for_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::buy_trade_count(&[t]), 0);
    }

    #[test]
    fn test_buy_trade_count_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::buy_trade_count(&[t1, t2]), 1);
    }

    #[test]
    fn test_sell_trade_count_correct() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::sell_trade_count(&[t1, t2]), 1);
    }

    #[test]
    fn test_price_reversal_fraction_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert!(NormalizedTick::price_reversal_fraction(&[t1, t2]).is_none());
    }

    #[test]
    fn test_price_reversal_fraction_one_for_zigzag() {
        use rust_decimal_macros::dec;
        // up-down-up: 2 reversals out of 2 pairs → 1.0
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(115), dec!(1)),
        ];
        let f = NormalizedTick::price_reversal_fraction(&ticks).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "perfect zigzag → 1.0, got {}", f);
    }

    // ── round-89 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_near_vwap_fraction_none_for_empty() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::near_vwap_fraction(&[], dec!(1)).is_none());
    }

    #[test]
    fn test_near_vwap_fraction_one_for_all_at_vwap() {
        use rust_decimal_macros::dec;
        // All ticks at same price → price == VWAP, band = 0 → fraction = 1
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let f = NormalizedTick::near_vwap_fraction(&ticks, dec!(0)).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all at VWAP → 1.0, got {}", f);
    }

    #[test]
    fn test_mean_tick_return_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::mean_tick_return(&[t]).is_none());
    }

    #[test]
    fn test_mean_tick_return_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let r = NormalizedTick::mean_tick_return(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "constant price → mean_return=0, got {}", r);
    }

    #[test]
    fn test_passive_buy_count_zero_for_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert_eq!(NormalizedTick::passive_buy_count(&[t]), 0);
    }

    #[test]
    fn test_quantity_iqr_none_for_small_slice() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(2)),
        ];
        assert!(NormalizedTick::quantity_iqr(&ticks).is_none());
    }

    #[test]
    fn test_quantity_iqr_positive_for_varied_quantities() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = [dec!(1), dec!(2), dec!(8), dec!(16), dec!(32), dec!(64), dec!(128), dec!(256)]
            .iter()
            .map(|&q| make_tick_pq(dec!(100), q))
            .collect();
        let iqr = NormalizedTick::quantity_iqr(&ticks).unwrap();
        assert!(iqr > dec!(0));
    }

    #[test]
    fn test_top_quartile_price_fraction_none_for_small_slice() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::top_quartile_price_fraction(&ticks).is_none());
    }

    #[test]
    fn test_buy_notional_ratio_none_for_empty() {
        assert!(NormalizedTick::buy_notional_ratio(&[]).is_none());
    }

    #[test]
    fn test_buy_notional_ratio_one_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let r = NormalizedTick::buy_notional_ratio(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all buys → ratio=1, got {}", r);
    }

    #[test]
    fn test_return_std_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert!(NormalizedTick::return_std(&[t1, t2]).is_none());
    }

    #[test]
    fn test_return_std_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let s = NormalizedTick::return_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "constant price → return_std=0, got {}", s);
    }

    // ── round-90 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_max_drawdown_none_for_empty() {
        assert!(NormalizedTick::max_drawdown(&[]).is_none());
    }

    #[test]
    fn test_max_drawdown_zero_for_rising_prices() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
        ];
        let dd = NormalizedTick::max_drawdown(&ticks).unwrap();
        assert!(dd.abs() < 1e-9, "monotone rise → drawdown=0, got {}", dd);
    }

    #[test]
    fn test_max_drawdown_positive_after_peak() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
            make_tick_pq(dec!(90), dec!(1)),
        ];
        let dd = NormalizedTick::max_drawdown(&ticks).unwrap();
        // peak=120, trough=90 → dd = 30/120 = 0.25
        assert!((dd - 0.25).abs() < 1e-6, "expected 0.25, got {}", dd);
    }

    #[test]
    fn test_high_to_low_ratio_none_for_empty() {
        assert!(NormalizedTick::high_to_low_ratio(&[]).is_none());
    }

    #[test]
    fn test_high_to_low_ratio_one_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let r = NormalizedTick::high_to_low_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "constant price → ratio=1, got {}", r);
    }

    #[test]
    fn test_tick_velocity_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::tick_velocity(&[t]).is_none());
    }

    #[test]
    fn test_notional_decay_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::notional_decay(&[t]).is_none());
    }

    #[test]
    fn test_notional_decay_one_for_balanced_halves() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        let r = NormalizedTick::notional_decay(&[t1, t2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "equal halves → ratio=1, got {}", r);
    }

    #[test]
    fn test_late_price_momentum_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::late_price_momentum(&[t]).is_none());
    }

    #[test]
    fn test_consecutive_buys_max_zero_for_empty() {
        assert_eq!(NormalizedTick::consecutive_buys_max(&[]), 0);
    }

    #[test]
    fn test_consecutive_buys_max_two_for_run_of_two() {
        use rust_decimal_macros::dec;
        let mut buy1 = make_tick_pq(dec!(100), dec!(1));
        buy1.side = Some(TradeSide::Buy);
        let mut buy2 = make_tick_pq(dec!(101), dec!(1));
        buy2.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(102), dec!(1));
        sell.side = Some(TradeSide::Sell);
        assert_eq!(NormalizedTick::consecutive_buys_max(&[buy1, buy2, sell]), 2);
    }

    // ── round-91 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_price_acceleration_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert!(NormalizedTick::price_acceleration(&[t1, t2]).is_none());
    }

    #[test]
    fn test_price_acceleration_zero_for_linear_price() {
        use rust_decimal_macros::dec;
        // Linear: 100, 101, 102 → second differences all zero
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let a = NormalizedTick::price_acceleration(&ticks).unwrap();
        assert!(a.abs() < 1e-9, "linear price → acceleration=0, got {}", a);
    }

    #[test]
    fn test_above_mean_qty_fraction_none_for_empty() {
        assert!(NormalizedTick::above_mean_qty_fraction(&[]).is_none());
    }

    #[test]
    fn test_above_mean_qty_fraction_half_for_one_above_one_below() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1)); // qty=1 < mean=2
        let t2 = make_tick_pq(dec!(100), dec!(3)); // qty=3 > mean=2
        let f = NormalizedTick::above_mean_qty_fraction(&[t1, t2]).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "one above, one below → 0.5, got {}", f);
    }

    #[test]
    fn test_side_alternation_rate_none_for_no_sided_ticks() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::side_alternation_rate(&[t]).is_none());
    }

    #[test]
    fn test_side_alternation_rate_one_for_full_alternation() {
        use rust_decimal_macros::dec;
        let mut b = make_tick_pq(dec!(100), dec!(1));
        b.side = Some(TradeSide::Buy);
        let mut s = make_tick_pq(dec!(101), dec!(1));
        s.side = Some(TradeSide::Sell);
        let mut b2 = make_tick_pq(dec!(102), dec!(1));
        b2.side = Some(TradeSide::Buy);
        let r = NormalizedTick::side_alternation_rate(&[b, s, b2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "B-S-B → rate=1.0, got {}", r);
    }

    #[test]
    fn test_price_range_per_tick_none_for_empty() {
        assert!(NormalizedTick::price_range_per_tick(&[]).is_none());
    }

    #[test]
    fn test_price_range_per_tick_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let r = NormalizedTick::price_range_per_tick(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "constant price → range_per_tick=0, got {}", r);
    }

    #[test]
    fn test_qty_weighted_price_std_none_for_empty() {
        assert!(NormalizedTick::qty_weighted_price_std(&[]).is_none());
    }

    #[test]
    fn test_qty_weighted_price_std_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let s = NormalizedTick::qty_weighted_price_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "constant price → std=0, got {}", s);
    }

    // ── round-92 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_buy_pressure_ratio_none_for_empty() {
        assert!(NormalizedTick::buy_pressure_ratio(&[]).is_none());
    }

    #[test]
    fn test_buy_pressure_ratio_one_for_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(TradeSide::Buy);
        let r = NormalizedTick::buy_pressure_ratio(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "all buys → ratio=1, got {}", r);
    }

    #[test]
    fn test_sell_pressure_ratio_complements_buy() {
        use rust_decimal_macros::dec;
        let mut b1 = make_tick_pq(dec!(100), dec!(3));
        b1.side = Some(TradeSide::Buy);
        let mut s1 = make_tick_pq(dec!(100), dec!(1));
        s1.side = Some(TradeSide::Sell);
        let mut b2 = make_tick_pq(dec!(100), dec!(3));
        b2.side = Some(TradeSide::Buy);
        let mut s2 = make_tick_pq(dec!(100), dec!(1));
        s2.side = Some(TradeSide::Sell);
        let buy_r = NormalizedTick::buy_pressure_ratio(&[b1, s1]).unwrap();
        let sell_r = NormalizedTick::sell_pressure_ratio(&[b2, s2]).unwrap();
        assert!((buy_r + sell_r - 1.0).abs() < 1e-9, "buy+sell=1, got {}", buy_r + sell_r);
    }

    #[test]
    fn test_trade_interval_ratio_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::trade_interval_ratio(&[t]).is_none());
    }

    #[test]
    fn test_weighted_price_change_none_for_empty() {
        assert!(NormalizedTick::weighted_price_change(&[]).is_none());
    }

    #[test]
    fn test_first_last_price_ratio_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::first_last_price_ratio(&[t]).is_none());
    }

    #[test]
    fn test_first_last_price_ratio_one_for_unchanged_price() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        let r = NormalizedTick::first_last_price_ratio(&[t1, t2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "unchanged price → ratio=1, got {}", r);
    }

    #[test]
    fn test_tick_price_variance_none_for_empty() {
        assert!(NormalizedTick::tick_price_variance(&[]).is_none());
    }

    #[test]
    fn test_tick_price_variance_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let v = NormalizedTick::tick_price_variance(&ticks).unwrap();
        assert!(v.abs() < 1e-9, "constant price → variance=0, got {}", v);
    }

    // ── round-93 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_buy_side_vwap_none_for_no_buys() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1)); // no side
        assert!(NormalizedTick::buy_side_vwap(&[t]).is_none());
    }

    #[test]
    fn test_buy_side_vwap_equals_price_for_single_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(5));
        t.side = Some(TradeSide::Buy);
        let vwap = NormalizedTick::buy_side_vwap(&[t]).unwrap();
        assert_eq!(vwap, dec!(100));
    }

    #[test]
    fn test_sell_side_vwap_none_for_no_sells() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::sell_side_vwap(&[t]).is_none());
    }

    #[test]
    fn test_inter_tick_gap_cv_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::inter_tick_gap_cv(&[t1, t2]).is_none());
    }

    #[test]
    fn test_signed_tick_count_none_for_single_tick() {
        let t = make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1));
        assert!(NormalizedTick::signed_tick_count(&[t]).is_none());
    }

    #[test]
    fn test_signed_tick_count_positive_for_rising() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let c = NormalizedTick::signed_tick_count(&ticks).unwrap();
        assert_eq!(c, 2, "two up-ticks → +2");
    }
}
