//! Top of book (best bid / best ask) normalization.

use super::{
    epoch_to_ms, ms_field, parse_decimal_field, rfc3339_ms_field, Exchange, RawTick, TickNormalizer,
};
use crate::error::StreamError;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tracing::trace;

/// Canonical best bid / best ask quote, exchange-agnostic.
///
/// Built by [`TickNormalizer::normalize_quote`] from one ticker or quote
/// payload, or by [`TickNormalizer::normalize_quote_frame`] from a whole
/// WebSocket frame. Prices and sizes are exact [`Decimal`]s.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NormalizedQuote {
    /// Source exchange.
    pub exchange: Exchange,
    /// Instrument symbol.
    pub symbol: String,
    /// Best bid price.
    pub bid_price: Decimal,
    /// Quantity resting at the best bid, when the feed sends it.
    pub bid_qty: Option<Decimal>,
    /// Best ask price.
    pub ask_price: Decimal,
    /// Quantity resting at the best ask, when the feed sends it.
    pub ask_qty: Option<Decimal>,
    /// Exchange-side timestamp (ms since Unix epoch), if included in the feed.
    pub exchange_ts_ms: Option<u64>,
    /// Local system-clock timestamp when this quote was received.
    pub received_at_ms: u64,
}

impl NormalizedQuote {
    /// Midpoint: `(bid + ask) / 2`.
    pub fn mid_price(&self) -> Decimal {
        (self.bid_price + self.ask_price) / Decimal::TWO
    }

    /// Absolute spread: `ask - bid`. Negative when the quote is crossed.
    pub fn spread(&self) -> Decimal {
        self.ask_price - self.bid_price
    }

    /// Spread in basis points of the mid price. `None` when the mid is zero.
    pub fn spread_bps(&self) -> Option<Decimal> {
        let mid = self.mid_price();
        if mid.is_zero() {
            return None;
        }
        Some(self.spread() / mid * Decimal::from(10_000))
    }

    /// `true` when the bid is above the ask.
    pub fn is_crossed(&self) -> bool {
        self.bid_price > self.ask_price
    }

    /// `true` when the bid equals the ask.
    pub fn is_locked(&self) -> bool {
        self.bid_price == self.ask_price
    }
}

impl From<&NormalizedQuote> for crate::protocol::QuoteEvent {
    /// Lossy conversion to the `f64` [`QuoteEvent`](crate::protocol::QuoteEvent)
    /// used by [`MarketEvent`](crate::protocol::MarketEvent). Missing sizes
    /// become `0.0`; the timestamp is the exchange time when present, else the
    /// receive time, in nanoseconds.
    fn from(q: &NormalizedQuote) -> Self {
        let f = |d: Decimal| d.to_f64().unwrap_or(0.0);
        let ms = q.exchange_ts_ms.unwrap_or(q.received_at_ms);
        Self {
            symbol: q.symbol.clone(),
            bid: f(q.bid_price),
            ask: f(q.ask_price),
            bid_size: q.bid_qty.map(f).unwrap_or(0.0),
            ask_size: q.ask_qty.map(f).unwrap_or(0.0),
            timestamp_ns: ms.saturating_mul(1_000_000),
            exchange: q.exchange.to_string(),
        }
    }
}

/// Field names for one venue's quote payload.
struct QuoteFields {
    bid: &'static str,
    bid_qty: &'static [&'static str],
    ask: &'static str,
    ask_qty: &'static [&'static str],
}

