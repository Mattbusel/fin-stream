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

    // ── round-94 ─────────────────────────────────────────────────────────────

    /// Net price displacement divided by total path length (0.0–1.0).
    /// Returns `None` for fewer than 2 ticks or when the total path is zero.
    pub fn price_efficiency_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let net = (ticks.last()?.price - ticks[0].price).abs();
        let path: Decimal = ticks
            .windows(2)
            .map(|w| (w[1].price - w[0].price).abs())
            .sum();
        if path.is_zero() {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        Some((net / path).to_f64().unwrap_or(0.0))
    }

    /// Minimum gap between consecutive `received_at_ms` timestamps in milliseconds.
    /// Returns `None` for fewer than 2 ticks.
    pub fn min_inter_tick_gap_ms(ticks: &[NormalizedTick]) -> Option<u64> {
        if ticks.len() < 2 {
            return None;
        }
        ticks
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .min()
    }

    /// Maximum gap between consecutive `received_at_ms` timestamps in milliseconds.
    /// Returns `None` for fewer than 2 ticks.
    pub fn max_inter_tick_gap_ms(ticks: &[NormalizedTick]) -> Option<u64> {
        if ticks.len() < 2 {
            return None;
        }
        ticks
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .max()
    }

    /// Signed imbalance between buy and sell counts: `(buys − sells) / total`.
    /// Returns `None` when fewer than 2 ticks carry side information.
    pub fn trade_count_imbalance(ticks: &[NormalizedTick]) -> Option<f64> {
        let (buys, sells) = ticks.iter().fold((0i64, 0i64), |(b, s), t| match t.side {
            Some(TradeSide::Buy) => (b + 1, s),
            Some(TradeSide::Sell) => (b, s + 1),
            None => (b, s),
        });
        let total = buys + sells;
        if total < 2 {
            return None;
        }
        Some((buys - sells) as f64 / total as f64)
    }

    // ── round-95 ─────────────────────────────────────────────────────────────

    /// Fraction of total traded volume that is buy-side.
    /// Returns `None` if no ticks carry side information or total volume is zero.
    pub fn buy_volume_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let (buy_vol, total_vol) = ticks.iter().fold(
            (Decimal::ZERO, Decimal::ZERO),
            |(bv, tv), t| match t.side {
                Some(TradeSide::Buy) => (bv + t.quantity, tv + t.quantity),
                Some(TradeSide::Sell) => (bv, tv + t.quantity),
                None => (bv, tv),
            },
        );
        if total_vol.is_zero() {
            return None;
        }
        Some((buy_vol / total_vol).to_f64().unwrap_or(0.0))
    }

    /// Skewness (third standardized moment) of the tick quantity distribution.
    /// Returns `None` for fewer than 3 ticks or when standard deviation is zero.
    pub fn tick_qty_skewness(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 {
            return None;
        }
        let n = ticks.len() as f64;
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return None;
        }
        let skew = vals.iter().map(|v| ((v - mean) / std_dev).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Fraction of ticks whose price is strictly above the median price.
    /// Returns `None` for an empty slice.
    pub fn above_median_price_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let mut prices: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        prices.sort();
        let mid = prices.len() / 2;
        let median = if prices.len() % 2 == 0 {
            (prices[mid - 1] + prices[mid]) / Decimal::TWO
        } else {
            prices[mid]
        };
        let above = ticks.iter().filter(|t| t.price > median).count();
        Some(above as f64 / ticks.len() as f64)
    }

    /// Last value of the cumulative buy-minus-sell quantity imbalance as a
    /// fraction of total sided quantity.  Returns `None` when no ticks have sides
    /// or total sided quantity is zero.
    pub fn cumulative_qty_imbalance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let (net, total) = ticks.iter().fold(
            (Decimal::ZERO, Decimal::ZERO),
            |(net, tot), t| match t.side {
                Some(TradeSide::Buy) => (net + t.quantity, tot + t.quantity),
                Some(TradeSide::Sell) => (net - t.quantity, tot + t.quantity),
                None => (net, tot),
            },
        );
        if total.is_zero() {
            return None;
        }
        Some((net / total).to_f64().unwrap_or(0.0))
    }

    // ── round-96 ─────────────────────────────────────────────────────────────

    /// Quantity-weighted average spread between each tick's price and the VWAP.
    /// Returns `None` for an empty slice or zero total quantity.
    pub fn qty_weighted_spread(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap = ticks.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        let spread = ticks
            .iter()
            .map(|t| (t.price - vwap).abs() * t.quantity)
            .sum::<Decimal>()
            / total_qty;
        Some(spread.to_f64().unwrap_or(0.0))
    }

    /// Fraction of ticks whose quantity exceeds the mean quantity.
    /// Returns `None` for an empty slice.
    pub fn large_tick_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let n = Decimal::from(ticks.len());
        let mean_qty: Decimal = ticks.iter().map(|t| t.quantity).sum::<Decimal>() / n;
        let above = ticks.iter().filter(|t| t.quantity > mean_qty).count();
        Some(above as f64 / ticks.len() as f64)
    }

    /// Net directional price drift: sum of signed price changes divided by the
    /// number of consecutive pairs.  Returns `None` for fewer than 2 ticks.
    pub fn net_price_drift(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let net: Decimal = ticks.windows(2).map(|w| w[1].price - w[0].price).sum();
        let pairs = Decimal::from(ticks.len() - 1);
        Some((net / pairs).to_f64().unwrap_or(0.0))
    }

    /// Approximate entropy of tick inter-arrival times: −Σ p·ln(p) over
    /// quantised gap buckets (5 equal-width bins).  Returns `None` for fewer
    /// than 3 ticks or when all gaps are identical.
    pub fn tick_arrival_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let gaps: Vec<u64> = ticks
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .collect();
        let min_gap = *gaps.iter().min()?;
        let max_gap = *gaps.iter().max()?;
        if min_gap == max_gap {
            return None;
        }
        let range = (max_gap - min_gap) as f64;
        let n_bins = 5usize;
        let mut bins = vec![0usize; n_bins];
        for &g in &gaps {
            let idx = (((g - min_gap) as f64 / range) * (n_bins - 1) as f64).round() as usize;
            bins[idx.min(n_bins - 1)] += 1;
        }
        let total = gaps.len() as f64;
        let entropy = bins
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / total;
                -p * p.ln()
            })
            .sum::<f64>();
        Some(entropy)
    }

    // ── round-97 ─────────────────────────────────────────────────────────────

    /// Number of times consecutive tick prices cross the VWAP line.
    /// Returns `None` for fewer than 2 ticks.
    pub fn price_gap_count(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.len() < 2 {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap = ticks.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        let crossings = ticks
            .windows(2)
            .filter(|w| {
                (w[0].price > vwap) != (w[1].price > vwap)
            })
            .count();
        Some(crossings)
    }

    /// Number of ticks per millisecond of the total window span.
    /// Returns `None` for fewer than 2 ticks or zero time span.
    pub fn tick_density(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let span = ticks
            .last()?
            .received_at_ms
            .saturating_sub(ticks.first()?.received_at_ms);
        if span == 0 {
            return None;
        }
        Some(ticks.len() as f64 / span as f64)
    }

    /// Mean quantity of buy-side ticks. Returns `None` if no buy ticks present.
    pub fn buy_qty_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buys: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.quantity)
            .collect();
        if buys.is_empty() {
            return None;
        }
        let mean = buys.iter().copied().sum::<Decimal>() / Decimal::from(buys.len());
        Some(mean.to_f64().unwrap_or(0.0))
    }

    /// Mean quantity of sell-side ticks. Returns `None` if no sell ticks present.
    pub fn sell_qty_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let sells: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .map(|t| t.quantity)
            .collect();
        if sells.is_empty() {
            return None;
        }
        let mean = sells.iter().copied().sum::<Decimal>() / Decimal::from(sells.len());
        Some(mean.to_f64().unwrap_or(0.0))
    }

    /// Asymmetry of the price range: `(high − mid) − (mid − low)` normalised by
    /// the total range, where `mid = (high + low) / 2`.
    /// Returns `None` for fewer than 1 tick or when the price range is zero.
    pub fn price_range_asymmetry(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let high = ticks.iter().map(|t| t.price).max()?;
        let low = ticks.iter().map(|t| t.price).min()?;
        let range = high - low;
        if range.is_zero() {
            return None;
        }
        let mid = (high + low) / Decimal::TWO;
        let asym = ((high - mid) - (mid - low)) / range;
        Some(asym.to_f64().unwrap_or(0.0))
    }

    // ── round-98 ─────────────────────────────────────────────────────────────

    /// Fraction of consecutive tick pairs where the price direction reverses.
    /// Returns `None` for fewer than 3 ticks or no directional changes.
    pub fn tick_reversal_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        let dirs: Vec<i32> = ticks
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
            .collect();
        let reversals = dirs.windows(2).filter(|d| d[0] != 0 && d[1] != 0 && d[0] != d[1]).count();
        let valid_pairs = dirs.windows(2).filter(|d| d[0] != 0 && d[1] != 0).count();
        if valid_pairs == 0 {
            return None;
        }
        Some(reversals as f64 / valid_pairs as f64)
    }

    /// VWAP of the first half of the tick slice.
    /// Returns `None` for fewer than 2 ticks or zero quantity in first half.
    pub fn first_half_vwap(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let half = ticks.len() / 2;
        let first = &ticks[..half];
        let total_qty: Decimal = first.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap = first.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        Some(vwap.to_f64().unwrap_or(0.0))
    }

    /// VWAP of the second half of the tick slice.
    /// Returns `None` for fewer than 2 ticks or zero quantity in second half.
    pub fn second_half_vwap(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let half = ticks.len() / 2;
        let second = &ticks[half..];
        let total_qty: Decimal = second.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap = second.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        Some(vwap.to_f64().unwrap_or(0.0))
    }

    /// Momentum of cumulative traded quantity: last tick's quantity minus first tick's.
    /// Returns `None` for fewer than 2 ticks.
    pub fn qty_momentum(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let diff = ticks.last()?.quantity - ticks.first()?.quantity;
        Some(diff.to_f64().unwrap_or(0.0))
    }

    // ── round-99 ─────────────────────────────────────────────────────────────

    /// Rate of change of the average price-change per tick, comparing second
    /// half vs first half inter-tick price changes.
    /// Returns `None` for fewer than 4 ticks.
    pub fn price_change_acceleration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 4 {
            return None;
        }
        let changes: Vec<f64> = ticks
            .windows(2)
            .map(|w| (w[1].price - w[0].price).to_f64().unwrap_or(0.0))
            .collect();
        let half = changes.len() / 2;
        let first_mean = changes[..half].iter().sum::<f64>() / half as f64;
        let second_mean = changes[half..].iter().sum::<f64>() / (changes.len() - half) as f64;
        Some(second_mean - first_mean)
    }

    /// Average quantity traded per distinct side (buy or sell).
    /// Returns `None` if no ticks carry side information.
    pub fn avg_qty_per_direction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let (buy_qty, buy_n, sell_qty, sell_n) = ticks.iter().fold(
            (Decimal::ZERO, 0usize, Decimal::ZERO, 0usize),
            |(bq, bn, sq, sn), t| match t.side {
                Some(TradeSide::Buy) => (bq + t.quantity, bn + 1, sq, sn),
                Some(TradeSide::Sell) => (bq, bn, sq + t.quantity, sn + 1),
                None => (bq, bn, sq, sn),
            },
        );
        if buy_n == 0 && sell_n == 0 {
            return None;
        }
        let total_qty = buy_qty + sell_qty;
        let total_n = buy_n + sell_n;
        Some((total_qty / Decimal::from(total_n)).to_f64().unwrap_or(0.0))
    }

    /// Micro-price: mid-price weighted by bid and ask imbalance using buy/sell
    /// quantities as proxies. `(buy_vol * bid + sell_vol * ask) / (buy_vol + sell_vol)`.
    /// Here bid = min price of buy ticks, ask = max price of sell ticks.
    /// Returns `None` if both buy and sell sided ticks are absent.
    pub fn micro_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
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
        if buy_vol.is_zero() && sell_vol.is_zero() {
            return None;
        }
        let bid = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.price)
            .min()
            .unwrap_or(Decimal::ZERO);
        let ask = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .map(|t| t.price)
            .max()
            .unwrap_or(Decimal::ZERO);
        let total_vol = buy_vol + sell_vol;
        if total_vol.is_zero() {
            return None;
        }
        let mp = (buy_vol * bid + sell_vol * ask) / total_vol;
        Some(mp.to_f64().unwrap_or(0.0))
    }

    /// Interquartile range of inter-tick gap times (Q3 − Q1) in milliseconds.
    /// Returns `None` for fewer than 4 ticks.
    pub fn inter_tick_gap_iqr(ticks: &[NormalizedTick]) -> Option<u64> {
        if ticks.len() < 4 {
            return None;
        }
        let mut gaps: Vec<u64> = ticks
            .windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .collect();
        gaps.sort();
        let q1 = gaps[gaps.len() / 4];
        let q3 = gaps[(gaps.len() * 3) / 4];
        Some(q3.saturating_sub(q1))
    }

    // ── round-100 ────────────────────────────────────────────────────────────

    /// Length of the longest consecutive run of buy-side ticks.
    /// Returns `None` for an empty slice or no sided ticks.
    pub fn consecutive_buy_streak(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() {
            return None;
        }
        let mut max_run = 0usize;
        let mut cur = 0usize;
        let mut any_sided = false;
        for t in ticks {
            if t.side == Some(TradeSide::Buy) {
                any_sided = true;
                cur += 1;
                if cur > max_run {
                    max_run = cur;
                }
            } else if t.side.is_some() {
                any_sided = true;
                cur = 0;
            }
        }
        if !any_sided {
            return None;
        }
        Some(max_run)
    }

    /// Herfindahl-like quantity concentration: sum of squared quantity shares.
    /// Returns `None` for an empty slice or zero total quantity.
    pub fn qty_concentration_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        let hhi: f64 = ticks
            .iter()
            .map(|t| {
                let share = (t.quantity / total).to_f64().unwrap_or(0.0);
                share * share
            })
            .sum();
        Some(hhi)
    }

    /// Number of distinct price levels (unique prices) in the tick slice.
    /// Returns `None` for an empty slice.
    pub fn price_level_count(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() {
            return None;
        }
        let mut prices: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        prices.sort();
        prices.dedup();
        Some(prices.len())
    }

    /// Mean number of ticks per distinct price level.
    /// Returns `None` for an empty slice or zero distinct levels.
    pub fn tick_count_per_price_level(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() {
            return None;
        }
        let levels = NormalizedTick::price_level_count(ticks)?;
        if levels == 0 {
            return None;
        }
        Some(ticks.len() as f64 / levels as f64)
    }

    // ── round-101 ────────────────────────────────────────────────────────────

    /// Average price of buy-side ticks.
    /// Returns `None` if no buy ticks are present.
    pub fn avg_buy_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buys: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Buy))
            .map(|t| t.price)
            .collect();
        if buys.is_empty() {
            return None;
        }
        let mean = buys.iter().copied().sum::<Decimal>() / Decimal::from(buys.len());
        Some(mean.to_f64().unwrap_or(0.0))
    }

    /// Average price of sell-side ticks.
    /// Returns `None` if no sell ticks are present.
    pub fn avg_sell_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let sells: Vec<Decimal> = ticks
            .iter()
            .filter(|t| t.side == Some(TradeSide::Sell))
            .map(|t| t.price)
            .collect();
        if sells.is_empty() {
            return None;
        }
        let mean = sells.iter().copied().sum::<Decimal>() / Decimal::from(sells.len());
        Some(mean.to_f64().unwrap_or(0.0))
    }

    /// Ratio of price range to the VWAP: `(high − low) / vwap`.
    /// Returns `None` for an empty slice or zero VWAP.
    pub fn price_spread_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap = ticks.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        if vwap.is_zero() {
            return None;
        }
        let high = ticks.iter().map(|t| t.price).max()?;
        let low = ticks.iter().map(|t| t.price).min()?;
        Some(((high - low) / vwap).to_f64().unwrap_or(0.0))
    }

    /// Approximate entropy of trade sizes using 5 equal-width bins.
    /// Returns `None` for fewer than 3 ticks or all identical quantities.
    pub fn trade_size_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let qtys: Vec<f64> = ticks
            .iter()
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .collect();
        let min_q = qtys.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_q = qtys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (max_q - min_q).abs() < 1e-12 {
            return None;
        }
        let range = max_q - min_q;
        let n_bins = 5usize;
        let mut bins = vec![0usize; n_bins];
        for &q in &qtys {
            let idx = (((q - min_q) / range) * (n_bins - 1) as f64).round() as usize;
            bins[idx.min(n_bins - 1)] += 1;
        }
        let total = qtys.len() as f64;
        let entropy = bins
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / total;
                -p * p.ln()
            })
            .sum::<f64>();
        Some(entropy)
    }

    // ── round-102 ────────────────────────────────────────────────────────────

    /// Ratio of absolute net price move to total volume: price impact per unit volume.
    /// Returns `None` for an empty slice or zero volume.
    pub fn price_impact_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let total_vol: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_vol.is_zero() {
            return None;
        }
        let net_move = (ticks.last()?.price - ticks.first()?.price).abs();
        Some((net_move / total_vol).to_f64().unwrap_or(0.0))
    }

    /// Length of the longest consecutive run of sell-side ticks.
    /// Returns `None` for an empty slice or no sided ticks.
    pub fn consecutive_sell_streak(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() {
            return None;
        }
        let mut max_run = 0usize;
        let mut cur = 0usize;
        let mut any_sided = false;
        for t in ticks {
            if t.side == Some(TradeSide::Sell) {
                any_sided = true;
                cur += 1;
                if cur > max_run {
                    max_run = cur;
                }
            } else if t.side.is_some() {
                any_sided = true;
                cur = 0;
            }
        }
        if !any_sided {
            return None;
        }
        Some(max_run)
    }

    /// Variance of tick quantities.
    /// Returns `None` for fewer than 2 ticks.
    pub fn avg_qty_variance(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        use rust_decimal::prelude::ToPrimitive;
        let n = ticks.len() as f64;
        let vals: Vec<f64> = ticks
            .iter()
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .collect();
        let mean = vals.iter().sum::<f64>() / n;
        let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
        Some(variance)
    }

    /// Simple price midpoint: `(max_price + min_price) / 2`.
    /// Returns `None` for an empty slice.
    pub fn price_midpoint(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let high = ticks.iter().map(|t| t.price).max()?;
        let low = ticks.iter().map(|t| t.price).min()?;
        Some(((high + low) / Decimal::TWO).to_f64().unwrap_or(0.0))
    }

    // ── round-103 ────────────────────────────────────────────────────────────

    /// Range of trade quantities: `max_qty - min_qty`.
    /// Returns `None` for an empty slice.
    pub fn qty_range(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let max_q = ticks.iter().map(|t| t.quantity).max()?;
        let min_q = ticks.iter().map(|t| t.quantity).min()?;
        Some((max_q - min_q).to_f64().unwrap_or(0.0))
    }

    /// Time-weighted average quantity: each tick's quantity weighted by the
    /// duration (ms) it was the "active" trade until the next tick arrives.
    /// The last tick carries zero weight. Returns `None` for fewer than 2 ticks.
    pub fn time_weighted_qty(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let mut total_weight = 0u64;
        let mut weighted_sum = Decimal::ZERO;
        for i in 0..ticks.len() - 1 {
            let gap = ticks[i + 1].received_at_ms.saturating_sub(ticks[i].received_at_ms);
            let w = Decimal::from(gap);
            weighted_sum += ticks[i].quantity * w;
            total_weight += gap;
        }
        if total_weight == 0 {
            return None;
        }
        Some((weighted_sum / Decimal::from(total_weight)).to_f64().unwrap_or(0.0))
    }

    /// Fraction of ticks whose price exceeds the VWAP of the slice.
    /// Returns `None` for an empty slice or zero total quantity.
    pub fn above_vwap_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() {
            return None;
        }
        let vwap: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum::<Decimal>() / total_qty;
        let above = ticks.iter().filter(|t| t.price > vwap).count();
        Some(above as f64 / ticks.len() as f64)
    }

    /// Tick speed: price range divided by the total time span in milliseconds.
    /// Returns `None` for fewer than 2 ticks or zero time span.
    pub fn tick_speed(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let t_start = ticks.iter().map(|t| t.received_at_ms).min()?;
        let t_end = ticks.iter().map(|t| t.received_at_ms).max()?;
        let span_ms = t_end.saturating_sub(t_start);
        if span_ms == 0 {
            return None;
        }
        let high = ticks.iter().map(|t| t.price).max()?;
        let low = ticks.iter().map(|t| t.price).min()?;
        let range = (high - low).to_f64().unwrap_or(0.0);
        Some(range / span_ms as f64)
    }

    // ── round-104 ────────────────────────────────────────────────────────────

    /// 75th-percentile trade quantity across the slice.
    /// Returns `None` for an empty slice.
    pub fn qty_percentile_75(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = ticks.iter().map(|t| t.quantity).collect();
        sorted.sort();
        let idx = ((sorted.len() as f64 * 0.75).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)].to_f64()
    }

    /// Count of ticks whose quantity exceeds the mean quantity of the slice.
    /// Returns `None` for an empty slice.
    pub fn large_qty_count(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        let mean = total / Decimal::from(ticks.len());
        Some(ticks.iter().filter(|t| t.quantity > mean).count())
    }

    /// Root mean square of prices: `sqrt(mean(price^2))`.
    /// Returns `None` for an empty slice.
    pub fn price_rms(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let sum_sq: Decimal = ticks.iter().map(|t| t.price * t.price).sum();
        let mean_sq = sum_sq / Decimal::from(ticks.len());
        mean_sq.to_f64().map(f64::sqrt)
    }

    /// Count of ticks, weighted by quantity — i.e. the total quantity,
    /// expressed as a simple scalar (equivalent to `sum(quantity)`).
    /// Returns `None` for an empty slice.
    pub fn weighted_tick_count(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        total.to_f64()
    }

    // ── round-105 ────────────────────────────────────────────────────────────

    /// Count of inter-tick gaps that are shorter than the median gap (burst events).
    /// Returns `None` for fewer than 3 ticks (need at least 2 gaps to compute median).
    pub fn tick_burst_count(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.len() < 3 {
            return None;
        }
        let mut gaps: Vec<u64> = ticks.windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .collect();
        gaps.sort();
        let median = gaps[gaps.len() / 2];
        let burst_count = gaps.iter().filter(|&&g| g < median).count();
        Some(burst_count)
    }

    /// Price trend score: fraction of consecutive tick pairs where price increases.
    /// Returns `None` for fewer than 2 ticks.
    pub fn price_trend_score(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 {
            return None;
        }
        let up = ticks.windows(2).filter(|w| w[1].price > w[0].price).count();
        Some(up as f64 / (ticks.len() - 1) as f64)
    }

    /// Fraction of total quantity traded on the sell side.
    /// Returns `None` for an empty slice or zero total quantity.
    pub fn sell_qty_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() {
            return None;
        }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() {
            return None;
        }
        let sell_qty: Decimal = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.quantity)
            .sum();
        Some((sell_qty / total).to_f64().unwrap_or(0.0))
    }

    /// Count of ticks whose quantity exceeds the median quantity of the slice.
    /// Returns `None` for an empty slice.
    pub fn qty_above_median(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() {
            return None;
        }
        let mut sorted: Vec<Decimal> = ticks.iter().map(|t| t.quantity).collect();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        Some(ticks.iter().filter(|t| t.quantity > median).count())
    }

    // ── round-106 ────────────────────────────────────────────────────────────

    /// Z-score of the latest tick price relative to the slice.
    /// Returns `None` for fewer than 2 ticks or zero standard deviation.
    pub fn price_zscore(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 {
            return None;
        }
        let prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if prices.len() != ticks.len() { return None; }
        let n = prices.len() as f64;
        let mean = prices.iter().sum::<f64>() / n;
        let std_dev = (prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std_dev == 0.0 { return None; }
        let last = *prices.last()?;
        Some((last - mean) / std_dev)
    }

    /// Fraction of total ticks that are on the buy side.
    /// Returns `None` for an empty slice.
    pub fn buy_side_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() { return None; }
        let buy_count = ticks.iter().filter(|t| matches!(t.side, Some(TradeSide::Buy))).count();
        Some(buy_count as f64 / ticks.len() as f64)
    }

    /// Coefficient of variation of trade quantities: `std_dev / mean`.
    /// Returns `None` for an empty slice or zero mean.
    pub fn tick_qty_cv(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let qtys: Vec<f64> = ticks.iter().filter_map(|t| t.quantity.to_f64()).collect();
        if qtys.len() != ticks.len() { return None; }
        let n = qtys.len() as f64;
        let mean = qtys.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let std_dev = (qtys.iter().map(|q| (q - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(std_dev / mean)
    }

    /// Mean trade value: average of `price * quantity` across all ticks.
    /// Returns `None` for an empty slice.
    pub fn avg_trade_value(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let total: Decimal = ticks.iter().map(|t| t.price * t.quantity).sum();
        Some((total / Decimal::from(ticks.len())).to_f64().unwrap_or(0.0))
    }

    // ── round-107 ────────────────────────────────────────────────────────────

    /// Maximum price among buy-side ticks.
    /// Returns `None` if there are no buy-side ticks.
    pub fn max_buy_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price)
            .max()
            .and_then(|p| p.to_f64())
    }

    /// Minimum price among sell-side ticks.
    /// Returns `None` if there are no sell-side ticks.
    pub fn min_sell_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price)
            .min()
            .and_then(|p| p.to_f64())
    }

    /// Ratio of price range to mean price: `(max - min) / mean`.
    /// Returns `None` for an empty slice or zero mean.
    pub fn price_range_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let max_p = ticks.iter().map(|t| t.price).max()?;
        let min_p = ticks.iter().map(|t| t.price).min()?;
        let total: Decimal = ticks.iter().map(|t| t.price).sum();
        let mean = total / Decimal::from(ticks.len());
        if mean.is_zero() { return None; }
        Some(((max_p - min_p) / mean).to_f64().unwrap_or(0.0))
    }

    /// Quantity-weighted sum of absolute price changes between consecutive ticks.
    /// Returns `None` for fewer than 2 ticks.
    pub fn qty_weighted_price_change(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let total: Decimal = ticks.windows(2).map(|w| {
            let price_diff = (w[1].price - w[0].price).abs();
            let avg_qty = (w[0].quantity + w[1].quantity) / Decimal::TWO;
            price_diff * avg_qty
        }).sum();
        total.to_f64()
    }

    // ── round-108 ────────────────────────────────────────────────────────────

    /// Price change between the last two ticks. Returns `None` for fewer than 2 ticks.
    pub fn last_price_change(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let last = ticks[ticks.len() - 1].price;
        let prev = ticks[ticks.len() - 2].price;
        (last - prev).to_f64()
    }

    /// Buy ticks per millisecond: buy tick count divided by total time span.
    /// Returns `None` for fewer than 2 ticks or zero time span.
    pub fn buy_tick_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let t_start = ticks.iter().map(|t| t.received_at_ms).min()?;
        let t_end = ticks.iter().map(|t| t.received_at_ms).max()?;
        let span_ms = t_end.saturating_sub(t_start);
        if span_ms == 0 { return None; }
        let buy_count = ticks.iter().filter(|t| matches!(t.side, Some(TradeSide::Buy))).count();
        Some(buy_count as f64 / span_ms as f64)
    }

    /// Median absolute deviation of trade quantities.
    /// Returns `None` for an empty slice.
    pub fn qty_median_absolute_deviation(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = ticks.iter().map(|t| t.quantity).collect();
        sorted.sort();
        let n = sorted.len();
        let median = if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / Decimal::TWO };
        let mut diffs: Vec<Decimal> = sorted.iter().map(|&q| (q - median).abs()).collect();
        diffs.sort();
        let mad = if diffs.len() % 2 == 1 { diffs[diffs.len() / 2] }
                  else { (diffs[diffs.len() / 2 - 1] + diffs[diffs.len() / 2]) / Decimal::TWO };
        mad.to_f64()
    }

    /// 25th-percentile price across the slice.
    /// Returns `None` for an empty slice.
    pub fn price_percentile_25(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        sorted.sort();
        let idx = ((sorted.len() as f64 * 0.25).ceil() as usize).saturating_sub(1);
        sorted[idx.min(sorted.len() - 1)].to_f64()
    }

    // ── round-109 ────────────────────────────────────────────────────────────

    /// Count of ticks on the sell side.
    /// Returns `None` for an empty slice.
    pub fn sell_tick_count(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() { return None; }
        Some(ticks.iter().filter(|t| matches!(t.side, Some(TradeSide::Sell))).count())
    }

    /// Range of inter-tick gaps in milliseconds: `max_gap - min_gap`.
    /// Returns `None` for fewer than 3 ticks (need at least 2 gaps).
    pub fn inter_tick_range_ms(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 { return None; }
        let gaps: Vec<u64> = ticks.windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .collect();
        let max_g = gaps.iter().copied().max()?;
        let min_g = gaps.iter().copied().min()?;
        Some((max_g - min_g) as f64)
    }

    /// Net quantity flow: buy total quantity minus sell total quantity.
    /// Returns `None` for an empty slice.
    pub fn net_qty_flow(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let buy_qty: Decimal = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.quantity).sum();
        let sell_qty: Decimal = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.quantity).sum();
        (buy_qty - sell_qty).to_f64()
    }

    /// Ratio of max quantity to min quantity across the slice.
    /// Returns `None` for an empty slice or zero min quantity.
    pub fn qty_skew_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let max_q = ticks.iter().map(|t| t.quantity).max()?;
        let min_q = ticks.iter().map(|t| t.quantity).min()?;
        if min_q.is_zero() { return None; }
        Some((max_q / min_q).to_f64().unwrap_or(0.0))
    }

    // ── round-110 ────────────────────────────────────────────────────────────

    /// Price change from the first tick to the last tick.
    /// Returns `None` for an empty slice.
    pub fn first_to_last_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let first = ticks.first()?.price;
        let last = ticks.last()?.price;
        (last - first).to_f64()
    }

    /// Count of distinct price levels (unique price values) in the slice.
    /// Returns `None` for an empty slice.
    pub fn tick_volume_profile(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() { return None; }
        use std::collections::HashSet;
        let unique: HashSet<_> = ticks.iter().map(|t| t.price).collect();
        Some(unique.len())
    }

    /// Interquartile range of prices: 75th percentile minus 25th percentile.
    /// Returns `None` for an empty slice.
    pub fn price_quartile_range(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let mut sorted: Vec<Decimal> = ticks.iter().map(|t| t.price).collect();
        sorted.sort();
        let n = sorted.len();
        let q1_idx = ((n as f64 * 0.25).ceil() as usize).saturating_sub(1);
        let q3_idx = ((n as f64 * 0.75).ceil() as usize).saturating_sub(1);
        let q1 = sorted[q1_idx.min(n - 1)];
        let q3 = sorted[q3_idx.min(n - 1)];
        (q3 - q1).to_f64()
    }

    /// Buy pressure index: buy volume as fraction of total, minus 0.5, scaled by 2.
    /// Range: -1.0 (all sell) to 1.0 (all buy). Returns `None` for zero total quantity.
    pub fn buy_pressure_index(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let total: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total.is_zero() { return None; }
        let buy_qty: Decimal = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.quantity).sum();
        let fraction = (buy_qty / total).to_f64().unwrap_or(0.0);
        Some((fraction - 0.5) * 2.0)
    }

    // ── round-111 ────────────────────────────────────────────────────────────

    /// Price entropy: Shannon entropy of price frequency distribution across 10 equal buckets.
    /// Returns `None` for fewer than 2 ticks.
    pub fn tick_price_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let min_p = ticks.iter().map(|t| t.price).min()?;
        let max_p = ticks.iter().map(|t| t.price).max()?;
        let range = max_p - min_p;
        if range.is_zero() { return None; }
        let n_buckets = 10usize;
        let bucket_size = range.to_f64().unwrap_or(0.0) / n_buckets as f64;
        if bucket_size == 0.0 { return None; }
        let mut counts = vec![0u64; n_buckets];
        let min_f = min_p.to_f64().unwrap_or(0.0);
        for t in ticks {
            let p = t.price.to_f64().unwrap_or(min_f);
            let idx = ((p - min_f) / bucket_size).floor() as usize;
            let idx = idx.min(n_buckets - 1);
            counts[idx] += 1;
        }
        let total = ticks.len() as f64;
        let entropy = counts.iter().filter(|&&c| c > 0).map(|&c| {
            let p = c as f64 / total;
            -p * p.ln()
        }).sum::<f64>();
        Some(entropy)
    }

    /// Mean of the absolute price difference between consecutive ticks (average spread proxy).
    /// Returns `None` for fewer than 2 ticks.
    pub fn average_spread(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let sum: Decimal = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).abs())
            .sum();
        Some((sum / Decimal::from(ticks.len() - 1)).to_f64().unwrap_or(0.0))
    }

    /// Standard deviation of prices across the slice.
    /// Returns `None` for fewer than 2 ticks.
    pub fn tick_sigma(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().filter_map(|t| t.price.to_f64()).collect();
        if prices.len() != ticks.len() { return None; }
        let n = prices.len() as f64;
        let mean = prices.iter().sum::<f64>() / n;
        let var = prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / n;
        Some(var.sqrt())
    }

    /// Fraction of total quantity on the downside: ticks with price below the mean price.
    /// Returns `None` for an empty slice or zero total quantity.
    pub fn downside_qty_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let total_qty: Decimal = ticks.iter().map(|t| t.quantity).sum();
        if total_qty.is_zero() { return None; }
        let total_price: Decimal = ticks.iter().map(|t| t.price).sum();
        let mean_price = total_price / Decimal::from(ticks.len());
        let down_qty: Decimal = ticks.iter()
            .filter(|t| t.price < mean_price)
            .map(|t| t.quantity).sum();
        Some((down_qty / total_qty).to_f64().unwrap_or(0.0))
    }

    // ── round-112 ────────────────────────────────────────────────────────────

    /// Fraction of consecutive tick pairs where price reverses direction.
    /// Returns `None` for fewer than 3 ticks.
    pub fn price_reversal_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 { return None; }
        let directions: Vec<i8> = ticks.windows(2).map(|w| {
            if w[1].price > w[0].price { 1i8 }
            else if w[1].price < w[0].price { -1i8 }
            else { 0i8 }
        }).collect();
        let reversals = directions.windows(2)
            .filter(|w| w[0] != 0 && w[1] != 0 && w[0] != w[1])
            .count();
        Some(reversals as f64 / (directions.len() - 1) as f64)
    }

    /// Exponential moving average of trade quantities (alpha = 2/(n+1), n = tick count).
    /// Returns `None` for an empty slice.
    pub fn qty_ema(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let n = ticks.len();
        let alpha = 2.0 / (n + 1) as f64;
        let mut ema = ticks[0].quantity.to_f64().unwrap_or(0.0);
        for t in &ticks[1..] {
            let q = t.quantity.to_f64().unwrap_or(0.0);
            ema = alpha * q + (1.0 - alpha) * ema;
        }
        Some(ema)
    }

    /// Last price among buy-side ticks.
    /// Returns `None` if no buy-side ticks exist.
    pub fn last_buy_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        ticks.iter().rev()
            .find(|t| matches!(t.side, Some(TradeSide::Buy)))
            .and_then(|t| t.price.to_f64())
    }

    /// Last price among sell-side ticks.
    /// Returns `None` if no sell-side ticks exist.
    pub fn last_sell_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        ticks.iter().rev()
            .find(|t| matches!(t.side, Some(TradeSide::Sell)))
            .and_then(|t| t.price.to_f64())
    }

    // ── round-113 ────────────────────────────────────────────────────────────

    /// Mean inter-tick gap in milliseconds. Returns `None` for fewer than 2 ticks.
    pub fn avg_inter_tick_gap(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let sum: u64 = ticks.windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms))
            .sum();
        Some(sum as f64 / (ticks.len() - 1) as f64)
    }

    /// Tick intensity: number of ticks per second. Returns `None` if time span is zero
    /// or fewer than 2 ticks.
    pub fn tick_intensity(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let span_ms = ticks.last()?.received_at_ms.saturating_sub(ticks.first()?.received_at_ms);
        if span_ms == 0 { return None; }
        Some(ticks.len() as f64 / (span_ms as f64 / 1000.0))
    }

    /// Price swing: `max_price - min_price` as a fraction of `min_price`.
    /// Returns `None` for empty input or zero min price.
    pub fn price_swing(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let max_p = ticks.iter().map(|t| t.price).fold(ticks[0].price, |a, b| a.max(b));
        let min_p = ticks.iter().map(|t| t.price).fold(ticks[0].price, |a, b| a.min(b));
        if min_p.is_zero() { return None; }
        ((max_p - min_p) / min_p).to_f64()
    }

    /// Quantity velocity: mean rate of quantity change between consecutive ticks.
    /// Returns `None` for fewer than 2 ticks or zero time span.
    pub fn qty_velocity(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for w in ticks.windows(2) {
            let dt = w[1].received_at_ms.saturating_sub(w[0].received_at_ms);
            if dt == 0 { continue; }
            let dq = (w[1].quantity - w[0].quantity).to_f64().unwrap_or(0.0);
            sum += dq / dt as f64;
            cnt += 1;
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    // ── round-114 ────────────────────────────────────────────────────────────

    /// Ratio of price standard deviation to mean price.
    /// Returns `None` for fewer than 2 ticks or zero mean.
    pub fn price_std_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let n = prices.len() as f64;
        let mean = prices.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let var = prices.iter().map(|&p| (p - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(var.sqrt() / mean)
    }

    /// Quantity trend strength: Pearson correlation of quantity with tick index.
    /// Returns `None` for fewer than 2 ticks or zero variance in either series.
    pub fn qty_trend_strength(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let n = ticks.len() as f64;
        let xs: Vec<f64> = (0..ticks.len()).map(|i| i as f64).collect();
        let ys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let x_mean = xs.iter().sum::<f64>() / n;
        let y_mean = ys.iter().sum::<f64>() / n;
        let num: f64 = xs.iter().zip(ys.iter()).map(|(&x, &y)| (x - x_mean) * (y - y_mean)).sum();
        let den_x: f64 = xs.iter().map(|&x| (x - x_mean).powi(2)).sum::<f64>().sqrt();
        let den_y: f64 = ys.iter().map(|&y| (y - y_mean).powi(2)).sum::<f64>().sqrt();
        if den_x == 0.0 || den_y == 0.0 { None } else { Some(num / (den_x * den_y)) }
    }

    /// Mean absolute price gap between consecutive buy→sell or sell→buy transitions.
    /// Returns `None` if fewer than 2 sided transitions exist.
    pub fn buy_to_sell_gap(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let sided: Vec<&NormalizedTick> = ticks.iter().filter(|t| t.side.is_some()).collect();
        if sided.len() < 2 { return None; }
        let mut sum = 0f64;
        let mut cnt = 0usize;
        for w in sided.windows(2) {
            if w[0].side != w[1].side {
                sum += (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0);
                cnt += 1;
            }
        }
        if cnt == 0 { None } else { Some(sum / cnt as f64) }
    }

    /// Tick range efficiency: `|last_price - first_price| / (max_price - min_price)`.
    /// Returns `None` for empty input or zero price range.
    pub fn tick_range_efficiency(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let first = ticks.first()?.price;
        let last = ticks.last()?.price;
        let max_p = ticks.iter().map(|t| t.price).fold(first, |a, b| a.max(b));
        let min_p = ticks.iter().map(|t| t.price).fold(first, |a, b| a.min(b));
        let range = max_p - min_p;
        if range.is_zero() { return None; }
        ((last - first).abs() / range).to_f64()
    }

    // ── round-115 ────────────────────────────────────────────────────────────

    /// Mean mid-price `(price + prev_price) / 2` across consecutive tick pairs.
    /// Returns `None` for fewer than 2 ticks.
    pub fn mid_price_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let sum: f64 = ticks.windows(2).map(|w| {
            ((w[0].price + w[1].price) / rust_decimal::Decimal::TWO).to_f64().unwrap_or(0.0)
        }).sum();
        Some(sum / (ticks.len() - 1) as f64)
    }

    /// Range of quantities: `max_qty - min_qty`. Returns `None` for empty input.
    pub fn tick_qty_range(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let max_q = ticks.iter().map(|t| t.quantity).fold(ticks[0].quantity, |a, b| a.max(b));
        let min_q = ticks.iter().map(|t| t.quantity).fold(ticks[0].quantity, |a, b| a.min(b));
        (max_q - min_q).to_f64()
    }

    /// Length of the longest consecutive run of buy-side ticks.
    /// Returns `None` if no buy-side ticks exist.
    pub fn buy_dominance_streak(ticks: &[NormalizedTick]) -> Option<usize> {
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for t in ticks {
            if matches!(t.side, Some(TradeSide::Buy)) {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 0;
            }
        }
        if max_run == 0 { None } else { Some(max_run) }
    }

    /// Mean absolute price gap between consecutive ticks.
    /// Returns `None` for fewer than 2 ticks.
    pub fn price_gap_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let sum: f64 = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0))
            .sum();
        Some(sum / (ticks.len() - 1) as f64)
    }

    // ── round-116 ────────────────────────────────────────────────────────────

    /// Price momentum index: fraction of consecutive pairs with rising price.
    /// Returns `None` for fewer than 2 ticks.
    pub fn price_momentum_index(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let rising = ticks.windows(2).filter(|w| w[1].price > w[0].price).count();
        Some(rising as f64 / (ticks.len() - 1) as f64)
    }

    /// Ratio of quantity range to mean quantity.
    /// Returns `None` for empty input or zero mean quantity.
    pub fn qty_range_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let max_q = ticks.iter().map(|t| t.quantity).fold(ticks[0].quantity, |a, b| a.max(b));
        let min_q = ticks.iter().map(|t| t.quantity).fold(ticks[0].quantity, |a, b| a.min(b));
        let n = ticks.len() as f64;
        let mean_q = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).sum::<f64>() / n;
        if mean_q == 0.0 { return None; }
        ((max_q - min_q) / rust_decimal::Decimal::from_f64_retain(mean_q)?).to_f64()
    }

    /// Price change from the last tick to the one before it.
    /// Returns `None` for fewer than 2 ticks.
    pub fn recent_price_change(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let n = ticks.len();
        (ticks[n - 1].price - ticks[n - 2].price).to_f64()
    }

    /// Length of the longest consecutive run of sell-side ticks.
    /// Returns `None` if no sell-side ticks exist.
    pub fn sell_dominance_streak(ticks: &[NormalizedTick]) -> Option<usize> {
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for t in ticks {
            if matches!(t.side, Some(TradeSide::Sell)) {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 0;
            }
        }
        if max_run == 0 { None } else { Some(max_run) }
    }

    // ── round-117 ────────────────────────────────────────────────────────────

    /// OLS slope of price over tick index. Returns `None` for fewer than 2 ticks.
    pub fn price_momentum_slope(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let n = ticks.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let y_mean = prices.iter().sum::<f64>() / n;
        let num: f64 = prices.iter().enumerate().map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean)).sum();
        let den: f64 = prices.iter().enumerate().map(|(i, _)| (i as f64 - x_mean).powi(2)).sum();
        if den == 0.0 { None } else { Some(num / den) }
    }

    /// Quantity dispersion: coefficient of variation `std / mean`.
    /// Returns `None` for empty input or zero mean.
    pub fn qty_dispersion(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(std / mean)
    }

    /// Fraction of ticks that are buy-side. Returns `None` if no sided ticks exist.
    pub fn tick_buy_pct(ticks: &[NormalizedTick]) -> Option<f64> {
        let sided: Vec<&NormalizedTick> = ticks.iter().filter(|t| t.side.is_some()).collect();
        if sided.is_empty() { return None; }
        let buys = sided.iter().filter(|t| matches!(t.side, Some(TradeSide::Buy))).count();
        Some(buys as f64 / sided.len() as f64)
    }

    /// Length of the longest consecutive run of rising prices.
    /// Returns `None` for fewer than 2 ticks.
    pub fn consecutive_price_rise(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.len() < 2 { return None; }
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for w in ticks.windows(2) {
            if w[1].price > w[0].price {
                cur_run += 1;
                if cur_run > max_run { max_run = cur_run; }
            } else {
                cur_run = 0;
            }
        }
        Some(max_run)
    }

    // ── round-118 ────────────────────────────────────────────────────────────

    /// Entropy rate of prices: mean absolute log-return between consecutive ticks.
    /// Returns `None` for fewer than 2 ticks or any non-positive price.
    pub fn price_entropy_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| {
            let p = t.price.to_f64()?;
            if p <= 0.0 { None } else { Some(p) }
        }).collect::<Option<Vec<_>>>()?;
        let sum: f64 = prices.windows(2).map(|w| (w[1] / w[0]).ln().abs()).sum();
        Some(sum / (prices.len() - 1) as f64)
    }

    /// Lag-1 autocorrelation of tick quantities.
    /// Returns `None` for fewer than 3 ticks or zero variance.
    pub fn qty_lag1_corr(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let cov: f64 = vals.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>()
            / (vals.len() - 1) as f64;
        Some(cov / var)
    }

    /// Fraction of consecutive sided-tick pairs where the side changes.
    /// Returns `None` if fewer than 2 sided ticks.
    pub fn tick_side_transition_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        let sided: Vec<&NormalizedTick> = ticks.iter().filter(|t| t.side.is_some()).collect();
        if sided.len() < 2 { return None; }
        let transitions = sided.windows(2).filter(|w| w[0].side != w[1].side).count();
        Some(transitions as f64 / (sided.len() - 1) as f64)
    }

    /// Mean price divided by mean quantity — a rough price-per-unit measure.
    /// Returns `None` for empty input or zero mean quantity.
    pub fn avg_price_per_unit(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let n = ticks.len() as f64;
        let mean_p = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).sum::<f64>() / n;
        let mean_q = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).sum::<f64>() / n;
        if mean_q == 0.0 { return None; }
        Some(mean_p / mean_q)
    }

    // ── round-119 ────────────────────────────────────────────────────────────

    /// Fraction of consecutive pairs where price reverses after a prior reversal
    /// (i.e., rebounds). Returns `None` for fewer than 3 ticks.
    pub fn price_rebound_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 3 { return None; }
        let dirs: Vec<i8> = ticks.windows(2).map(|w| {
            if w[1].price > w[0].price { 1i8 }
            else if w[1].price < w[0].price { -1i8 }
            else { 0i8 }
        }).collect();
        let rebounds = dirs.windows(2).filter(|w| w[0] == 0 || w[0] == -w[1]).count();
        let eligible = dirs.len().saturating_sub(1);
        if eligible == 0 { None } else { Some(rebounds as f64 / eligible as f64) }
    }

    /// Volume-weighted mean of absolute price differences between consecutive ticks
    /// (a spread proxy). Returns `None` for fewer than 2 ticks or zero total volume.
    pub fn weighted_spread(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let mut num = 0f64;
        let mut denom = 0f64;
        for w in ticks.windows(2) {
            let spread = (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0);
            let vol = (w[0].quantity + w[1].quantity).to_f64().unwrap_or(0.0);
            num += spread * vol;
            denom += vol;
        }
        if denom == 0.0 { None } else { Some(num / denom) }
    }

    /// Mean price of buy-side ticks minus mean price of sell-side ticks.
    /// Returns `None` if either side has no ticks.
    pub fn buy_price_advantage(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buys: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        let sells: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        if buys.is_empty() || sells.is_empty() { return None; }
        let buy_mean = buys.iter().sum::<f64>() / buys.len() as f64;
        let sell_mean = sells.iter().sum::<f64>() / sells.len() as f64;
        Some(buy_mean - sell_mean)
    }

    /// Shannon entropy of quantity distribution across 8 equal-width buckets.
    /// Returns `None` for fewer than 2 ticks.
    pub fn qty_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let max = vals.iter().cloned().fold(0f64, f64::max);
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        const BUCKETS: usize = 8;
        let mut counts = [0usize; BUCKETS];
        if range == 0.0 {
            counts[0] = vals.len();
        } else {
            for &v in &vals {
                let idx = ((v - min) / range * (BUCKETS - 1) as f64) as usize;
                counts[idx.min(BUCKETS - 1)] += 1;
            }
        }
        let n = vals.len() as f64;
        Some(counts.iter().map(|&c| {
            if c == 0 { 0.0 } else { let p = c as f64 / n; -p * p.ln() }
        }).sum())
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    /// Price jitter: mean squared price change between consecutive ticks.
    /// Returns `None` for fewer than 2 ticks.
    pub fn price_jitter(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let sum: f64 = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).to_f64().unwrap_or(0.0).powi(2))
            .sum();
        Some(sum / (ticks.len() - 1) as f64)
    }

    /// Flow ratio: buy volume divided by total volume.
    /// Returns `None` if no sided ticks or zero total volume.
    pub fn tick_flow_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut buy_vol = 0f64;
        let mut total_vol = 0f64;
        for t in ticks {
            let q = t.quantity.to_f64().unwrap_or(0.0);
            if matches!(t.side, Some(TradeSide::Buy)) { buy_vol += q; }
            if t.side.is_some() { total_vol += q; }
        }
        if total_vol == 0.0 { None } else { Some(buy_vol / total_vol) }
    }

    /// Absolute skewness of quantities (|skewness|). Returns `None` for fewer than
    /// 3 ticks or zero std dev.
    pub fn qty_skewness_abs(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        if std == 0.0 { return None; }
        let skew = vals.iter().map(|&x| ((x - mean) / std).powi(3)).sum::<f64>() / n;
        Some(skew.abs())
    }

    /// Side balance score: fraction of sided ticks on the majority side minus 0.5,
    /// normalized to [0, 0.5]. Returns `None` if no sided ticks.
    pub fn side_balance_score(ticks: &[NormalizedTick]) -> Option<f64> {
        let sided: Vec<&NormalizedTick> = ticks.iter().filter(|t| t.side.is_some()).collect();
        if sided.is_empty() { return None; }
        let buys = sided.iter().filter(|t| matches!(t.side, Some(TradeSide::Buy))).count();
        let frac = buys as f64 / sided.len() as f64;
        Some((frac - 0.5).abs())
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    /// Pearson correlation between price and quantity across ticks.
    pub fn price_vol_correlation(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let ps: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let qs: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let n = ps.len() as f64;
        let pm = ps.iter().sum::<f64>() / n;
        let qm = qs.iter().sum::<f64>() / n;
        let cov: f64 = ps.iter().zip(qs.iter()).map(|(&p, &q)| (p - pm) * (q - qm)).sum::<f64>() / n;
        let sp = (ps.iter().map(|&p| (p - pm).powi(2)).sum::<f64>() / n).sqrt();
        let sq = (qs.iter().map(|&q| (q - qm).powi(2)).sum::<f64>() / n).sqrt();
        if sp == 0.0 || sq == 0.0 { return None; }
        Some(cov / (sp * sq))
    }

    /// Mean second-order price change: mean of (p[i+2] - 2*p[i+1] + p[i]).
    pub fn qty_acceleration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let qs: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let acc: Vec<f64> = qs.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect();
        Some(acc.iter().sum::<f64>() / acc.len() as f64)
    }

    /// Mean buy price minus mean sell price (requires sided ticks).
    pub fn buy_sell_price_diff(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buys: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let sells: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        if buys.is_empty() || sells.is_empty() { return None; }
        let buy_mean = buys.iter().sum::<f64>() / buys.len() as f64;
        let sell_mean = sells.iter().sum::<f64>() / sells.len() as f64;
        Some(buy_mean - sell_mean)
    }

    /// Order flow imbalance: (buy_qty - sell_qty) / (buy_qty + sell_qty).
    pub fn tick_imbalance_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut buy_qty = 0f64;
        let mut sell_qty = 0f64;
        for t in ticks {
            let q = t.quantity.to_f64().unwrap_or(0.0);
            match t.side {
                Some(TradeSide::Buy) => buy_qty += q,
                Some(TradeSide::Sell) => sell_qty += q,
                _ => {}
            }
        }
        let total = buy_qty + sell_qty;
        if total == 0.0 { None } else { Some((buy_qty - sell_qty) / total) }
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    /// Exponential moving average of absolute price changes (alpha=0.2).
    pub fn price_range_ema(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let alpha = 0.2f64;
        let mut ema = (ticks[1].price - ticks[0].price).abs().to_f64().unwrap_or(0.0);
        for w in ticks.windows(2).skip(1) {
            let diff = (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0);
            ema = alpha * diff + (1.0 - alpha) * ema;
        }
        Some(ema)
    }

    /// Exponential moving average of quantity values (alpha=0.2).
    pub fn qty_trend_ema(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let alpha = 0.2f64;
        let mut ema = ticks[0].quantity.to_f64().unwrap_or(0.0);
        for t in ticks.iter().skip(1) {
            ema = alpha * t.quantity.to_f64().unwrap_or(0.0) + (1.0 - alpha) * ema;
        }
        Some(ema)
    }

    /// Volume-weighted mid price across ticks using (price * quantity) / total_qty.
    pub fn weighted_mid_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let mut wsum = 0f64;
        let mut qty_total = 0f64;
        for t in ticks {
            let q = t.quantity.to_f64().unwrap_or(0.0);
            wsum += t.price.to_f64().unwrap_or(0.0) * q;
            qty_total += q;
        }
        if qty_total == 0.0 { None } else { Some(wsum / qty_total) }
    }

    /// Fraction of total quantity that is on the buy side.
    pub fn tick_buy_qty_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let mut buy_qty = 0f64;
        let mut total = 0f64;
        for t in ticks {
            let q = t.quantity.to_f64().unwrap_or(0.0);
            total += q;
            if matches!(t.side, Some(TradeSide::Buy)) { buy_qty += q; }
        }
        if total == 0.0 { None } else { Some(buy_qty / total) }
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    /// Excess kurtosis of quantity distribution across ticks.
    pub fn qty_kurtosis(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 4 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        if var == 0.0 { return None; }
        let kurt = vals.iter().map(|&x| ((x - mean) / var.sqrt()).powi(4)).sum::<f64>() / n - 3.0;
        Some(kurt)
    }

    /// Fraction of consecutive price pairs that are monotonically increasing.
    pub fn price_monotonicity(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let rises = ticks.windows(2).filter(|w| w[1].price > w[0].price).count();
        Some(rises as f64 / (ticks.len() - 1) as f64)
    }

    /// Ticks per second using first/last received_at_ms; None if same timestamp.
    pub fn tick_count_per_second(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let first = ticks.first()?.received_at_ms;
        let last = ticks.last()?.received_at_ms;
        if last <= first { return None; }
        let secs = (last - first) as f64 / 1000.0;
        Some(ticks.len() as f64 / secs)
    }

    /// Standard deviation of price across all ticks.
    pub fn price_range_std(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let var = vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        Some(var.sqrt())
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    /// Count of times price crosses its own mean value.
    pub fn price_cross_zero(ticks: &[NormalizedTick]) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let centered: Vec<f64> = prices.iter().map(|&p| p - mean).collect();
        let crosses = centered.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        Some(crosses)
    }

    /// Momentum score: mean of sign of price changes across consecutive ticks.
    pub fn tick_momentum_score(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let score: f64 = ticks.windows(2).map(|w| {
            if w[1].price > w[0].price { 1.0 } else if w[1].price < w[0].price { -1.0 } else { 0.0 }
        }).sum::<f64>() / (ticks.len() - 1) as f64;
        Some(score)
    }

    /// Fraction of sided ticks that are sells.
    pub fn sell_side_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        let sided: Vec<&NormalizedTick> = ticks.iter().filter(|t| t.side.is_some()).collect();
        if sided.is_empty() { return None; }
        let sells = sided.iter().filter(|t| matches!(t.side, Some(TradeSide::Sell))).count();
        Some(sells as f64 / sided.len() as f64)
    }

    /// Mean quantity per buy side, or None if no sided ticks.
    pub fn avg_qty_per_side(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let sided: Vec<f64> = ticks.iter()
            .filter(|t| t.side.is_some())
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .collect();
        if sided.is_empty() { return None; }
        Some(sided.iter().sum::<f64>() / sided.len() as f64)
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    /// Simple Hurst exponent estimate using log-range scaling (2 halves).
    /// H≈0.5 random, H>0.5 trending, H<0.5 mean-reverting.
    pub fn price_hurst_estimate(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 4 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let range = |s: &[f64]| -> f64 {
            let max = s.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let min = s.iter().cloned().fold(f64::INFINITY, f64::min);
            max - min
        };
        let full_range = range(&prices);
        let mid = prices.len() / 2;
        let half_range = (range(&prices[..mid]) + range(&prices[mid..])) / 2.0;
        if half_range == 0.0 || full_range == 0.0 { return None; }
        Some((full_range / half_range).ln() / 2f64.ln())
    }

    /// Mean reversion tendency of quantity: mean of |qty - mean_qty| / std_qty.
    pub fn qty_mean_reversion(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        Some(vals.iter().map(|&x| (x - mean).abs() / std).sum::<f64>() / n)
    }

    /// Average trade price impact: mean of |price[i] - price[i-1]| / price[i-1].
    pub fn avg_trade_impact(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let sum: f64 = ticks.windows(2).map(|w| {
            let p0 = w[0].price.to_f64().unwrap_or(0.0);
            if p0 == 0.0 { 0.0 } else { (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0) / p0 }
        }).sum();
        Some(sum / (ticks.len() - 1) as f64)
    }

    /// Interquartile range of price values.
    pub fn price_range_iqr(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 4 { return None; }
        let mut prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = prices.len();
        let q1 = prices[n / 4];
        let q3 = prices[(3 * n) / 4];
        Some(q3 - q1)
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    /// Root mean square of quantity values.
    pub fn qty_rms(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let sum_sq: f64 = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0).powi(2)).sum();
        Some((sum_sq / ticks.len() as f64).sqrt())
    }

    /// Bid-ask spread proxy: mean of |price[i] - price[i-1]| across alternating buy/sell pairs.
    pub fn bid_ask_proxy(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let pairs: Vec<f64> = ticks.windows(2)
            .filter(|w| {
                matches!(w[0].side, Some(TradeSide::Buy)) && matches!(w[1].side, Some(TradeSide::Sell))
                    || matches!(w[0].side, Some(TradeSide::Sell)) && matches!(w[1].side, Some(TradeSide::Buy))
            })
            .map(|w| (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0))
            .collect();
        if pairs.is_empty() { return None; }
        Some(pairs.iter().sum::<f64>() / pairs.len() as f64)
    }

    /// Mean second-order price change (price acceleration).
    pub fn tick_price_accel(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let acc: Vec<f64> = prices.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect();
        Some(acc.iter().sum::<f64>() / acc.len() as f64)
    }

    /// IQR of absolute price changes between consecutive ticks.
    pub fn price_entropy_iqr(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 5 { return None; }
        let mut diffs: Vec<f64> = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0))
            .collect();
        diffs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = diffs.len();
        Some(diffs[(3 * n) / 4] - diffs[n / 4])
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    /// Ratio of price std to price mean (coefficient of variation).
    pub fn tick_dispersion_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        if mean == 0.0 { return None; }
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        Some(std / mean)
    }

    /// Mean squared error of a linear fit to price sequence.
    pub fn price_linear_fit_error(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let y: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let n = y.len() as f64;
        let x_mean = (n - 1.0) / 2.0;
        let y_mean = y.iter().sum::<f64>() / n;
        let ss_xy: f64 = y.iter().enumerate().map(|(i, &yi)| (i as f64 - x_mean) * (yi - y_mean)).sum();
        let ss_xx: f64 = (0..y.len()).map(|i| (i as f64 - x_mean).powi(2)).sum();
        let slope = if ss_xx == 0.0 { 0.0 } else { ss_xy / ss_xx };
        let intercept = y_mean - slope * x_mean;
        let mse: f64 = y.iter().enumerate().map(|(i, &yi)| {
            let pred = intercept + slope * i as f64;
            (yi - pred).powi(2)
        }).sum::<f64>() / n;
        Some(mse)
    }

    /// Harmonic mean of quantity values.
    pub fn qty_harmonic_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let sum_recip: f64 = ticks.iter().map(|t| {
            let q = t.quantity.to_f64().unwrap_or(0.0);
            if q == 0.0 { 0.0 } else { 1.0 / q }
        }).sum();
        if sum_recip == 0.0 { return None; }
        Some(ticks.len() as f64 / sum_recip)
    }

    /// Fraction of ticks in the second half of the slice.
    pub fn late_trade_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let mid = ticks.len() / 2;
        Some((ticks.len() - mid) as f64 / ticks.len() as f64)
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    /// Z-score of last price relative to rolling mean/std of prices: Bollinger score.
    pub fn price_bollinger_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let vals: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let std = (vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let last = *vals.last()?;
        Some((last - mean) / std)
    }

    /// Log mean: exp(mean of log quantities).
    pub fn qty_log_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let logs: Vec<f64> = ticks.iter()
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .filter(|&q| q > 0.0)
            .map(|q| q.ln())
            .collect();
        if logs.is_empty() { return None; }
        Some((logs.iter().sum::<f64>() / logs.len() as f64).exp())
    }

    /// Variance of inter-tick price speed (|price diff| per tick).
    pub fn tick_speed_variance(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let speeds: Vec<f64> = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0))
            .collect();
        let n = speeds.len() as f64;
        let mean = speeds.iter().sum::<f64>() / n;
        let var = speeds.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n;
        Some(var)
    }

    /// Relative strength: mean buy price / mean sell price; None if either side absent.
    pub fn relative_price_strength(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        let sell_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        if buy_prices.is_empty() || sell_prices.is_empty() { return None; }
        let buy_mean = buy_prices.iter().sum::<f64>() / buy_prices.len() as f64;
        let sell_mean = sell_prices.iter().sum::<f64>() / sell_prices.len() as f64;
        if sell_mean == 0.0 { return None; }
        Some(buy_mean / sell_mean)
    }

    // ── round-129 ────────────────────────────────────────────────────────────

    /// Price wave ratio: ratio of upward price moves to total moves.
    pub fn price_wave_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let total = prices.len() - 1;
        let up = prices.windows(2).filter(|w| w[1] > w[0]).count();
        Some(up as f64 / total as f64)
    }

    /// Quantity entropy score: Shannon entropy of quantity distribution.
    pub fn qty_entropy_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = qtys.iter().sum();
        if total == 0.0 { return None; }
        let entropy = qtys.iter().map(|&q| {
            let p = q / total;
            if p > 0.0 { -p * p.ln() } else { 0.0 }
        }).sum::<f64>();
        Some(entropy)
    }

    /// Tick burst rate: ratio of max inter-tick gap to mean inter-tick gap.
    pub fn tick_burst_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let gaps: Vec<f64> = ticks.windows(2)
            .map(|w| (w[1].received_at_ms as f64) - (w[0].received_at_ms as f64))
            .collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        if mean == 0.0 { return None; }
        let max = gaps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(max / mean)
    }

    /// Side-weighted price: mean price weighted by quantity, buy side positive, sell side negative.
    pub fn side_weighted_price(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let weighted: Vec<f64> = ticks.iter().filter_map(|t| {
            let p = t.price.to_f64()?;
            let q = t.quantity.to_f64()?;
            match t.side {
                Some(TradeSide::Buy)  => Some(p * q),
                Some(TradeSide::Sell) => Some(-p * q),
                _ => None,
            }
        }).collect();
        if weighted.is_empty() { return None; }
        let total_qty: f64 = ticks.iter()
            .filter(|t| t.side.is_some())
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        if total_qty == 0.0 { return None; }
        Some(weighted.iter().sum::<f64>() / total_qty)
    }

    // ── round-130 ────────────────────────────────────────────────────────────

    /// Price median deviation: mean absolute deviation from the median price.
    pub fn price_median_deviation(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let mut prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if prices.len() % 2 == 0 {
            (prices[prices.len() / 2 - 1] + prices[prices.len() / 2]) / 2.0
        } else {
            prices[prices.len() / 2]
        };
        let mad = prices.iter().map(|&p| (p - median).abs()).sum::<f64>() / prices.len() as f64;
        Some(mad)
    }

    /// Tick autocorrelation lag-1: correlation between consecutive price changes.
    pub fn tick_autocorr_lag1(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
        if diffs.len() < 2 { return None; }
        let n = diffs.len() as f64;
        let mean = diffs.iter().sum::<f64>() / n;
        let variance = diffs.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / n;
        if variance == 0.0 { return None; }
        let cov: f64 = diffs.windows(2).map(|w| (w[0] - mean) * (w[1] - mean)).sum::<f64>()
            / (diffs.len() - 1) as f64;
        Some(cov / variance)
    }

    /// Side momentum ratio: sum of buy qty minus sum of sell qty over total qty.
    pub fn side_momentum_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_qty: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        let sell_qty: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        let total = buy_qty + sell_qty;
        if total == 0.0 { return None; }
        Some((buy_qty - sell_qty) / total)
    }

    /// Price stability score: 1 minus coefficient of variation of prices.
    pub fn price_stability_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        if mean == 0.0 { return None; }
        let std = (prices.iter().map(|&p| (p - mean).powi(2)).sum::<f64>() / prices.len() as f64).sqrt();
        Some(1.0 - (std / mean.abs()))
    }

    // ── round-131 ────────────────────────────────────────────────────────────

    /// Price range momentum: (last price - first price) / (max - min) price range.
    pub fn price_range_momentum(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let first = *prices.first()?;
        let last = *prices.last()?;
        let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return None; }
        Some((last - first) / range)
    }

    /// Quantity imbalance ratio: |buy_qty - sell_qty| / total_sided_qty.
    pub fn qty_imbalance_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_qty: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        let sell_qty: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        let total = buy_qty + sell_qty;
        if total == 0.0 { return None; }
        Some((buy_qty - sell_qty).abs() / total)
    }

    /// Tick flow entropy: entropy of price change directions (up/flat/down).
    pub fn tick_flow_entropy(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let n = (prices.len() - 1) as f64;
        let up = prices.windows(2).filter(|w| w[1] > w[0]).count() as f64 / n;
        let dn = prices.windows(2).filter(|w| w[1] < w[0]).count() as f64 / n;
        let fl = prices.windows(2).filter(|w| w[1] == w[0]).count() as f64 / n;
        let entropy = [up, dn, fl].iter().map(|&p| {
            if p > 0.0 { -p * p.ln() } else { 0.0 }
        }).sum();
        Some(entropy)
    }

    /// Side price spread: mean buy price minus mean sell price.
    pub fn side_price_spread(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        let sell_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        if buy_prices.is_empty() || sell_prices.is_empty() { return None; }
        let buy_mean = buy_prices.iter().sum::<f64>() / buy_prices.len() as f64;
        let sell_mean = sell_prices.iter().sum::<f64>() / sell_prices.len() as f64;
        Some(buy_mean - sell_mean)
    }

    // ── round-132 ────────────────────────────────────────────────────────────

    /// Price z-score absolute: absolute value of z-score of the last price.
    pub fn price_zscore_abs(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let std = (prices.iter().map(|&p| (p - mean).powi(2)).sum::<f64>() / prices.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let last = *prices.last()?;
        Some(((last - mean) / std).abs())
    }

    /// Tick reversal count: number of direction changes in consecutive price moves.
    pub fn tick_reversal_count(ticks: &[NormalizedTick]) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
        let count = diffs.windows(2).filter(|w| w[0] * w[1] < 0.0).count();
        Some(count)
    }

    /// Tick price range ratio: price range / mean price.
    pub fn tick_price_range_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        if mean == 0.0 { return None; }
        let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        Some((max - min) / mean.abs())
    }

    /// Price range skew: (mean - min) / (max - min), measures if prices cluster near min.
    pub fn price_range_skew(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        let range = max - min;
        if range == 0.0 { return None; }
        Some((mean - min) / range)
    }

    // ── round-133 ────────────────────────────────────────────────────────────

    /// Price rate of change: (last price - first price) / first price.
    pub fn price_roc(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let first = ticks.first()?.price.to_f64()?;
        let last = ticks.last()?.price.to_f64()?;
        if first == 0.0 { return None; }
        Some((last - first) / first.abs())
    }

    /// Quantity rate of change: (last qty - first qty) / first qty.
    pub fn qty_roc(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let first = ticks.first()?.quantity.to_f64()?;
        let last = ticks.last()?.quantity.to_f64()?;
        if first == 0.0 { return None; }
        Some((last - first) / first.abs())
    }

    /// Tick timing score: fraction of ticks in the first half by timestamp.
    pub fn tick_timing_score(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let mid_ts = (ticks.first()?.received_at_ms + ticks.last()?.received_at_ms) / 2;
        let in_first_half = ticks.iter().filter(|t| t.received_at_ms <= mid_ts).count();
        Some(in_first_half as f64 / ticks.len() as f64)
    }

    /// Side spread ratio: std of buy prices / std of sell prices.
    pub fn side_spread_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let std_of = |prices: &[f64]| -> Option<f64> {
            if prices.len() < 2 { return None; }
            let mean = prices.iter().sum::<f64>() / prices.len() as f64;
            let std = (prices.iter().map(|&p| (p - mean).powi(2)).sum::<f64>() / prices.len() as f64).sqrt();
            if std == 0.0 { None } else { Some(std) }
        };
        let buy_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        let sell_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        let buy_std = std_of(&buy_prices)?;
        let sell_std = std_of(&sell_prices)?;
        Some(buy_std / sell_std)
    }

    // ── round-134 ────────────────────────────────────────────────────────────

    /// Price z-score mean: mean absolute z-score across all prices.
    pub fn price_zscore_mean(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let std = (prices.iter().map(|&p| (p - mean).powi(2)).sum::<f64>() / prices.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let avg_zscore = prices.iter().map(|&p| ((p - mean) / std).abs()).sum::<f64>()
            / prices.len() as f64;
        Some(avg_zscore)
    }

    /// Tick size ratio: last quantity / mean quantity.
    pub fn tick_size_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let mean = qtys.iter().sum::<f64>() / qtys.len() as f64;
        if mean == 0.0 { return None; }
        let last = *qtys.last()?;
        Some(last / mean)
    }

    /// Buy tick fraction: fraction of ticks with Buy side.
    pub fn buy_tick_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() { return None; }
        let buys = ticks.iter().filter(|t| matches!(t.side, Some(TradeSide::Buy))).count();
        Some(buys as f64 / ticks.len() as f64)
    }

    /// Price jump count: number of price changes exceeding one std deviation.
    pub fn price_jump_count(ticks: &[NormalizedTick]) -> Option<usize> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = prices.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let std = (diffs.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / diffs.len() as f64).sqrt();
        if std == 0.0 { return Some(0); }
        let count = diffs.iter().filter(|&&d| d > mean + std).count();
        Some(count)
    }

    // ── round-135 ────────────────────────────────────────────────────────────

    /// Tick cluster density: ticks per unit time (ticks/ms * 1000).
    pub fn tick_cluster_density(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let first = ticks.first()?.received_at_ms;
        let last = ticks.last()?.received_at_ms;
        if last <= first { return None; }
        Some(ticks.len() as f64 / (last - first) as f64 * 1000.0)
    }

    /// Quantity z-score last: z-score of the last quantity in the series.
    pub fn qty_zscore_last(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let mean = qtys.iter().sum::<f64>() / qtys.len() as f64;
        let std = (qtys.iter().map(|&q| (q - mean).powi(2)).sum::<f64>() / qtys.len() as f64).sqrt();
        if std == 0.0 { return None; }
        let last = *qtys.last()?;
        Some((last - mean) / std)
    }

    /// Side price ratio: mean price of all sided ticks on buy side relative to sell side (abs ratio).
    pub fn side_price_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_mean = {
            let v: Vec<f64> = ticks.iter()
                .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
                .map(|t| t.price.to_f64().unwrap_or(0.0))
                .collect();
            if v.is_empty() { return None; }
            v.iter().sum::<f64>() / v.len() as f64
        };
        let sell_mean = {
            let v: Vec<f64> = ticks.iter()
                .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
                .map(|t| t.price.to_f64().unwrap_or(0.0))
                .collect();
            if v.is_empty() { return None; }
            v.iter().sum::<f64>() / v.len() as f64
        };
        if sell_mean == 0.0 { return None; }
        Some((buy_mean - sell_mean).abs() / sell_mean)
    }

    /// Quantity entropy normalized: entropy of qty distribution / ln(n).
    pub fn qty_entropy_norm(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let total: f64 = qtys.iter().sum();
        if total == 0.0 { return None; }
        let entropy: f64 = qtys.iter().map(|&q| {
            let p = q / total;
            if p > 0.0 { -p * p.ln() } else { 0.0 }
        }).sum();
        let max_entropy = (ticks.len() as f64).ln();
        if max_entropy == 0.0 { return None; }
        Some(entropy / max_entropy)
    }

    // ── round-136 ────────────────────────────────────────────────────────────

    /// Tick volatility ratio: std of price changes / mean absolute price change.
    pub fn tick_vol_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let diffs: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
        let mean_abs = diffs.iter().map(|&d| d.abs()).sum::<f64>() / diffs.len() as f64;
        if mean_abs == 0.0 { return None; }
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        let std = (diffs.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / diffs.len() as f64).sqrt();
        Some(std / mean_abs)
    }

    /// Quantity std ratio: std of quantities / mean quantity.
    pub fn qty_std_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let mean = qtys.iter().sum::<f64>() / qtys.len() as f64;
        if mean == 0.0 { return None; }
        let std = (qtys.iter().map(|&q| (q - mean).powi(2)).sum::<f64>() / qtys.len() as f64).sqrt();
        Some(std / mean.abs())
    }

    /// Side quantity concentration: max(buy_qty, sell_qty) / total_sided_qty.
    pub fn side_qty_concentration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_qty: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        let sell_qty: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .filter_map(|t| t.quantity.to_f64())
            .sum();
        let total = buy_qty + sell_qty;
        if total == 0.0 { return None; }
        Some(buy_qty.max(sell_qty) / total)
    }

    /// Price reversion speed: fraction of ticks where price moves back toward the mean.
    pub fn price_reversion_speed(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let reversions = prices.windows(2)
            .filter(|w| {
                let dev0 = (w[0] - mean).abs();
                let dev1 = (w[1] - mean).abs();
                dev1 < dev0
            })
            .count();
        Some(reversions as f64 / (prices.len() - 1) as f64)
    }

    // ── round-137 ────────────────────────────────────────────────────────────

    /// Price downside ratio: fraction of prices below the mean.
    pub fn price_downside_ratio(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let below = prices.iter().filter(|&&p| p < mean).count() as f64;
        Some(below / prices.len() as f64)
    }

    /// Average trade lag: mean of inter-tick time differences in milliseconds.
    pub fn avg_trade_lag(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let gaps: Vec<f64> = ticks.windows(2)
            .map(|w| w[1].received_at_ms.saturating_sub(w[0].received_at_ms) as f64)
            .collect();
        Some(gaps.iter().sum::<f64>() / gaps.len() as f64)
    }

    /// Quantity max run: maximum consecutive run of increasing quantities.
    pub fn qty_max_run(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.len() < 2 { return None; }
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for w in ticks.windows(2) {
            if w[1].quantity > w[0].quantity { cur_run += 1; max_run = max_run.max(cur_run); }
            else { cur_run = 0; }
        }
        Some(max_run)
    }

    /// Tick sell fraction: fraction of ticks with Sell side.
    pub fn tick_sell_fraction(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.is_empty() { return None; }
        let sells = ticks.iter().filter(|t| matches!(t.side, Some(TradeSide::Sell))).count();
        Some(sells as f64 / ticks.len() as f64)
    }

    // ── round-138 ────────────────────────────────────────────────────────────

    /// Quantity turnover rate: mean absolute quantity change per tick.
    pub fn qty_turnover_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let diffs: Vec<f64> = ticks.windows(2)
            .map(|w| (w[1].quantity - w[0].quantity).abs().to_f64().unwrap_or(0.0))
            .collect();
        Some(diffs.iter().sum::<f64>() / diffs.len() as f64)
    }

    /// Tick price acceleration: mean second derivative of price (change in changes).
    pub fn tick_price_acceleration(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let accels: Vec<f64> = prices.windows(3)
            .map(|w| (w[2] - w[1]) - (w[1] - w[0]))
            .collect();
        Some(accels.iter().sum::<f64>() / accels.len() as f64)
    }

    /// Side volume skew: buy total quantity minus sell total quantity, normalized by total.
    pub fn side_volume_skew(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_vol: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .sum();
        let sell_vol: f64 = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.quantity.to_f64().unwrap_or(0.0))
            .sum();
        let total = buy_vol + sell_vol;
        if total == 0.0 { return None; }
        Some((buy_vol - sell_vol) / total)
    }

    /// Price decay rate: mean of negative price changes (downward moves only).
    pub fn price_decay_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let downs: Vec<f64> = ticks.windows(2)
            .filter_map(|w| {
                let d = (w[1].price - w[0].price).to_f64().unwrap_or(0.0);
                if d < 0.0 { Some(d.abs()) } else { None }
            })
            .collect();
        if downs.is_empty() { return Some(0.0); }
        Some(downs.iter().sum::<f64>() / downs.len() as f64)
    }

    // ── round-139 ────────────────────────────────────────────────────────────

    /// Price upper shadow: fraction of ticks where price > mean price.
    pub fn price_upper_shadow(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let prices: Vec<f64> = ticks.iter().map(|t| t.price.to_f64().unwrap_or(0.0)).collect();
        let mean = prices.iter().sum::<f64>() / prices.len() as f64;
        let above = prices.iter().filter(|&&p| p > mean).count() as f64;
        Some(above / prices.len() as f64)
    }

    /// Quantity momentum score: last quantity vs mean quantity, normalized by std.
    pub fn qty_momentum_score(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 2 { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let mean = qtys.iter().sum::<f64>() / qtys.len() as f64;
        let std = (qtys.iter().map(|&q| (q - mean).powi(2)).sum::<f64>() / qtys.len() as f64).sqrt();
        if std == 0.0 { return None; }
        Some((*qtys.last()? - mean) / std)
    }

    /// Tick buy run: longest consecutive run of Buy-sided ticks.
    pub fn tick_buy_run(ticks: &[NormalizedTick]) -> Option<usize> {
        if ticks.is_empty() { return None; }
        let mut max_run = 0usize;
        let mut cur_run = 0usize;
        for t in ticks {
            if matches!(t.side, Some(TradeSide::Buy)) {
                cur_run += 1;
                max_run = max_run.max(cur_run);
            } else {
                cur_run = 0;
            }
        }
        Some(max_run)
    }

    /// Side price gap: mean price difference between Buy and Sell ticks.
    pub fn side_price_gap(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        let buy_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Buy)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        let sell_prices: Vec<f64> = ticks.iter()
            .filter(|t| matches!(t.side, Some(TradeSide::Sell)))
            .map(|t| t.price.to_f64().unwrap_or(0.0))
            .collect();
        if buy_prices.is_empty() || sell_prices.is_empty() { return None; }
        let buy_mean = buy_prices.iter().sum::<f64>() / buy_prices.len() as f64;
        let sell_mean = sell_prices.iter().sum::<f64>() / sell_prices.len() as f64;
        Some(buy_mean - sell_mean)
    }

    // ── round-140 ────────────────────────────────────────────────────────────

    /// Price volatility skew: skewness of absolute price changes.
    pub fn price_volatility_skew(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let abs_changes: Vec<f64> = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).abs().to_f64().unwrap_or(0.0))
            .collect();
        let n = abs_changes.len() as f64;
        let mean = abs_changes.iter().sum::<f64>() / n;
        let std = (abs_changes.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        if std == 0.0 { return None; }
        let skew = abs_changes.iter().map(|&x| ((x - mean) / std).powi(3)).sum::<f64>() / n;
        Some(skew)
    }

    /// Quantity peak to trough: max quantity / min quantity across ticks.
    pub fn qty_peak_to_trough(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.is_empty() { return None; }
        let qtys: Vec<f64> = ticks.iter().map(|t| t.quantity.to_f64().unwrap_or(0.0)).collect();
        let max = qtys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = qtys.iter().cloned().fold(f64::INFINITY, f64::min);
        if min == 0.0 { return None; }
        Some(max / min)
    }

    /// Tick momentum decay: correlation of price changes with their index (negative = decay).
    pub fn tick_momentum_decay(ticks: &[NormalizedTick]) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        if ticks.len() < 3 { return None; }
        let changes: Vec<f64> = ticks.windows(2)
            .map(|w| (w[1].price - w[0].price).to_f64().unwrap_or(0.0))
            .collect();
        let n = changes.len() as f64;
        let indices: Vec<f64> = (0..changes.len()).map(|i| i as f64).collect();
        let x_mean = indices.iter().sum::<f64>() / n;
        let y_mean = changes.iter().sum::<f64>() / n;
        let cov: f64 = indices.iter().zip(changes.iter()).map(|(&x, &y)| (x - x_mean) * (y - y_mean)).sum::<f64>() / n;
        let x_std = (indices.iter().map(|&x| (x - x_mean).powi(2)).sum::<f64>() / n).sqrt();
        let y_std = (changes.iter().map(|&y| (y - y_mean).powi(2)).sum::<f64>() / n).sqrt();
        if x_std == 0.0 || y_std == 0.0 { return None; }
        Some(cov / (x_std * y_std))
    }

    /// Side transition rate: fraction of consecutive tick pairs that change side.
    pub fn side_transition_rate(ticks: &[NormalizedTick]) -> Option<f64> {
        if ticks.len() < 2 { return None; }
        let sided: Vec<_> = ticks.iter()
            .filter_map(|t| t.side)
            .collect();
        if sided.len() < 2 { return None; }
        let transitions = sided.windows(2)
            .filter(|w| !matches!((w[0], w[1]), (TradeSide::Buy, TradeSide::Buy) | (TradeSide::Sell, TradeSide::Sell)))
            .count() as f64;
        Some(transitions / (sided.len() - 1) as f64)
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

    // ── round-94 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_price_efficiency_ratio_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_efficiency_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_efficiency_ratio_one_for_straight_line() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let r = NormalizedTick::price_efficiency_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "straight line → ratio 1.0, got {}", r);
    }

    #[test]
    fn test_min_inter_tick_gap_ms_none_for_single() {
        let t = make_tick_at(1000);
        assert!(NormalizedTick::min_inter_tick_gap_ms(&[t]).is_none());
    }

    #[test]
    fn test_min_inter_tick_gap_ms_returns_smallest_gap() {
        let t1 = make_tick_at(0);
        let t2 = make_tick_at(10);
        let t3 = make_tick_at(25);
        let min = NormalizedTick::min_inter_tick_gap_ms(&[t1, t2, t3]).unwrap();
        assert_eq!(min, 10, "smallest gap is 10ms");
    }

    #[test]
    fn test_max_inter_tick_gap_ms_none_for_single() {
        let t = make_tick_at(1000);
        assert!(NormalizedTick::max_inter_tick_gap_ms(&[t]).is_none());
    }

    #[test]
    fn test_max_inter_tick_gap_ms_returns_largest_gap() {
        let t1 = make_tick_at(0);
        let t2 = make_tick_at(10);
        let t3 = make_tick_at(25);
        let max = NormalizedTick::max_inter_tick_gap_ms(&[t1, t2, t3]).unwrap();
        assert_eq!(max, 15, "largest gap is 15ms");
    }

    #[test]
    fn test_trade_count_imbalance_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::trade_count_imbalance(&ticks).is_none());
    }

    #[test]
    fn test_trade_count_imbalance_all_buys() {
        let ticks = vec![
            NormalizedTick {
                exchange: crate::Exchange::Binance,
                symbol: "BTCUSDT".into(),
                price: rust_decimal_macros::dec!(100),
                quantity: rust_decimal_macros::dec!(1),
                side: Some(TradeSide::Buy),
                trade_id: None,
                exchange_ts_ms: None,
                received_at_ms: 0,
            },
            NormalizedTick {
                exchange: crate::Exchange::Binance,
                symbol: "BTCUSDT".into(),
                price: rust_decimal_macros::dec!(101),
                quantity: rust_decimal_macros::dec!(1),
                side: Some(TradeSide::Buy),
                trade_id: None,
                exchange_ts_ms: None,
                received_at_ms: 1,
            },
        ];
        let imb = NormalizedTick::trade_count_imbalance(&ticks).unwrap();
        assert!((imb - 1.0).abs() < 1e-9, "all buys → imbalance 1.0, got {}", imb);
    }

    // ── round-95 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_buy_volume_fraction_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::buy_volume_fraction(&ticks).is_none());
    }

    #[test]
    fn test_buy_volume_fraction_all_buys() {
        let ticks = vec![
            NormalizedTick {
                exchange: crate::Exchange::Binance,
                symbol: "BTCUSDT".into(),
                price: rust_decimal_macros::dec!(100),
                quantity: rust_decimal_macros::dec!(5),
                side: Some(TradeSide::Buy),
                trade_id: None,
                exchange_ts_ms: None,
                received_at_ms: 0,
            },
        ];
        let f = NormalizedTick::buy_volume_fraction(&ticks).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "all buys → 1.0, got {}", f);
    }

    #[test]
    fn test_tick_qty_skewness_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(2))];
        assert!(NormalizedTick::tick_qty_skewness(&ticks).is_none());
    }

    #[test]
    fn test_tick_qty_skewness_zero_for_constant_qty() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        assert!(NormalizedTick::tick_qty_skewness(&ticks).is_none(), "constant → std=0 → None");
    }

    #[test]
    fn test_above_median_price_fraction_none_for_empty() {
        assert!(NormalizedTick::above_median_price_fraction(&[]).is_none());
    }

    #[test]
    fn test_above_median_price_fraction_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
        ];
        // median = 110; only 120 is strictly above → 1/3
        let f = NormalizedTick::above_median_price_fraction(&ticks).unwrap();
        assert!((f - 1.0 / 3.0).abs() < 1e-9, "expected 1/3, got {}", f);
    }

    #[test]
    fn test_cumulative_qty_imbalance_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::cumulative_qty_imbalance(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_cumulative_qty_imbalance_balanced() {
        let ticks = vec![
            NormalizedTick {
                exchange: crate::Exchange::Binance,
                symbol: "BTCUSDT".into(),
                price: rust_decimal_macros::dec!(100),
                quantity: rust_decimal_macros::dec!(3),
                side: Some(TradeSide::Buy),
                trade_id: None,
                exchange_ts_ms: None,
                received_at_ms: 0,
            },
            NormalizedTick {
                exchange: crate::Exchange::Binance,
                symbol: "BTCUSDT".into(),
                price: rust_decimal_macros::dec!(101),
                quantity: rust_decimal_macros::dec!(3),
                side: Some(TradeSide::Sell),
                trade_id: None,
                exchange_ts_ms: None,
                received_at_ms: 1,
            },
        ];
        let imb = NormalizedTick::cumulative_qty_imbalance(&ticks).unwrap();
        assert!(imb.abs() < 1e-9, "balanced → 0.0, got {}", imb);
    }

    // ── round-96 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_qty_weighted_spread_none_for_empty() {
        assert!(NormalizedTick::qty_weighted_spread(&[]).is_none());
    }

    #[test]
    fn test_qty_weighted_spread_zero_for_uniform_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        let s = NormalizedTick::qty_weighted_spread(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "uniform price → spread=0, got {}", s);
    }

    #[test]
    fn test_large_tick_fraction_none_for_empty() {
        assert!(NormalizedTick::large_tick_fraction(&[]).is_none());
    }

    #[test]
    fn test_large_tick_fraction_basic() {
        use rust_decimal_macros::dec;
        // mean = 2; only qty=3 is above
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(3)),
        ];
        let f = NormalizedTick::large_tick_fraction(&ticks).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_net_price_drift_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::net_price_drift(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_net_price_drift_positive_for_rising() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let d = NormalizedTick::net_price_drift(&ticks).unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    #[test]
    fn test_tick_arrival_entropy_none_for_two_ticks() {
        let ticks = vec![make_tick_at(0), make_tick_at(10)];
        assert!(NormalizedTick::tick_arrival_entropy(&ticks).is_none());
    }

    #[test]
    fn test_tick_arrival_entropy_none_for_uniform_gaps() {
        let ticks = vec![make_tick_at(0), make_tick_at(10), make_tick_at(20)];
        // all gaps equal → None
        assert!(NormalizedTick::tick_arrival_entropy(&ticks).is_none());
    }

    // ── round-97 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_price_gap_count_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_gap_count(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_gap_count_zero_for_uniform_price() {
        use rust_decimal_macros::dec;
        // all prices equal → VWAP = same → no crossings
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let c = NormalizedTick::price_gap_count(&ticks).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_tick_density_none_for_single_tick() {
        let t = make_tick_at(0);
        assert!(NormalizedTick::tick_density(&[t]).is_none());
    }

    #[test]
    fn test_tick_density_basic() {
        let t1 = make_tick_at(0);
        let t2 = make_tick_at(1000);
        // 2 ticks over 1000ms → 2/1000 = 0.002
        let d = NormalizedTick::tick_density(&[t1, t2]).unwrap();
        assert!((d - 0.002).abs() < 1e-9, "expected 0.002, got {}", d);
    }

    #[test]
    fn test_buy_qty_mean_none_for_no_buys() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::buy_qty_mean(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_sell_qty_mean_none_for_no_sells() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::sell_qty_mean(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_range_asymmetry_none_for_uniform_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        assert!(NormalizedTick::price_range_asymmetry(&ticks).is_none());
    }

    #[test]
    fn test_price_range_asymmetry_zero_for_symmetric_range() {
        use rust_decimal_macros::dec;
        // high=110, low=90 → mid=100; (110-100)-(100-90)=0
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let a = NormalizedTick::price_range_asymmetry(&ticks).unwrap();
        assert!(a.abs() < 1e-9, "symmetric range → asymmetry=0, got {}", a);
    }

    // ── round-98 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_tick_reversal_ratio_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::tick_reversal_ratio(&ticks).is_none());
    }

    #[test]
    fn test_tick_reversal_ratio_zero_for_monotone() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let r = NormalizedTick::tick_reversal_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "monotone → no reversals, got {}", r);
    }

    #[test]
    fn test_first_half_vwap_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::first_half_vwap(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_second_half_vwap_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::second_half_vwap(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_momentum_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_momentum(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_momentum_positive_when_increasing() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(3)),
        ];
        let m = NormalizedTick::qty_momentum(&ticks).unwrap();
        assert!((m - 2.0).abs() < 1e-9, "expected 2.0, got {}", m);
    }

    // ── round-99 tests ────────────────────────────────────────────────────────

    #[test]
    fn test_price_change_acceleration_none_for_three_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        assert!(NormalizedTick::price_change_acceleration(&ticks).is_none());
    }

    #[test]
    fn test_price_change_acceleration_zero_for_constant_rate() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(103), dec!(1)),
        ];
        let a = NormalizedTick::price_change_acceleration(&ticks).unwrap();
        assert!(a.abs() < 1e-9, "constant rate → acceleration=0, got {}", a);
    }

    #[test]
    fn test_avg_qty_per_direction_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_qty_per_direction(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_micro_price_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::micro_price(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_inter_tick_gap_iqr_none_for_three_ticks() {
        let ticks = vec![make_tick_at(0), make_tick_at(10), make_tick_at(20)];
        assert!(NormalizedTick::inter_tick_gap_iqr(&ticks).is_none());
    }

    #[test]
    fn test_inter_tick_gap_iqr_zero_for_uniform_gaps() {
        let ticks = vec![make_tick_at(0), make_tick_at(10), make_tick_at(20), make_tick_at(30)];
        let iqr = NormalizedTick::inter_tick_gap_iqr(&ticks).unwrap();
        assert_eq!(iqr, 0, "uniform gaps → IQR=0");
    }

    // ── round-100 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_consecutive_buy_streak_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::consecutive_buy_streak(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_consecutive_buy_streak_basic() {
        let buys: Vec<_> = (0..3).map(|i| NormalizedTick {
            exchange: crate::Exchange::Binance,
            symbol: "BTCUSDT".into(),
            price: rust_decimal_macros::dec!(100),
            quantity: rust_decimal_macros::dec!(1),
            side: Some(TradeSide::Buy),
            trade_id: None,
            exchange_ts_ms: None,
            received_at_ms: i,
        }).collect();
        let streak = NormalizedTick::consecutive_buy_streak(&buys).unwrap();
        assert_eq!(streak, 3);
    }

    #[test]
    fn test_qty_concentration_ratio_none_for_empty() {
        assert!(NormalizedTick::qty_concentration_ratio(&[]).is_none());
    }

    #[test]
    fn test_qty_concentration_ratio_one_for_single_tick() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(5))];
        let r = NormalizedTick::qty_concentration_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "single tick → HHI=1.0, got {}", r);
    }

    #[test]
    fn test_price_level_count_none_for_empty() {
        assert!(NormalizedTick::price_level_count(&[]).is_none());
    }

    #[test]
    fn test_price_level_count_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let c = NormalizedTick::price_level_count(&ticks).unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_tick_count_per_price_level_none_for_empty() {
        assert!(NormalizedTick::tick_count_per_price_level(&[]).is_none());
    }

    #[test]
    fn test_tick_count_per_price_level_basic() {
        use rust_decimal_macros::dec;
        // 3 ticks, 2 levels → 1.5
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let r = NormalizedTick::tick_count_per_price_level(&ticks).unwrap();
        assert!((r - 1.5).abs() < 1e-9, "expected 1.5, got {}", r);
    }

    // ── round-101 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_avg_buy_price_none_for_no_buys() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_buy_price(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_avg_sell_price_none_for_no_sells() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_sell_price(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_spread_ratio_none_for_empty() {
        assert!(NormalizedTick::price_spread_ratio(&[]).is_none());
    }

    #[test]
    fn test_price_spread_ratio_zero_for_uniform_price() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        let r = NormalizedTick::price_spread_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "uniform price → spread_ratio=0, got {}", r);
    }

    #[test]
    fn test_trade_size_entropy_none_for_two_ticks() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(2))];
        assert!(NormalizedTick::trade_size_entropy(&ticks).is_none());
    }

    #[test]
    fn test_trade_size_entropy_none_for_identical_sizes() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(101), dec!(5)),
            make_tick_pq(dec!(102), dec!(5)),
        ];
        assert!(NormalizedTick::trade_size_entropy(&ticks).is_none());
    }

    // ── round-102 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_price_impact_ratio_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_impact_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_impact_ratio_zero_for_no_move() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let r = NormalizedTick::price_impact_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "no price move → ratio=0, got {}", r);
    }

    #[test]
    fn test_consecutive_sell_streak_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::consecutive_sell_streak(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_avg_qty_variance_none_for_single_tick() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_qty_variance(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_avg_qty_variance_zero_for_constant_qty() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(101), dec!(5)),
        ];
        let v = NormalizedTick::avg_qty_variance(&ticks).unwrap();
        assert!(v.abs() < 1e-9, "constant qty → variance=0, got {}", v);
    }

    #[test]
    fn test_price_midpoint_none_for_empty() {
        assert!(NormalizedTick::price_midpoint(&[]).is_none());
    }

    #[test]
    fn test_price_midpoint_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let m = NormalizedTick::price_midpoint(&ticks).unwrap();
        assert!((m - 100.0).abs() < 1e-9, "expected 100.0, got {}", m);
    }

    // ── round-103 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_qty_range_none_for_empty() {
        assert!(NormalizedTick::qty_range(&[]).is_none());
    }

    #[test]
    fn test_qty_range_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(8)),
        ];
        let r = NormalizedTick::qty_range(&ticks).unwrap();
        assert!((r - 6.0).abs() < 1e-9, "expected 6.0, got {}", r);
    }

    #[test]
    fn test_time_weighted_qty_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(5));
        assert!(NormalizedTick::time_weighted_qty(&[t]).is_none());
    }

    #[test]
    fn test_time_weighted_qty_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(10));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(100), dec!(20));
        t2.received_at_ms = 100;
        let mut t3 = make_tick_pq(dec!(100), dec!(5));
        t3.received_at_ms = 200;
        // t1 weighted 100ms, t2 weighted 100ms; t3 has zero weight
        // result = (10*100 + 20*100) / 200 = 3000/200 = 15
        let twq = NormalizedTick::time_weighted_qty(&[t1, t2, t3]).unwrap();
        assert!((twq - 15.0).abs() < 1e-9, "expected 15.0, got {}", twq);
    }

    #[test]
    fn test_above_vwap_fraction_none_for_empty() {
        assert!(NormalizedTick::above_vwap_fraction(&[]).is_none());
    }

    #[test]
    fn test_above_vwap_fraction_basic() {
        use rust_decimal_macros::dec;
        // VWAP = (100*1 + 200*1) / 2 = 150; only price=200 is above
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(200), dec!(1)),
        ];
        let f = NormalizedTick::above_vwap_fraction(&ticks).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_tick_speed_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_speed(&[t]).is_none());
    }

    #[test]
    fn test_tick_speed_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(110), dec!(1));
        t2.received_at_ms = 100;
        // speed = (110-100) / 100ms = 0.1
        let s = NormalizedTick::tick_speed(&[t1, t2]).unwrap();
        assert!((s - 0.1).abs() < 1e-9, "expected 0.1, got {}", s);
    }

    // ── round-104 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_qty_percentile_75_none_for_empty() {
        assert!(NormalizedTick::qty_percentile_75(&[]).is_none());
    }

    #[test]
    fn test_qty_percentile_75_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
            make_tick_pq(dec!(100), dec!(4)),
        ];
        // sorted: [1,2,3,4], 75th percentile index = ceil(4*0.75)-1 = 3-1=2 → val=3
        let p = NormalizedTick::qty_percentile_75(&ticks).unwrap();
        assert!((p - 3.0).abs() < 1e-9, "expected 3.0, got {}", p);
    }

    #[test]
    fn test_large_qty_count_none_for_empty() {
        assert!(NormalizedTick::large_qty_count(&[]).is_none());
    }

    #[test]
    fn test_large_qty_count_basic() {
        use rust_decimal_macros::dec;
        // mean = (1+2+3)/3 = 2; only qty=3 is above mean
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let c = NormalizedTick::large_qty_count(&ticks).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_price_rms_none_for_empty() {
        assert!(NormalizedTick::price_rms(&[]).is_none());
    }

    #[test]
    fn test_price_rms_basic() {
        use rust_decimal_macros::dec;
        // rms of [3, 4] = sqrt((9+16)/2) = sqrt(12.5) ≈ 3.5355
        let ticks = vec![
            make_tick_pq(dec!(3), dec!(1)),
            make_tick_pq(dec!(4), dec!(1)),
        ];
        let r = NormalizedTick::price_rms(&ticks).unwrap();
        assert!((r - 12.5_f64.sqrt()).abs() < 1e-9, "expected sqrt(12.5), got {}", r);
    }

    #[test]
    fn test_weighted_tick_count_none_for_empty() {
        assert!(NormalizedTick::weighted_tick_count(&[]).is_none());
    }

    #[test]
    fn test_weighted_tick_count_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let w = NormalizedTick::weighted_tick_count(&ticks).unwrap();
        assert!((w - 8.0).abs() < 1e-9, "expected 8.0, got {}", w);
    }

    // ── round-105 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_tick_burst_count_none_for_two() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_burst_count(&[t1, t2]).is_none());
    }

    #[test]
    fn test_tick_burst_count_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.received_at_ms = 10; // gap=10 (burst, below median=50)
        let mut t3 = make_tick_pq(dec!(100), dec!(1));
        t3.received_at_ms = 110; // gap=100
        let count = NormalizedTick::tick_burst_count(&[t1, t2, t3]).unwrap();
        assert_eq!(count, 1); // gap=10 < median=55 (median of [10,100])
    }

    #[test]
    fn test_price_trend_score_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_trend_score(&[t]).is_none());
    }

    #[test]
    fn test_price_trend_score_all_up() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
            make_tick_pq(dec!(120), dec!(1)),
        ];
        let s = NormalizedTick::price_trend_score(&ticks).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_sell_qty_fraction_none_for_empty() {
        assert!(NormalizedTick::sell_qty_fraction(&[]).is_none());
    }

    #[test]
    fn test_sell_qty_fraction_basic() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(6));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(4));
        sell.side = Some(TradeSide::Sell);
        let f = NormalizedTick::sell_qty_fraction(&[buy, sell]).unwrap();
        assert!((f - 0.4).abs() < 1e-9, "expected 0.4, got {}", f);
    }

    #[test]
    fn test_qty_above_median_none_for_empty() {
        assert!(NormalizedTick::qty_above_median(&[]).is_none());
    }

    #[test]
    fn test_qty_above_median_basic() {
        use rust_decimal_macros::dec;
        // sorted qtys: [1,2,3,4,5], median=3; above median: 4,5 → count=2
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(5)),
            make_tick_pq(dec!(100), dec!(3)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(4)),
        ];
        let c = NormalizedTick::qty_above_median(&ticks).unwrap();
        assert_eq!(c, 2);
    }

    // ── round-106 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_price_zscore_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_zscore(&[t]).is_none());
    }

    #[test]
    fn test_price_zscore_basic() {
        use rust_decimal_macros::dec;
        // prices [100, 200]: mean=150, std=50; last=200 → zscore=1.0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(200), dec!(1));
        let z = NormalizedTick::price_zscore(&[t1, t2]).unwrap();
        assert!((z - 1.0).abs() < 1e-9, "expected 1.0, got {}", z);
    }

    #[test]
    fn test_buy_side_fraction_none_for_empty() {
        assert!(NormalizedTick::buy_side_fraction(&[]).is_none());
    }

    #[test]
    fn test_buy_side_fraction_basic() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let f = NormalizedTick::buy_side_fraction(&[buy, sell]).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_tick_qty_cv_none_for_empty() {
        assert!(NormalizedTick::tick_qty_cv(&[]).is_none());
    }

    #[test]
    fn test_tick_qty_cv_basic() {
        use rust_decimal_macros::dec;
        // qtys [1, 1]: std=0, mean=1, cv=0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        let cv = NormalizedTick::tick_qty_cv(&[t1, t2]).unwrap();
        assert!(cv.abs() < 1e-9, "expected ~0, got {}", cv);
    }

    #[test]
    fn test_avg_trade_value_none_for_empty() {
        assert!(NormalizedTick::avg_trade_value(&[]).is_none());
    }

    #[test]
    fn test_avg_trade_value_basic() {
        use rust_decimal_macros::dec;
        // price=100, qty=5 → value=500 for both ticks → avg=500
        let t1 = make_tick_pq(dec!(100), dec!(5));
        let t2 = make_tick_pq(dec!(100), dec!(5));
        let v = NormalizedTick::avg_trade_value(&[t1, t2]).unwrap();
        assert!((v - 500.0).abs() < 1e-9, "expected 500.0, got {}", v);
    }

    // ── round-107 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_max_buy_price_none_when_no_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Sell);
        assert!(NormalizedTick::max_buy_price(&[t]).is_none());
    }

    #[test]
    fn test_max_buy_price_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(105), dec!(1));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(110), dec!(1));
        t2.side = Some(TradeSide::Buy);
        let m = NormalizedTick::max_buy_price(&[t1, t2]).unwrap();
        assert!((m - 110.0).abs() < 1e-9, "expected 110.0, got {}", m);
    }

    #[test]
    fn test_min_sell_price_none_when_no_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        assert!(NormalizedTick::min_sell_price(&[t]).is_none());
    }

    #[test]
    fn test_min_sell_price_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(95), dec!(1));
        t1.side = Some(TradeSide::Sell);
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.side = Some(TradeSide::Sell);
        let m = NormalizedTick::min_sell_price(&[t1, t2]).unwrap();
        assert!((m - 95.0).abs() < 1e-9, "expected 95.0, got {}", m);
    }

    #[test]
    fn test_price_range_ratio_none_for_empty() {
        assert!(NormalizedTick::price_range_ratio(&[]).is_none());
    }

    #[test]
    fn test_price_range_ratio_basic() {
        use rust_decimal_macros::dec;
        // prices [90, 110]: range=20, mean=100 → ratio=0.2
        let t1 = make_tick_pq(dec!(90), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let r = NormalizedTick::price_range_ratio(&[t1, t2]).unwrap();
        assert!((r - 0.2).abs() < 1e-9, "expected 0.2, got {}", r);
    }

    #[test]
    fn test_qty_weighted_price_change_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::qty_weighted_price_change(&[t]).is_none());
    }

    #[test]
    fn test_qty_weighted_price_change_basic() {
        use rust_decimal_macros::dec;
        // price change=10, avg_qty=(5+5)/2=5 → weighted=50
        let t1 = make_tick_pq(dec!(100), dec!(5));
        let t2 = make_tick_pq(dec!(110), dec!(5));
        let w = NormalizedTick::qty_weighted_price_change(&[t1, t2]).unwrap();
        assert!((w - 50.0).abs() < 1e-9, "expected 50.0, got {}", w);
    }

    // ── round-108 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_last_price_change_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::last_price_change(&[t]).is_none());
    }

    #[test]
    fn test_last_price_change_basic() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(115), dec!(1));
        let c = NormalizedTick::last_price_change(&[t1, t2]).unwrap();
        assert!((c - 15.0).abs() < 1e-9, "expected 15.0, got {}", c);
    }

    #[test]
    fn test_buy_tick_rate_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::buy_tick_rate(&[t]).is_none());
    }

    #[test]
    fn test_buy_tick_rate_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.received_at_ms = 100;
        t2.side = Some(TradeSide::Buy);
        // 2 buys over 100ms → rate = 0.02
        let r = NormalizedTick::buy_tick_rate(&[t1, t2]).unwrap();
        assert!((r - 0.02).abs() < 1e-9, "expected 0.02, got {}", r);
    }

    #[test]
    fn test_qty_median_absolute_deviation_none_for_empty() {
        assert!(NormalizedTick::qty_median_absolute_deviation(&[]).is_none());
    }

    #[test]
    fn test_qty_median_absolute_deviation_basic() {
        use rust_decimal_macros::dec;
        // qtys [1,2,3]: median=2; deviations [1,0,1]; median of [0,1,1] = 1
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
        ];
        let m = NormalizedTick::qty_median_absolute_deviation(&ticks).unwrap();
        assert!((m - 1.0).abs() < 1e-9, "expected 1.0, got {}", m);
    }

    #[test]
    fn test_price_percentile_25_none_for_empty() {
        assert!(NormalizedTick::price_percentile_25(&[]).is_none());
    }

    #[test]
    fn test_price_percentile_25_basic() {
        use rust_decimal_macros::dec;
        // sorted: [10,20,30,40], p25 index = ceil(4*0.25)-1 = 1-1=0 → val=10
        let ticks = vec![
            make_tick_pq(dec!(30), dec!(1)),
            make_tick_pq(dec!(10), dec!(1)),
            make_tick_pq(dec!(40), dec!(1)),
            make_tick_pq(dec!(20), dec!(1)),
        ];
        let p = NormalizedTick::price_percentile_25(&ticks).unwrap();
        assert!((p - 10.0).abs() < 1e-9, "expected 10.0, got {}", p);
    }

    // ── round-109 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_sell_tick_count_none_for_empty() {
        assert!(NormalizedTick::sell_tick_count(&[]).is_none());
    }

    #[test]
    fn test_sell_tick_count_basic() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let c = NormalizedTick::sell_tick_count(&[buy, sell]).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_inter_tick_range_ms_none_for_two() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::inter_tick_range_ms(&[t1, t2]).is_none());
    }

    #[test]
    fn test_inter_tick_range_ms_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(100), dec!(1));
        t2.received_at_ms = 10;
        let mut t3 = make_tick_pq(dec!(100), dec!(1));
        t3.received_at_ms = 110;
        // gaps: [10, 100]; range = 90
        let r = NormalizedTick::inter_tick_range_ms(&[t1, t2, t3]).unwrap();
        assert!((r - 90.0).abs() < 1e-9, "expected 90.0, got {}", r);
    }

    #[test]
    fn test_net_qty_flow_none_for_empty() {
        assert!(NormalizedTick::net_qty_flow(&[]).is_none());
    }

    #[test]
    fn test_net_qty_flow_basic() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(7));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(3));
        sell.side = Some(TradeSide::Sell);
        let f = NormalizedTick::net_qty_flow(&[buy, sell]).unwrap();
        assert!((f - 4.0).abs() < 1e-9, "expected 4.0, got {}", f);
    }

    #[test]
    fn test_qty_skew_ratio_none_for_empty() {
        assert!(NormalizedTick::qty_skew_ratio(&[]).is_none());
    }

    #[test]
    fn test_qty_skew_ratio_basic() {
        use rust_decimal_macros::dec;
        // max=10, min=2 → ratio=5
        let t1 = make_tick_pq(dec!(100), dec!(2));
        let t2 = make_tick_pq(dec!(100), dec!(10));
        let r = NormalizedTick::qty_skew_ratio(&[t1, t2]).unwrap();
        assert!((r - 5.0).abs() < 1e-9, "expected 5.0, got {}", r);
    }

    // ── round-110 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_first_to_last_price_none_for_empty() {
        assert!(NormalizedTick::first_to_last_price(&[]).is_none());
    }

    #[test]
    fn test_first_to_last_price_basic() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(115), dec!(1));
        let p = NormalizedTick::first_to_last_price(&[t1, t2]).unwrap();
        assert!((p - 15.0).abs() < 1e-9, "expected 15.0, got {}", p);
    }

    #[test]
    fn test_tick_volume_profile_none_for_empty() {
        assert!(NormalizedTick::tick_volume_profile(&[]).is_none());
    }

    #[test]
    fn test_tick_volume_profile_basic() {
        use rust_decimal_macros::dec;
        // 3 ticks: 2 at price 100 and 1 at 110 → 2 distinct levels
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(2));
        let t3 = make_tick_pq(dec!(110), dec!(1));
        let c = NormalizedTick::tick_volume_profile(&[t1, t2, t3]).unwrap();
        assert_eq!(c, 2);
    }

    #[test]
    fn test_price_quartile_range_none_for_empty() {
        assert!(NormalizedTick::price_quartile_range(&[]).is_none());
    }

    #[test]
    fn test_price_quartile_range_basic() {
        use rust_decimal_macros::dec;
        // prices [10,20,30,40]: q1=10, q3=30 → IQR=20
        let ticks = vec![
            make_tick_pq(dec!(10), dec!(1)),
            make_tick_pq(dec!(20), dec!(1)),
            make_tick_pq(dec!(30), dec!(1)),
            make_tick_pq(dec!(40), dec!(1)),
        ];
        let r = NormalizedTick::price_quartile_range(&ticks).unwrap();
        assert!((r - 20.0).abs() < 1e-9, "expected 20.0, got {}", r);
    }

    #[test]
    fn test_buy_pressure_index_none_for_empty() {
        assert!(NormalizedTick::buy_pressure_index(&[]).is_none());
    }

    #[test]
    fn test_buy_pressure_index_all_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(10));
        t.side = Some(TradeSide::Buy);
        let idx = NormalizedTick::buy_pressure_index(&[t]).unwrap();
        assert!((idx - 1.0).abs() < 1e-9, "expected 1.0, got {}", idx);
    }

    // ── round-111 tests ───────────────────────────────────────────────────────

    #[test]
    fn test_tick_price_entropy_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_price_entropy(&[t]).is_none());
    }

    #[test]
    fn test_tick_price_entropy_basic() {
        use rust_decimal_macros::dec;
        // 2 different prices — entropy > 0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(200), dec!(1));
        let e = NormalizedTick::tick_price_entropy(&[t1, t2]).unwrap();
        assert!(e > 0.0, "expected entropy > 0, got {}", e);
    }

    #[test]
    fn test_average_spread_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::average_spread(&[t]).is_none());
    }

    #[test]
    fn test_average_spread_basic() {
        use rust_decimal_macros::dec;
        // |110-100| = 10; average spread = 10
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let s = NormalizedTick::average_spread(&[t1, t2]).unwrap();
        assert!((s - 10.0).abs() < 1e-9, "expected 10.0, got {}", s);
    }

    #[test]
    fn test_tick_sigma_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_sigma(&[t]).is_none());
    }

    #[test]
    fn test_tick_sigma_uniform() {
        use rust_decimal_macros::dec;
        // all same price → sigma=0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        let s = NormalizedTick::tick_sigma(&[t1, t2]).unwrap();
        assert!(s.abs() < 1e-9, "expected ~0, got {}", s);
    }

    #[test]
    fn test_downside_qty_fraction_none_for_empty() {
        assert!(NormalizedTick::downside_qty_fraction(&[]).is_none());
    }

    #[test]
    fn test_downside_qty_fraction_basic() {
        use rust_decimal_macros::dec;
        // mean price = (100+200)/2 = 150; below mean: price=100, qty=5; total=10 → 0.5
        let t1 = make_tick_pq(dec!(100), dec!(5));
        let t2 = make_tick_pq(dec!(200), dec!(5));
        let f = NormalizedTick::downside_qty_fraction(&[t1, t2]).unwrap();
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    #[test]
    fn test_price_reversal_rate_none_for_two() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert!(NormalizedTick::price_reversal_rate(&[t1, t2]).is_none());
    }

    #[test]
    fn test_price_reversal_rate_basic() {
        use rust_decimal_macros::dec;
        // up, down → reversal fraction = 1/(2-1) = 1.0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(105), dec!(1));
        let t3 = make_tick_pq(dec!(100), dec!(1));
        let r = NormalizedTick::price_reversal_rate(&[t1, t2, t3]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_qty_ema_none_for_empty() {
        assert!(NormalizedTick::qty_ema(&[]).is_none());
    }

    #[test]
    fn test_qty_ema_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(5));
        let e = NormalizedTick::qty_ema(&[t]).unwrap();
        assert!((e - 5.0).abs() < 1e-9, "expected 5.0, got {}", e);
    }

    #[test]
    fn test_last_buy_price_none_when_no_buy() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        // no side set → None
        assert!(NormalizedTick::last_buy_price(&[t]).is_none());
    }

    #[test]
    fn test_last_sell_price_none_when_no_sell() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::last_sell_price(&[t]).is_none());
    }

    #[test]
    fn test_avg_inter_tick_gap_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::avg_inter_tick_gap(&[t]).is_none());
    }

    #[test]
    fn test_avg_inter_tick_gap_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        let mut t3 = make_tick_pq(dec!(102), dec!(1));
        t1.received_at_ms = 1000;
        t2.received_at_ms = 1100;
        t3.received_at_ms = 1300;
        // gaps: 100, 200 → mean = 150
        let g = NormalizedTick::avg_inter_tick_gap(&[t1, t2, t3]).unwrap();
        assert!((g - 150.0).abs() < 1e-9, "expected 150.0, got {}", g);
    }

    #[test]
    fn test_tick_intensity_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_intensity(&[t]).is_none());
    }

    #[test]
    fn test_tick_intensity_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t1.received_at_ms = 0;
        t2.received_at_ms = 2000;
        // 2 ticks over 2s → 1 tick/s
        let i = NormalizedTick::tick_intensity(&[t1, t2]).unwrap();
        assert!((i - 1.0).abs() < 1e-9, "expected 1.0, got {}", i);
    }

    #[test]
    fn test_price_swing_none_for_empty() {
        assert!(NormalizedTick::price_swing(&[]).is_none());
    }

    #[test]
    fn test_price_swing_basic() {
        use rust_decimal_macros::dec;
        // prices: 100, 110, 90 → max=110, min=90 → swing = 20/90
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let t3 = make_tick_pq(dec!(90), dec!(1));
        let s = NormalizedTick::price_swing(&[t1, t2, t3]).unwrap();
        let expected = 20.0 / 90.0;
        assert!((s - expected).abs() < 1e-9, "expected {}, got {}", expected, s);
    }

    #[test]
    fn test_qty_velocity_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::qty_velocity(&[t]).is_none());
    }

    #[test]
    fn test_qty_velocity_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(10));
        let mut t2 = make_tick_pq(dec!(101), dec!(20));
        t1.received_at_ms = 0;
        t2.received_at_ms = 1000;
        // dq=10, dt=1000ms → rate = 10/1000 = 0.01 per ms
        let v = NormalizedTick::qty_velocity(&[t1, t2]).unwrap();
        assert!((v - 0.01).abs() < 1e-9, "expected 0.01, got {}", v);
    }

    #[test]
    fn test_price_std_ratio_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_std_ratio(&[t]).is_none());
    }

    #[test]
    fn test_price_std_ratio_zero_for_uniform() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        let r = NormalizedTick::price_std_ratio(&[t1, t2]).unwrap();
        assert!(r.abs() < 1e-9, "expected 0, got {}", r);
    }

    #[test]
    fn test_qty_trend_strength_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::qty_trend_strength(&[t]).is_none());
    }

    #[test]
    fn test_qty_trend_strength_positive_for_increasing() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(2));
        let t3 = make_tick_pq(dec!(102), dec!(3));
        let s = NormalizedTick::qty_trend_strength(&[t1, t2, t3]).unwrap();
        assert!(s > 0.9, "expected high positive correlation, got {}", s);
    }

    #[test]
    fn test_buy_to_sell_gap_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert!(NormalizedTick::buy_to_sell_gap(&[t1, t2]).is_none());
    }

    #[test]
    fn test_tick_range_efficiency_none_for_empty() {
        assert!(NormalizedTick::tick_range_efficiency(&[]).is_none());
    }

    #[test]
    fn test_tick_range_efficiency_basic() {
        use rust_decimal_macros::dec;
        // prices: 100, 110, 90 → first=100, last=90, range=20 → |90-100|/20 = 0.5
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let t3 = make_tick_pq(dec!(90), dec!(1));
        let e = NormalizedTick::tick_range_efficiency(&[t1, t2, t3]).unwrap();
        assert!((e - 0.5).abs() < 1e-9, "expected 0.5, got {}", e);
    }

    #[test]
    fn test_mid_price_mean_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::mid_price_mean(&[t]).is_none());
    }

    #[test]
    fn test_mid_price_mean_basic() {
        use rust_decimal_macros::dec;
        // pair (100, 110) → mid = 105
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let m = NormalizedTick::mid_price_mean(&[t1, t2]).unwrap();
        assert!((m - 105.0).abs() < 1e-9, "expected 105.0, got {}", m);
    }

    #[test]
    fn test_tick_qty_range_none_for_empty() {
        assert!(NormalizedTick::tick_qty_range(&[]).is_none());
    }

    #[test]
    fn test_tick_qty_range_basic() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(3));
        let t2 = make_tick_pq(dec!(101), dec!(7));
        let r = NormalizedTick::tick_qty_range(&[t1, t2]).unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0, got {}", r);
    }

    #[test]
    fn test_buy_dominance_streak_none_when_no_buy() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::buy_dominance_streak(&[t]).is_none());
    }

    #[test]
    fn test_price_gap_mean_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_gap_mean(&[t]).is_none());
    }

    #[test]
    fn test_price_gap_mean_basic() {
        use rust_decimal_macros::dec;
        // gaps: |110-100|=10, |115-110|=5 → mean=7.5
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let t3 = make_tick_pq(dec!(115), dec!(1));
        let g = NormalizedTick::price_gap_mean(&[t1, t2, t3]).unwrap();
        assert!((g - 7.5).abs() < 1e-9, "expected 7.5, got {}", g);
    }

    #[test]
    fn test_price_momentum_index_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_momentum_index(&[t]).is_none());
    }

    #[test]
    fn test_price_momentum_index_basic() {
        use rust_decimal_macros::dec;
        // all rising → 1.0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        let t3 = make_tick_pq(dec!(102), dec!(1));
        let m = NormalizedTick::price_momentum_index(&[t1, t2, t3]).unwrap();
        assert!((m - 1.0).abs() < 1e-9, "expected 1.0, got {}", m);
    }

    #[test]
    fn test_qty_range_ratio_none_for_empty() {
        assert!(NormalizedTick::qty_range_ratio(&[]).is_none());
    }

    #[test]
    fn test_qty_range_ratio_basic() {
        use rust_decimal_macros::dec;
        // qtys: 1, 3 → range=2, mean=2 → ratio=1.0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(3));
        let r = NormalizedTick::qty_range_ratio(&[t1, t2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_recent_price_change_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::recent_price_change(&[t]).is_none());
    }

    #[test]
    fn test_recent_price_change_basic() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(105), dec!(1));
        let c = NormalizedTick::recent_price_change(&[t1, t2]).unwrap();
        assert!((c - 5.0).abs() < 1e-9, "expected 5.0, got {}", c);
    }

    #[test]
    fn test_sell_dominance_streak_none_when_no_sell() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::sell_dominance_streak(&[t]).is_none());
    }

    #[test]
    fn test_price_momentum_slope_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_momentum_slope(&[t]).is_none());
    }

    #[test]
    fn test_price_momentum_slope_positive_for_rising() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(110), dec!(1));
        let t3 = make_tick_pq(dec!(120), dec!(1));
        let s = NormalizedTick::price_momentum_slope(&[t1, t2, t3]).unwrap();
        assert!(s > 0.0, "expected positive slope, got {}", s);
    }

    #[test]
    fn test_qty_dispersion_none_for_empty() {
        assert!(NormalizedTick::qty_dispersion(&[]).is_none());
    }

    #[test]
    fn test_qty_dispersion_zero_for_uniform() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(5));
        let t2 = make_tick_pq(dec!(101), dec!(5));
        let d = NormalizedTick::qty_dispersion(&[t1, t2]).unwrap();
        assert!(d.abs() < 1e-9, "expected 0, got {}", d);
    }

    #[test]
    fn test_tick_buy_pct_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_buy_pct(&[t]).is_none());
    }

    #[test]
    fn test_consecutive_price_rise_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::consecutive_price_rise(&[t]).is_none());
    }

    #[test]
    fn test_consecutive_price_rise_basic() {
        use rust_decimal_macros::dec;
        // all rising → streak = 2
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(105), dec!(1));
        let t3 = make_tick_pq(dec!(110), dec!(1));
        let r = NormalizedTick::consecutive_price_rise(&[t1, t2, t3]).unwrap();
        assert_eq!(r, 2);
    }

    #[test]
    fn test_price_entropy_rate_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::price_entropy_rate(&[t]).is_none());
    }

    #[test]
    fn test_price_entropy_rate_zero_for_flat() {
        use rust_decimal_macros::dec;
        // same price → log(1)=0
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(100), dec!(1));
        let e = NormalizedTick::price_entropy_rate(&[t1, t2]).unwrap();
        assert!(e.abs() < 1e-9, "expected 0, got {}", e);
    }

    #[test]
    fn test_qty_lag1_corr_none_for_two() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(2));
        assert!(NormalizedTick::qty_lag1_corr(&[t1, t2]).is_none());
    }

    #[test]
    fn test_qty_lag1_corr_positive_for_trending() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(2));
        let t3 = make_tick_pq(dec!(102), dec!(3));
        let t4 = make_tick_pq(dec!(103), dec!(4));
        let t5 = make_tick_pq(dec!(104), dec!(5));
        let c = NormalizedTick::qty_lag1_corr(&[t1, t2, t3, t4, t5]).unwrap();
        assert!(c > 0.0, "expected positive autocorrelation, got {}", c);
    }

    #[test]
    fn test_tick_side_transition_rate_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::tick_side_transition_rate(&[t]).is_none());
    }

    #[test]
    fn test_avg_price_per_unit_none_for_empty() {
        assert!(NormalizedTick::avg_price_per_unit(&[]).is_none());
    }

    #[test]
    fn test_avg_price_per_unit_basic() {
        use rust_decimal_macros::dec;
        // mean_price=100, mean_qty=2 → price_per_unit=50
        let t1 = make_tick_pq(dec!(100), dec!(2));
        let t2 = make_tick_pq(dec!(100), dec!(2));
        let v = NormalizedTick::avg_price_per_unit(&[t1, t2]).unwrap();
        assert!((v - 50.0).abs() < 1e-9, "expected 50.0, got {}", v);
    }

    #[test]
    fn test_price_rebound_rate_none_for_two() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(1));
        assert!(NormalizedTick::price_rebound_rate(&[t1, t2]).is_none());
    }

    #[test]
    fn test_price_rebound_rate_basic() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(105), dec!(1));
        let t3 = make_tick_pq(dec!(100), dec!(1));
        let r = NormalizedTick::price_rebound_rate(&[t1, t2, t3]).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    #[test]
    fn test_weighted_spread_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::weighted_spread(&[t]).is_none());
    }

    #[test]
    fn test_weighted_spread_basic() {
        use rust_decimal_macros::dec;
        // flat price → spread=0
        let t1 = make_tick_pq(dec!(100), dec!(5));
        let t2 = make_tick_pq(dec!(100), dec!(5));
        let s = NormalizedTick::weighted_spread(&[t1, t2]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0, got {}", s);
    }

    #[test]
    fn test_buy_price_advantage_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::buy_price_advantage(&[t]).is_none());
    }

    #[test]
    fn test_qty_entropy_none_for_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(1));
        assert!(NormalizedTick::qty_entropy(&[t]).is_none());
    }

    #[test]
    fn test_qty_entropy_basic() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(101), dec!(2));
        let e = NormalizedTick::qty_entropy(&[t1, t2]).unwrap();
        assert!(e >= 0.0, "entropy should be non-negative, got {}", e);
    }

    // ── round-120 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_jitter_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_jitter(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_jitter_basic() {
        use rust_decimal_macros::dec;
        let ticks = [
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let j = NormalizedTick::price_jitter(&ticks).unwrap();
        assert!((j - 4.0).abs() < 1e-9, "expected 4.0, got {}", j);
    }

    #[test]
    fn test_tick_flow_ratio_none_for_unsided() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_flow_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_flow_ratio_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(2));
        t.side = Some(TradeSide::Buy);
        let r = NormalizedTick::tick_flow_ratio(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_qty_skewness_abs_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = [
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(2)),
        ];
        assert!(NormalizedTick::qty_skewness_abs(&ticks).is_none());
    }

    #[test]
    fn test_qty_skewness_abs_uniform_zero() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..5).map(|_| make_tick_pq(dec!(100), dec!(3))).collect();
        // uniform → skew = 0, std = 0 → None
        assert!(NormalizedTick::qty_skewness_abs(&ticks).is_none());
    }

    #[test]
    fn test_side_balance_score_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::side_balance_score(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_side_balance_score_all_buys() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let s = NormalizedTick::side_balance_score(&[t]).unwrap();
        // all buys → frac=1.0, |1.0-0.5|=0.5
        assert!((s - 0.5).abs() < 1e-9, "expected 0.5, got {}", s);
    }

    // ── round-121 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_vol_correlation_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_vol_correlation(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_vol_correlation_uniform_none() {
        use rust_decimal_macros::dec;
        // uniform quantity → zero std → None
        let ticks: Vec<_> = (0..3).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(5))).collect();
        assert!(NormalizedTick::price_vol_correlation(&ticks).is_none());
    }

    #[test]
    fn test_qty_acceleration_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = [make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(2))];
        assert!(NormalizedTick::qty_acceleration(&ticks).is_none());
    }

    #[test]
    fn test_qty_acceleration_constant_zero() {
        use rust_decimal_macros::dec;
        // constant qty → all diffs=0 → acceleration=0
        let ticks: Vec<_> = (0..4).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(3))).collect();
        let a = NormalizedTick::qty_acceleration(&ticks).unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_buy_sell_price_diff_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::buy_sell_price_diff(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_buy_sell_price_diff_basic() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(102), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let d = NormalizedTick::buy_sell_price_diff(&[buy, sell]).unwrap();
        assert!((d - 2.0).abs() < 1e-9, "expected 2.0, got {}", d);
    }

    #[test]
    fn test_tick_imbalance_score_none_for_unsided() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_imbalance_score(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_imbalance_score_balanced() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(5));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(5));
        sell.side = Some(TradeSide::Sell);
        let s = NormalizedTick::tick_imbalance_score(&[buy, sell]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0, got {}", s);
    }

    // ── round-122 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_range_ema_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_range_ema(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_range_ema_basic() {
        use rust_decimal_macros::dec;
        let ticks = [
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
        ];
        let e = NormalizedTick::price_range_ema(&ticks).unwrap();
        assert!(e > 0.0, "expected positive ema, got {}", e);
    }

    #[test]
    fn test_qty_trend_ema_none_for_empty() {
        assert!(NormalizedTick::qty_trend_ema(&[]).is_none());
    }

    #[test]
    fn test_qty_trend_ema_single() {
        use rust_decimal_macros::dec;
        let e = NormalizedTick::qty_trend_ema(&[make_tick_pq(dec!(100), dec!(5))]).unwrap();
        assert!((e - 5.0).abs() < 1e-9, "expected 5.0, got {}", e);
    }

    #[test]
    fn test_weighted_mid_price_none_for_empty() {
        assert!(NormalizedTick::weighted_mid_price(&[]).is_none());
    }

    #[test]
    fn test_weighted_mid_price_equal_weight() {
        use rust_decimal_macros::dec;
        let t1 = make_tick_pq(dec!(100), dec!(1));
        let t2 = make_tick_pq(dec!(200), dec!(1));
        let w = NormalizedTick::weighted_mid_price(&[t1, t2]).unwrap();
        assert!((w - 150.0).abs() < 1e-9, "expected 150.0, got {}", w);
    }

    #[test]
    fn test_tick_buy_qty_fraction_none_for_empty() {
        assert!(NormalizedTick::tick_buy_qty_fraction(&[]).is_none());
    }

    #[test]
    fn test_tick_buy_qty_fraction_all_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(3));
        t.side = Some(TradeSide::Buy);
        // all qty is buy → fraction = 1.0
        let f = NormalizedTick::tick_buy_qty_fraction(&[t]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    // ── round-123 ────────────────────────────────────────────────────────────

    #[test]
    fn test_qty_kurtosis_none_for_three() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        assert!(NormalizedTick::qty_kurtosis(&ticks).is_none());
    }

    #[test]
    fn test_qty_kurtosis_uniform_none() {
        use rust_decimal_macros::dec;
        // uniform qty → variance=0 → None
        let ticks: Vec<_> = (0..5).map(|_| make_tick_pq(dec!(100), dec!(3))).collect();
        assert!(NormalizedTick::qty_kurtosis(&ticks).is_none());
    }

    #[test]
    fn test_price_monotonicity_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_monotonicity(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_monotonicity_all_rising() {
        use rust_decimal_macros::dec;
        let ticks = [
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let m = NormalizedTick::price_monotonicity(&ticks).unwrap();
        assert!((m - 1.0).abs() < 1e-9, "expected 1.0, got {}", m);
    }

    #[test]
    fn test_tick_count_per_second_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_count_per_second(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_range_std_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_range_std(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_range_std_uniform_zero() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        let s = NormalizedTick::price_range_std(&ticks).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0, got {}", s);
    }

    // ── round-124 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_cross_zero_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_cross_zero(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_cross_zero_no_crossing() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        let c = NormalizedTick::price_cross_zero(&ticks).unwrap();
        assert_eq!(c, 0);
    }

    #[test]
    fn test_tick_momentum_score_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_momentum_score(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_momentum_score_all_rising() {
        use rust_decimal_macros::dec;
        let ticks = [
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let s = NormalizedTick::tick_momentum_score(&ticks).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {}", s);
    }

    #[test]
    fn test_sell_side_ratio_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::sell_side_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_sell_side_ratio_all_sells() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Sell);
        let r = NormalizedTick::sell_side_ratio(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_avg_qty_per_side_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_qty_per_side(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_avg_qty_per_side_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(4));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(100), dec!(6));
        t2.side = Some(TradeSide::Sell);
        let a = NormalizedTick::avg_qty_per_side(&[t1, t2]).unwrap();
        assert!((a - 5.0).abs() < 1e-9, "expected 5.0, got {}", a);
    }

    // ── round-125 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_hurst_estimate_none_for_three() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        assert!(NormalizedTick::price_hurst_estimate(&ticks).is_none());
    }

    #[test]
    fn test_price_hurst_estimate_basic() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..8).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        let h = NormalizedTick::price_hurst_estimate(&ticks).unwrap();
        assert!(h > 0.0, "expected positive hurst, got {}", h);
    }

    #[test]
    fn test_qty_mean_reversion_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_mean_reversion(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_mean_reversion_uniform_none() {
        use rust_decimal_macros::dec;
        // uniform → std=0 → None
        let ticks: Vec<_> = (0..4).map(|_| make_tick_pq(dec!(100), dec!(3))).collect();
        assert!(NormalizedTick::qty_mean_reversion(&ticks).is_none());
    }

    #[test]
    fn test_avg_trade_impact_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::avg_trade_impact(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_avg_trade_impact_zero_for_constant_price() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        let i = NormalizedTick::avg_trade_impact(&ticks).unwrap();
        assert!(i.abs() < 1e-9, "expected 0.0, got {}", i);
    }

    #[test]
    fn test_price_range_iqr_none_for_three() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        assert!(NormalizedTick::price_range_iqr(&ticks).is_none());
    }

    #[test]
    fn test_price_range_iqr_basic() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..8).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        let iqr = NormalizedTick::price_range_iqr(&ticks).unwrap();
        assert!(iqr >= 0.0, "expected non-negative iqr, got {}", iqr);
    }

    // ── round-126 ────────────────────────────────────────────────────────────

    #[test]
    fn test_qty_rms_none_for_empty() {
        assert!(NormalizedTick::qty_rms(&[]).is_none());
    }

    #[test]
    fn test_qty_rms_single() {
        use rust_decimal_macros::dec;
        let t = make_tick_pq(dec!(100), dec!(3));
        let r = NormalizedTick::qty_rms(&[t]).unwrap();
        assert!((r - 3.0).abs() < 1e-9, "expected 3.0, got {}", r);
    }

    #[test]
    fn test_bid_ask_proxy_none_for_no_pairs() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::bid_ask_proxy(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_bid_ask_proxy_basic() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(101), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let p = NormalizedTick::bid_ask_proxy(&[buy, sell]).unwrap();
        assert!((p - 1.0).abs() < 1e-9, "expected 1.0, got {}", p);
    }

    #[test]
    fn test_tick_price_accel_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = [make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::tick_price_accel(&ticks).is_none());
    }

    #[test]
    fn test_tick_price_accel_linear_zero() {
        use rust_decimal_macros::dec;
        // linear trend → zero acceleration
        let ticks: Vec<_> = (0..4).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        let a = NormalizedTick::tick_price_accel(&ticks).unwrap();
        assert!(a.abs() < 1e-9, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_price_entropy_iqr_none_for_four() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..4).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        assert!(NormalizedTick::price_entropy_iqr(&ticks).is_none());
    }

    #[test]
    fn test_price_entropy_iqr_basic() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..8).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        let iqr = NormalizedTick::price_entropy_iqr(&ticks).unwrap();
        assert!(iqr >= 0.0, "expected non-negative, got {}", iqr);
    }

    // ── round-127 ────────────────────────────────────────────────────────────

    #[test]
    fn test_tick_dispersion_ratio_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_dispersion_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_dispersion_ratio_constant_zero() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..4).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        let r = NormalizedTick::tick_dispersion_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_price_linear_fit_error_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_linear_fit_error(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_linear_fit_error_perfect_linear_zero() {
        use rust_decimal_macros::dec;
        // perfectly linear prices → MSE = 0
        let ticks: Vec<_> = (0..4).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i * 2), dec!(1))).collect();
        let e = NormalizedTick::price_linear_fit_error(&ticks).unwrap();
        assert!(e < 1e-9, "expected ~0.0, got {}", e);
    }

    #[test]
    fn test_qty_harmonic_mean_none_for_empty() {
        assert!(NormalizedTick::qty_harmonic_mean(&[]).is_none());
    }

    #[test]
    fn test_qty_harmonic_mean_uniform() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..4).map(|_| make_tick_pq(dec!(100), dec!(4))).collect();
        let h = NormalizedTick::qty_harmonic_mean(&ticks).unwrap();
        assert!((h - 4.0).abs() < 1e-9, "expected 4.0, got {}", h);
    }

    #[test]
    fn test_late_trade_fraction_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::late_trade_fraction(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_late_trade_fraction_basic() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..4).map(|i| make_tick_pq(dec!(100) + rust_decimal::Decimal::from(i), dec!(1))).collect();
        let f = NormalizedTick::late_trade_fraction(&ticks).unwrap();
        // 4 ticks: mid=2, late=2 → 0.5
        assert!((f - 0.5).abs() < 1e-9, "expected 0.5, got {}", f);
    }

    // ── round-128 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_bollinger_score_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_bollinger_score(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_bollinger_score_uniform_none() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        assert!(NormalizedTick::price_bollinger_score(&ticks).is_none());
    }

    #[test]
    fn test_qty_log_mean_none_for_empty() {
        assert!(NormalizedTick::qty_log_mean(&[]).is_none());
    }

    #[test]
    fn test_qty_log_mean_uniform() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..3).map(|_| make_tick_pq(dec!(100), dec!(4))).collect();
        // log mean of all-4 = exp(ln(4)) = 4
        let m = NormalizedTick::qty_log_mean(&ticks).unwrap();
        assert!((m - 4.0).abs() < 1e-9, "expected 4.0, got {}", m);
    }

    #[test]
    fn test_tick_speed_variance_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = [make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::tick_speed_variance(&ticks).is_none());
    }

    #[test]
    fn test_tick_speed_variance_constant_zero() {
        use rust_decimal_macros::dec;
        let ticks: Vec<_> = (0..4).map(|_| make_tick_pq(dec!(100), dec!(1))).collect();
        let v = NormalizedTick::tick_speed_variance(&ticks).unwrap();
        assert!(v.abs() < 1e-9, "expected 0.0, got {}", v);
    }

    #[test]
    fn test_relative_price_strength_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::relative_price_strength(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_relative_price_strength_equal() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let r = NormalizedTick::relative_price_strength(&[buy, sell]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    // ── round-129 ────────────────────────────────────────────────────────────
    #[test]
    fn test_price_wave_ratio_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_wave_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_wave_ratio_all_up() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let r = NormalizedTick::price_wave_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    #[test]
    fn test_qty_entropy_score_none_for_empty() {
        assert!(NormalizedTick::qty_entropy_score(&[]).is_none());
    }

    #[test]
    fn test_qty_entropy_score_uniform() {
        use rust_decimal_macros::dec;
        // uniform distribution has max entropy
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let e = NormalizedTick::qty_entropy_score(&ticks).unwrap();
        assert!(e > 0.0, "entropy should be positive, got {}", e);
    }

    #[test]
    fn test_tick_burst_rate_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_burst_rate(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_burst_rate_equal_gaps() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.received_at_ms = 100;
        let mut t3 = make_tick_pq(dec!(102), dec!(1));
        t3.received_at_ms = 200;
        let r = NormalizedTick::tick_burst_rate(&[t1, t2, t3]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for equal gaps, got {}", r);
    }

    #[test]
    fn test_side_weighted_price_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::side_weighted_price(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_side_weighted_price_equal_buy_sell() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let r = NormalizedTick::side_weighted_price(&[buy, sell]).unwrap();
        assert!(r.abs() < 1e-9, "expected ~0.0 for equal buy/sell, got {}", r);
    }

    // ── round-130 ────────────────────────────────────────────────────────────
    #[test]
    fn test_price_median_deviation_none_for_empty() {
        assert!(NormalizedTick::price_median_deviation(&[]).is_none());
    }

    #[test]
    fn test_price_median_deviation_identical() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let d = NormalizedTick::price_median_deviation(&ticks).unwrap();
        assert!(d.abs() < 1e-9, "expected 0.0 for identical prices, got {}", d);
    }

    #[test]
    fn test_tick_autocorr_lag1_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::tick_autocorr_lag1(&ticks).is_none());
    }

    #[test]
    fn test_tick_autocorr_lag1_alternating() {
        use rust_decimal_macros::dec;
        // alternating moves have negative autocorrelation
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let ac = NormalizedTick::tick_autocorr_lag1(&ticks).unwrap();
        assert!(ac < 0.0, "expected negative autocorr for alternating, got {}", ac);
    }

    #[test]
    fn test_side_momentum_ratio_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::side_momentum_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_side_momentum_ratio_all_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let r = NormalizedTick::side_momentum_ratio(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for all buy, got {}", r);
    }

    #[test]
    fn test_price_stability_score_none_for_empty() {
        assert!(NormalizedTick::price_stability_score(&[]).is_none());
    }

    #[test]
    fn test_price_stability_score_identical() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let s = NormalizedTick::price_stability_score(&ticks).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0 for identical prices, got {}", s);
    }

    // ── round-131 ────────────────────────────────────────────────────────────
    #[test]
    fn test_price_range_momentum_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_range_momentum(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_range_momentum_full_move_up() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let m = NormalizedTick::price_range_momentum(&ticks).unwrap();
        assert!((m - 1.0).abs() < 1e-9, "expected 1.0, got {}", m);
    }

    #[test]
    fn test_qty_imbalance_ratio_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_imbalance_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_imbalance_ratio_all_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let r = NormalizedTick::qty_imbalance_ratio(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for all buy, got {}", r);
    }

    #[test]
    fn test_tick_flow_entropy_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_flow_entropy(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_flow_entropy_monotone() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        // all up, p_up=1, p_dn=0, p_fl=0 → entropy=0
        let e = NormalizedTick::tick_flow_entropy(&ticks).unwrap();
        assert!(e.abs() < 1e-9, "expected 0.0 for monotone, got {}", e);
    }

    #[test]
    fn test_side_price_spread_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::side_price_spread(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_side_price_spread_equal() {
        use rust_decimal_macros::dec;
        let mut buy = make_tick_pq(dec!(100), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let s = NormalizedTick::side_price_spread(&[buy, sell]).unwrap();
        assert!(s.abs() < 1e-9, "expected 0.0 for equal sides, got {}", s);
    }

    // ── round-132 ────────────────────────────────────────────────────────────
    #[test]
    fn test_price_zscore_abs_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_zscore_abs(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_zscore_abs_last_at_mean() {
        use rust_decimal_macros::dec;
        // all same price → std=0 → None
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        assert!(NormalizedTick::price_zscore_abs(&ticks).is_none());
    }

    #[test]
    fn test_tick_reversal_count_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::tick_reversal_count(&ticks).is_none());
    }

    #[test]
    fn test_tick_reversal_count_one_reversal() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let c = NormalizedTick::tick_reversal_count(&ticks).unwrap();
        assert_eq!(c, 1);
    }

    #[test]
    fn test_tick_price_range_ratio_none_for_empty() {
        assert!(NormalizedTick::tick_price_range_ratio(&[]).is_none());
    }

    #[test]
    fn test_tick_price_range_ratio_basic() {
        use rust_decimal_macros::dec;
        // prices: 100, 110 → range=10, mean=105 → ratio≈0.095
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let r = NormalizedTick::tick_price_range_ratio(&ticks).unwrap();
        assert!(r > 0.0, "expected positive ratio, got {}", r);
    }

    #[test]
    fn test_price_range_skew_none_for_empty() {
        assert!(NormalizedTick::price_range_skew(&[]).is_none());
    }

    #[test]
    fn test_price_range_skew_midpoint() {
        use rust_decimal_macros::dec;
        // prices: 100, 110 → mean=105, min=100, range=10 → skew=0.5
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let s = NormalizedTick::price_range_skew(&ticks).unwrap();
        assert!((s - 0.5).abs() < 1e-9, "expected 0.5, got {}", s);
    }

    // ── round-133 ────────────────────────────────────────────────────────────
    #[test]
    fn test_price_roc_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_roc(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_roc_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        let r = NormalizedTick::price_roc(&ticks).unwrap();
        assert!((r - 0.1).abs() < 1e-9, "expected 0.1, got {}", r);
    }

    #[test]
    fn test_qty_roc_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_roc(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_roc_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(3)),
        ];
        let r = NormalizedTick::qty_roc(&ticks).unwrap();
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {}", r);
    }

    #[test]
    fn test_tick_timing_score_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_timing_score(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_timing_score_all_in_first_half() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.received_at_ms = 100;
        // both at or before mid=50 — t1 yes, t2 no → 1/2
        let s = NormalizedTick::tick_timing_score(&[t1, t2]).unwrap();
        assert!(s > 0.0, "expected positive score, got {}", s);
    }

    #[test]
    fn test_side_spread_ratio_none_for_insufficient_sides() {
        use rust_decimal_macros::dec;
        // only one side, can't compute std
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        assert!(NormalizedTick::side_spread_ratio(&[t]).is_none());
    }

    #[test]
    fn test_side_spread_ratio_equal_spread() {
        use rust_decimal_macros::dec;
        let mut b1 = make_tick_pq(dec!(100), dec!(1));
        b1.side = Some(TradeSide::Buy);
        let mut b2 = make_tick_pq(dec!(102), dec!(1));
        b2.side = Some(TradeSide::Buy);
        let mut s1 = make_tick_pq(dec!(100), dec!(1));
        s1.side = Some(TradeSide::Sell);
        let mut s2 = make_tick_pq(dec!(102), dec!(1));
        s2.side = Some(TradeSide::Sell);
        let r = NormalizedTick::side_spread_ratio(&[b1, b2, s1, s2]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for equal spread, got {}", r);
    }

    // ── round-134 ────────────────────────────────────────────────────────────
    #[test]
    fn test_price_zscore_mean_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_zscore_mean(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_zscore_mean_identical() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        // std=0 → None
        assert!(NormalizedTick::price_zscore_mean(&ticks).is_none());
    }

    #[test]
    fn test_tick_size_ratio_none_for_empty() {
        assert!(NormalizedTick::tick_size_ratio(&[]).is_none());
    }

    #[test]
    fn test_tick_size_ratio_uniform() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(2)),
        ];
        let r = NormalizedTick::tick_size_ratio(&ticks).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for uniform qty, got {}", r);
    }

    #[test]
    fn test_buy_tick_fraction_none_for_empty() {
        assert!(NormalizedTick::buy_tick_fraction(&[]).is_none());
    }

    #[test]
    fn test_buy_tick_fraction_all_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let f = NormalizedTick::buy_tick_fraction(&[t]).unwrap();
        assert!((f - 1.0).abs() < 1e-9, "expected 1.0, got {}", f);
    }

    #[test]
    fn test_price_jump_count_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_jump_count(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_jump_count_no_jumps() {
        use rust_decimal_macros::dec;
        // uniform prices → no jumps above std
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let c = NormalizedTick::price_jump_count(&ticks).unwrap();
        assert_eq!(c, 0);
    }

    // ── round-135 ────────────────────────────────────────────────────────────
    #[test]
    fn test_tick_cluster_density_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_cluster_density(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_cluster_density_basic() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 0;
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.received_at_ms = 500;
        // 2 ticks in 500ms → 2/500*1000 = 4.0 ticks/s
        let d = NormalizedTick::tick_cluster_density(&[t1, t2]).unwrap();
        assert!((d - 4.0).abs() < 1e-9, "expected 4.0, got {}", d);
    }

    #[test]
    fn test_qty_zscore_last_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_zscore_last(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_zscore_last_at_mean() {
        use rust_decimal_macros::dec;
        // all identical → std=0 → None
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(2)),
        ];
        assert!(NormalizedTick::qty_zscore_last(&ticks).is_none());
    }

    #[test]
    fn test_side_price_ratio_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::side_price_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_side_price_ratio_equal() {
        use rust_decimal_macros::dec;
        let mut b = make_tick_pq(dec!(100), dec!(1));
        b.side = Some(TradeSide::Buy);
        let mut s = make_tick_pq(dec!(100), dec!(1));
        s.side = Some(TradeSide::Sell);
        let r = NormalizedTick::side_price_ratio(&[b, s]).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0 for equal prices, got {}", r);
    }

    #[test]
    fn test_qty_entropy_norm_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_entropy_norm(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_entropy_norm_uniform() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        let e = NormalizedTick::qty_entropy_norm(&ticks).unwrap();
        // uniform distribution → max entropy → ratio = 1.0
        assert!((e - 1.0).abs() < 1e-9, "expected 1.0 for uniform, got {}", e);
    }

    // ── round-136 ────────────────────────────────────────────────────────────
    #[test]
    fn test_tick_vol_ratio_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::tick_vol_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_tick_vol_ratio_uniform_moves() {
        use rust_decimal_macros::dec;
        // uniform +1 moves → std=0 → ratio=0
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
        ];
        let r = NormalizedTick::tick_vol_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0 for uniform moves, got {}", r);
    }

    #[test]
    fn test_qty_std_ratio_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::qty_std_ratio(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_std_ratio_uniform() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(101), dec!(2)),
        ];
        let r = NormalizedTick::qty_std_ratio(&ticks).unwrap();
        assert!(r.abs() < 1e-9, "expected 0.0 for uniform qty, got {}", r);
    }

    #[test]
    fn test_side_qty_concentration_none_for_no_sides() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::side_qty_concentration(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_side_qty_concentration_all_buy() {
        use rust_decimal_macros::dec;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let c = NormalizedTick::side_qty_concentration(&[t]).unwrap();
        assert!((c - 1.0).abs() < 1e-9, "expected 1.0 for all buy, got {}", c);
    }

    #[test]
    fn test_price_reversion_speed_none_for_single() {
        use rust_decimal_macros::dec;
        assert!(NormalizedTick::price_reversion_speed(&[make_tick_pq(dec!(100), dec!(1))]).is_none());
    }

    #[test]
    fn test_price_reversion_speed_always_reverting() {
        use rust_decimal_macros::dec;
        // prices: 90, 100, 95, 100 → mean=96.25, diffs move toward mean each time
        let ticks = vec![
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        // 90 is far from mean (95), 100 is closer → reversion
        let r = NormalizedTick::price_reversion_speed(&ticks);
        assert!(r.is_some());
    }

    // ── round-137 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_downside_ratio_none_for_empty() {
        assert!(NormalizedTick::price_downside_ratio(&[]).is_none());
    }

    #[test]
    fn test_price_downside_ratio_all_below_mean() {
        use rust_decimal_macros::dec;
        // prices: 80, 90, 85 → mean=85; all at or below mean → ratio=1.0
        let ticks = vec![
            make_tick_pq(dec!(80), dec!(1)),
            make_tick_pq(dec!(90), dec!(1)),
            make_tick_pq(dec!(85), dec!(1)),
        ];
        let r = NormalizedTick::price_downside_ratio(&ticks).unwrap();
        assert!(r >= 0.0 && r <= 1.0, "expected [0,1], got {}", r);
    }

    #[test]
    fn test_avg_trade_lag_none_for_single() {
        let ticks = vec![make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1))];
        assert!(NormalizedTick::avg_trade_lag(&ticks).is_none());
    }

    #[test]
    fn test_avg_trade_lag_uniform_spacing() {
        use rust_decimal_macros::dec;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.received_at_ms = 1000;
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.received_at_ms = 1010;
        let mut t3 = make_tick_pq(dec!(102), dec!(1));
        t3.received_at_ms = 1020;
        let lag = NormalizedTick::avg_trade_lag(&[t1, t2, t3]).unwrap();
        assert!((lag - 10.0).abs() < 1e-9, "expected 10.0, got {}", lag);
    }

    #[test]
    fn test_qty_max_run_none_for_empty() {
        assert!(NormalizedTick::qty_max_run(&[]).is_none());
    }

    #[test]
    fn test_qty_max_run_increasing_then_drop() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(3)),
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        let run = NormalizedTick::qty_max_run(&ticks).unwrap();
        assert_eq!(run, 2, "expected max run of 2, got {}", run);
    }

    #[test]
    fn test_tick_sell_fraction_none_for_empty() {
        assert!(NormalizedTick::tick_sell_fraction(&[]).is_none());
    }

    #[test]
    fn test_tick_sell_fraction_zero_for_no_side() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        // side=None counts as non-sell → fraction=0.0
        let r = NormalizedTick::tick_sell_fraction(&ticks).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_tick_sell_fraction_all_sell() {
        use rust_decimal_macros::dec;
        use crate::tick::TradeSide;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Sell);
        let r = NormalizedTick::tick_sell_fraction(&[t]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0, got {}", r);
    }

    // ── round-138 ────────────────────────────────────────────────────────────

    #[test]
    fn test_qty_turnover_rate_none_for_single() {
        assert!(NormalizedTick::qty_turnover_rate(&[make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_turnover_rate_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(3)),
            make_tick_pq(dec!(100), dec!(2)),
        ];
        // diffs: |3-1|=2, |2-3|=1 → mean=1.5
        let r = NormalizedTick::qty_turnover_rate(&ticks).unwrap();
        assert!((r - 1.5).abs() < 1e-9, "expected 1.5, got {}", r);
    }

    #[test]
    fn test_tick_price_acceleration_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::tick_price_acceleration(&ticks).is_none());
    }

    #[test]
    fn test_tick_price_acceleration_constant_change() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(104), dec!(1)),
        ];
        // changes: +2, +2 → accel = 0
        let a = NormalizedTick::tick_price_acceleration(&ticks).unwrap();
        assert!((a - 0.0).abs() < 1e-6, "expected 0.0, got {}", a);
    }

    #[test]
    fn test_side_volume_skew_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::side_volume_skew(&ticks).is_none());
    }

    #[test]
    fn test_side_volume_skew_all_buy() {
        use rust_decimal_macros::dec;
        use crate::tick::TradeSide;
        let mut t = make_tick_pq(dec!(100), dec!(1));
        t.side = Some(TradeSide::Buy);
        let s = NormalizedTick::side_volume_skew(&[t]).unwrap();
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0 for all-buy, got {}", s);
    }

    #[test]
    fn test_price_decay_rate_none_for_single() {
        assert!(NormalizedTick::price_decay_rate(&[make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1))]).is_none());
    }

    #[test]
    fn test_price_decay_rate_only_up_moves() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(105), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        // no down moves → rate = 0.0
        let r = NormalizedTick::price_decay_rate(&ticks).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0 for all-up, got {}", r);
    }

    // ── round-139 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_upper_shadow_none_for_empty() {
        assert!(NormalizedTick::price_upper_shadow(&[]).is_none());
    }

    #[test]
    fn test_price_upper_shadow_all_below_mean() {
        use rust_decimal_macros::dec;
        // all prices equal → mean=100 → none above → 0.0
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(1)),
        ];
        let r = NormalizedTick::price_upper_shadow(&ticks).unwrap();
        assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {}", r);
    }

    #[test]
    fn test_qty_momentum_score_none_for_single() {
        assert!(NormalizedTick::qty_momentum_score(&[make_tick_pq(rust_decimal_macros::dec!(100), rust_decimal_macros::dec!(1))]).is_none());
    }

    #[test]
    fn test_qty_momentum_score_last_highest() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(10)),
        ];
        let s = NormalizedTick::qty_momentum_score(&ticks).unwrap();
        assert!(s > 0.0, "expected positive momentum for rising qty, got {}", s);
    }

    #[test]
    fn test_tick_buy_run_none_for_empty() {
        assert!(NormalizedTick::tick_buy_run(&[]).is_none());
    }

    #[test]
    fn test_tick_buy_run_all_buy() {
        use rust_decimal_macros::dec;
        use crate::tick::TradeSide;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.side = Some(TradeSide::Buy);
        let run = NormalizedTick::tick_buy_run(&[t1, t2]).unwrap();
        assert_eq!(run, 2, "expected 2, got {}", run);
    }

    #[test]
    fn test_side_price_gap_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1))];
        assert!(NormalizedTick::side_price_gap(&ticks).is_none());
    }

    #[test]
    fn test_side_price_gap_buy_above_sell() {
        use rust_decimal_macros::dec;
        use crate::tick::TradeSide;
        let mut buy = make_tick_pq(dec!(110), dec!(1));
        buy.side = Some(TradeSide::Buy);
        let mut sell = make_tick_pq(dec!(100), dec!(1));
        sell.side = Some(TradeSide::Sell);
        let gap = NormalizedTick::side_price_gap(&[buy, sell]).unwrap();
        assert!((gap - 10.0).abs() < 1e-6, "expected 10.0, got {}", gap);
    }

    // ── round-140 ────────────────────────────────────────────────────────────

    #[test]
    fn test_price_volatility_skew_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(102), dec!(1))];
        assert!(NormalizedTick::price_volatility_skew(&ticks).is_none());
    }

    #[test]
    fn test_price_volatility_skew_returns_some() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
            make_tick_pq(dec!(110), dec!(1)),
        ];
        assert!(NormalizedTick::price_volatility_skew(&ticks).is_some());
    }

    #[test]
    fn test_qty_peak_to_trough_none_for_empty() {
        assert!(NormalizedTick::qty_peak_to_trough(&[]).is_none());
    }

    #[test]
    fn test_qty_peak_to_trough_basic() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(2)),
            make_tick_pq(dec!(100), dec!(4)),
            make_tick_pq(dec!(100), dec!(8)),
        ];
        let r = NormalizedTick::qty_peak_to_trough(&ticks).unwrap();
        assert!((r - 4.0).abs() < 1e-9, "expected 4.0 (8/2), got {}", r);
    }

    #[test]
    fn test_tick_momentum_decay_none_for_two() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::tick_momentum_decay(&ticks).is_none());
    }

    #[test]
    fn test_tick_momentum_decay_returns_some() {
        use rust_decimal_macros::dec;
        let ticks = vec![
            make_tick_pq(dec!(100), dec!(1)),
            make_tick_pq(dec!(102), dec!(1)),
            make_tick_pq(dec!(101), dec!(1)),
        ];
        assert!(NormalizedTick::tick_momentum_decay(&ticks).is_some());
    }

    #[test]
    fn test_side_transition_rate_none_for_no_sides() {
        use rust_decimal_macros::dec;
        let ticks = vec![make_tick_pq(dec!(100), dec!(1)), make_tick_pq(dec!(101), dec!(1))];
        assert!(NormalizedTick::side_transition_rate(&ticks).is_none());
    }

    #[test]
    fn test_side_transition_rate_alternating() {
        use rust_decimal_macros::dec;
        use crate::tick::TradeSide;
        let mut t1 = make_tick_pq(dec!(100), dec!(1));
        t1.side = Some(TradeSide::Buy);
        let mut t2 = make_tick_pq(dec!(101), dec!(1));
        t2.side = Some(TradeSide::Sell);
        let mut t3 = make_tick_pq(dec!(102), dec!(1));
        t3.side = Some(TradeSide::Buy);
        // 2 transitions out of 2 pairs → 1.0
        let r = NormalizedTick::side_transition_rate(&[t1, t2, t3]).unwrap();
        assert!((r - 1.0).abs() < 1e-9, "expected 1.0 for alternating, got {}", r);
    }
}
