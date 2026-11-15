//! Trade normalizers for Kraken, OKX and Bybit public trade channels.
//!
//! Each takes one trade element (the object or array inside the venue's
//! `data` list), the same way the Binance, Coinbase, Alpaca and Polygon
//! normalizers take one trade object. Whole frames go through
//! [`TickNormalizer::normalize_trade_frame`].

use super::{
    epoch_to_ms, id_field, ms_field, parse_decimal_field, parse_decimal_value, parse_side_ci,
    rfc3339_ms_field, NormalizedTick, RawTick, TickNormalizer,
};
use crate::error::StreamError;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::str::FromStr;

impl TickNormalizer {
    /// Kraken. Two element shapes:
    /// - WebSocket v2 `trade` channel object: `price`, `qty`, `side` (taker),
    ///   `trade_id`, RFC 3339 `timestamp`.
    /// - WebSocket v1 trade array: `[price, volume, time, side, orderType, misc]`
    ///   with `time` in decimal seconds and `side` as `"b"`/`"s"` (taker).
    pub(super) fn normalize_kraken(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let ex = raw.exchange.to_string();
        let p = &raw.payload;
        if let Some(arr) = p.as_array() {
            let field = |i: usize, name: &str| {
                arr.get(i).ok_or_else(|| StreamError::ParseError {
                    exchange: ex.clone(),
                    reason: format!("v1 trade array has no {name} at index {i}"),
                })
            };
            let price = parse_decimal_value(field(0, "price")?, "price", &ex)?;
            let quantity = parse_decimal_value(field(1, "volume")?, "volume", &ex)?;
            let exchange_ts_ms = arr.get(2).and_then(seconds_to_ms);
            let side = arr.get(3).and_then(|v| v.as_str()).and_then(parse_side_ci);
            return Ok(NormalizedTick {
                exchange: raw.exchange,
                symbol: raw.symbol,
                price,
                quantity,
                side,
                trade_id: None,
                exchange_ts_ms,
                received_at_ms: raw.received_at_ms,
            });
        }
        let price = parse_decimal_field(p, "price", &ex)?;
        let quantity = parse_decimal_field(p, "qty", &ex)?;
        let side = p
            .get("side")
            .and_then(|v| v.as_str())
            .and_then(parse_side_ci);
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity,
            side,
            trade_id: id_field(p, "trade_id"),
            exchange_ts_ms: rfc3339_ms_field(p, "timestamp"),
            received_at_ms: raw.received_at_ms,
        })
    }

    /// OKX v5 `trades` / `trades-all` element: `px`, `sz`, `side` (taker),
    /// `tradeId`, `ts` (ms as a string).
    pub(super) fn normalize_okx(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let ex = raw.exchange.to_string();
        let p = &raw.payload;
        let price = parse_decimal_field(p, "px", &ex)?;
        let quantity = parse_decimal_field(p, "sz", &ex)?;
        let side = p
            .get("side")
            .and_then(|v| v.as_str())
            .and_then(parse_side_ci);
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity,
            side,
            trade_id: id_field(p, "tradeId"),
            exchange_ts_ms: ms_field(p, "ts"),
            received_at_ms: raw.received_at_ms,
        })
    }

    /// Bybit v5 `publicTrade.{symbol}` element: `p`, `v`, `S` (taker side,
    /// `"Buy"`/`"Sell"`), `i`, `T` (ms).
    pub(super) fn normalize_bybit(&self, raw: RawTick) -> Result<NormalizedTick, StreamError> {
        let ex = raw.exchange.to_string();
        let p = &raw.payload;
        let price = parse_decimal_field(p, "p", &ex)?;
        let quantity = parse_decimal_field(p, "v", &ex)?;
        let side = p.get("S").and_then(|v| v.as_str()).and_then(parse_side_ci);
        Ok(NormalizedTick {
            exchange: raw.exchange,
            symbol: raw.symbol,
            price,
            quantity,
            side,
            trade_id: id_field(p, "i"),
            exchange_ts_ms: ms_field(p, "T").map(epoch_to_ms),
            received_at_ms: raw.received_at_ms,
        })
    }
}

/// Decimal seconds (`"1534614057.321597"` or a JSON number) to whole ms.
fn seconds_to_ms(v: &serde_json::Value) -> Option<u64> {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let secs = Decimal::from_str(&s).ok()?;
    (secs * Decimal::from(1_000)).trunc().to_u64()
}