impl TickNormalizer {
    /// Normalize one ticker / quote payload into a [`NormalizedQuote`].
    ///
    /// The payload is the inner object for one instrument:
    ///
    /// | Exchange | Payload | Fields |
    /// |---|---|---|
    /// | Binance | `bookTicker` (spot or futures), `24hrTicker` | `b` `B` `a` `A`, time `T` or `E` |
    /// | Coinbase | Exchange `ticker`, Advanced Trade `ticker` / `ticker_batch` entry | `best_bid` `best_bid_size`/`best_bid_quantity` `best_ask` ..., time `time` |
    /// | Alpaca | quote message (`"T": "q"`) | `bp` `bs` `ap` `as`, RFC 3339 `t` |
    /// | Polygon | stocks `Q` or crypto `XQ` event | `bp` `bs` `ap` `as`, epoch `t` |
    /// | Kraken | v2 `ticker` data entry | `bid` `bid_qty` `ask` `ask_qty`, RFC 3339 `timestamp` |
    /// | OKX | v5 `tickers` data entry | `bidPx` `bidSz` `askPx` `askSz`, `ts` |
    /// | Bybit | v5 `tickers.{symbol}` data object | `bid1Price` `bid1Size` `ask1Price` `ask1Size` |
    ///
    /// ```
    /// use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
    /// use serde_json::json;
    ///
    /// let raw = RawTick::new(Exchange::Binance, "BNBUSDT", json!({
    ///     "u": 400900217, "s": "BNBUSDT",
    ///     "b": "25.35190000", "B": "31.21000000",
    ///     "a": "25.36520000", "A": "40.66000000"
    /// }));
    /// let q = TickNormalizer::new().normalize_quote(raw).unwrap();
    /// assert_eq!(q.bid_price.to_string(), "25.35190000");
    /// assert_eq!(q.spread().to_string(), "0.01330000");
    /// ```
    ///
    /// # Errors
    ///
    /// [`StreamError::ParseError`] if the bid or ask price is missing or not a
    /// number, and [`StreamError::InvalidTick`] if a price is not positive or a
    /// size is negative.
    pub fn normalize_quote(&self, raw: RawTick) -> Result<NormalizedQuote, StreamError> {
        let p = &raw.payload;
        let (fields, ts) = match raw.exchange {
            Exchange::Binance => (
                QuoteFields {
                    bid: "b",
                    bid_qty: &["B"],
                    ask: "a",
                    ask_qty: &["A"],
                },
                ms_field(p, "T").or_else(|| ms_field(p, "E")),
            ),
            Exchange::Coinbase => (
                QuoteFields {
                    bid: "best_bid",
                    bid_qty: &["best_bid_size", "best_bid_quantity"],
                    ask: "best_ask",
                    ask_qty: &["best_ask_size", "best_ask_quantity"],
                },
                rfc3339_ms_field(p, "time"),
            ),
            Exchange::Alpaca => (
                QuoteFields {
                    bid: "bp",
                    bid_qty: &["bs"],
                    ask: "ap",
                    ask_qty: &["as"],
                },
                rfc3339_ms_field(p, "t"),
            ),
            Exchange::Polygon => (
                QuoteFields {
                    bid: "bp",
                    bid_qty: &["bs"],
                    ask: "ap",
                    ask_qty: &["as"],
                },
                p.get("t").and_then(|v| v.as_u64()).map(epoch_to_ms),
            ),
            Exchange::Kraken => (
                QuoteFields {
                    bid: "bid",
                    bid_qty: &["bid_qty"],
                    ask: "ask",
                    ask_qty: &["ask_qty"],
                },
                rfc3339_ms_field(p, "timestamp"),
            ),
            Exchange::Okx => (
                QuoteFields {
                    bid: "bidPx",
                    bid_qty: &["bidSz"],
                    ask: "askPx",
                    ask_qty: &["askSz"],
                },
                ms_field(p, "ts"),
            ),
            Exchange::Bybit => (
                QuoteFields {
                    bid: "bid1Price",
                    bid_qty: &["bid1Size"],
                    ask: "ask1Price",
                    ask_qty: &["ask1Size"],
                },
                ms_field(p, "ts"),
            ),
        };
        let ex = raw.exchange.to_string();
        let qty = |names: &[&str]| -> Result<Option<Decimal>, StreamError> {
            match names.iter().find(|n| p.get(**n).is_some()) {
                Some(n) => parse_decimal_field(p, n, &ex).map(Some),
                None => Ok(None),
            }
        };
        let quote = NormalizedQuote {
            exchange: raw.exchange,
            bid_price: parse_decimal_field(p, fields.bid, &ex)?,
            bid_qty: qty(fields.bid_qty)?,
            ask_price: parse_decimal_field(p, fields.ask, &ex)?,
            ask_qty: qty(fields.ask_qty)?,
            exchange_ts_ms: ts,
            received_at_ms: raw.received_at_ms,
            symbol: raw.symbol,
        };
        for (name, price) in [("bid", quote.bid_price), ("ask", quote.ask_price)] {
            if price <= Decimal::ZERO {
                return Err(StreamError::InvalidTick {
                    reason: format!("{name} price must be positive, got {price}"),
                });
            }
        }
        for (name, q) in [("bid", quote.bid_qty), ("ask", quote.ask_qty)] {
            if let Some(q) = q {
                if q < Decimal::ZERO {
                    return Err(StreamError::InvalidTick {
                        reason: format!("{name} size must be non-negative, got {q}"),
                    });
                }
            }
        }
        trace!(
            exchange = %quote.exchange,
            symbol = %quote.symbol,
            bid = %quote.bid_price,
            ask = %quote.ask_price,
            "quote normalized"
        );
        Ok(quote)
    }
}
