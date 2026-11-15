//! Whole-frame parsing: one WebSocket text frame, envelope included, to zero
//! or more ticks or quotes.

use super::{
    ms_field, now_ms, rfc3339_ms_field, Exchange, NormalizedQuote, NormalizedTick, RawTick,
    TickNormalizer,
};
use crate::error::StreamError;
use serde_json::Value;

/// What the frame walker is looking for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Trade,
    Quote,
}

/// One payload found in a frame: symbol, inner element, and an envelope
/// timestamp to use when the element has none.
struct Item<'a> {
    symbol: String,
    payload: &'a Value,
    envelope_ts_ms: Option<u64>,
}

impl TickNormalizer {
    /// Normalize a whole trade frame, as received from the venue's WebSocket,
    /// into ticks.
    ///
    /// Unwraps the envelope and reads the symbol from the message, so the
    /// caller does not need to know the wire layout:
    ///
    /// | Exchange | Recognised frames |
    /// |---|---|
    /// | Binance | `trade` and `aggTrade` events, bare or in a combined-stream `{"stream", "data"}` wrapper |
    /// | Coinbase | Exchange `match` / `last_match`; Advanced Trade `market_trades` (every trade in every event) |
    /// | Alpaca | array (or single object) of messages, trades are `"T": "t"` |
    /// | Polygon | array (or single object) of events, `T` (stocks) and `XT` (crypto) |
    /// | Kraken | v2 `{"channel": "trade", "data": [...]}`; v1 `[id, [[...], ...], "trade", pair]` |
    /// | OKX | `{"arg": {"channel": "trades" or "trades-all"}, "data": [...]}` |
    /// | Bybit | `{"topic": "publicTrade.{symbol}", "data": [...]}` |
    ///
    /// Frames that carry no trades (subscription acks, heartbeats, other
    /// channels) give an empty `Vec`, so every frame from a socket can be
    /// passed in. Every tick gets the same `received_at_ms`, the time of the
    /// call.
    ///
    /// ```
    /// use fin_stream::tick::{Exchange, TickNormalizer, TradeSide};
    ///
    /// let frame: serde_json::Value = serde_json::from_str(r#"{
    ///   "arg": {"channel": "trades", "instId": "BTC-USDT"},
    ///   "data": [{"instId": "BTC-USDT", "tradeId": "130639474", "px": "42219.9",
    ///             "sz": "0.12060306", "side": "buy", "ts": "1630048897897"}]
    /// }"#).unwrap();
    /// let ticks = TickNormalizer::new().normalize_trade_frame(Exchange::Okx, &frame).unwrap();
    /// assert_eq!(ticks.len(), 1);
    /// assert_eq!(ticks[0].symbol, "BTC-USDT");
    /// assert_eq!(ticks[0].side, Some(TradeSide::Buy));
    /// assert_eq!(ticks[0].exchange_ts_ms, Some(1_630_048_897_897));
    /// ```
    ///
    /// # Errors
    ///
    /// The first error from [`normalize`](Self::normalize) on a trade in the
    /// frame, or [`StreamError::ParseError`] when a trade's symbol is missing.
    pub fn normalize_trade_frame(
        &self,
        exchange: Exchange,
        frame: &Value,
    ) -> Result<Vec<NormalizedTick>, StreamError> {
        let received_at_ms = now_ms();
        collect(exchange, frame, Kind::Trade)?
            .into_iter()
            .map(|item| {
                let mut tick = self.normalize(RawTick {
                    exchange,
                    symbol: item.symbol,
                    payload: item.payload.clone(),
                    received_at_ms,
                })?;
                if tick.exchange_ts_ms.is_none() {
                    tick.exchange_ts_ms = item.envelope_ts_ms;
                }
                Ok(tick)
            })
            .collect()
    }

    /// Normalize a whole ticker / quote frame into [`NormalizedQuote`]s.
    ///
    /// | Exchange | Recognised frames |
    /// |---|---|
    /// | Binance | `bookTicker` (spot frames have no `e`; futures have `"e": "bookTicker"`) and `24hrTicker`, bare or combined-stream |
    /// | Coinbase | Exchange `ticker`; Advanced Trade `ticker` and `ticker_batch` (time from the envelope `timestamp`) |
    /// | Alpaca | messages with `"T": "q"` |
    /// | Polygon | `Q` (stocks) and `XQ` (crypto) events |
    /// | Kraken | v2 `{"channel": "ticker", "data": [...]}` |
    /// | OKX | `{"arg": {"channel": "tickers"}, "data": [...]}` |
    /// | Bybit | `{"topic": "tickers.{symbol}", "data": {...}}` (time from the envelope `ts`) |
    ///
    /// Bybit `tickers` delta frames only carry the fields that changed; a
    /// delta without both `bid1Price` and `ask1Price` is skipped rather than
    /// reported as an error. Frames with no quotes give an empty `Vec`.
    ///
    /// ```
    /// use fin_stream::tick::{Exchange, TickNormalizer};
    ///
    /// let frame: serde_json::Value = serde_json::from_str(r#"[
    ///   {"T": "q", "S": "AMD", "bx": "U", "bp": 87.66, "bs": 1, "ax": "Q",
    ///    "ap": 87.68, "as": 4, "t": "2021-02-22T15:51:45.335689322Z"}
    /// ]"#).unwrap();
    /// let quotes = TickNormalizer::new().normalize_quote_frame(Exchange::Alpaca, &frame).unwrap();
    /// assert_eq!(quotes[0].symbol, "AMD");
    /// assert_eq!(quotes[0].spread().to_string(), "0.02");
    /// ```
    ///
    /// # Errors
    ///
    /// The first error from [`normalize_quote`](Self::normalize_quote) on a
    /// quote in the frame, or [`StreamError::ParseError`] when its symbol is
    /// missing.
    pub fn normalize_quote_frame(
        &self,
        exchange: Exchange,
        frame: &Value,
    ) -> Result<Vec<NormalizedQuote>, StreamError> {
        let received_at_ms = now_ms();
        collect(exchange, frame, Kind::Quote)?
            .into_iter()
            .map(|item| {
                let mut quote = self.normalize_quote(RawTick {
                    exchange,
                    symbol: item.symbol,
                    payload: item.payload.clone(),
                    received_at_ms,
                })?;
                if quote.exchange_ts_ms.is_none() {
                    quote.exchange_ts_ms = item.envelope_ts_ms;
                }
                Ok(quote)
            })
            .collect()
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn symbol(
    exchange: Exchange,
    v: &Value,
    keys: &[&str],
    fallback: Option<&str>,
) -> Result<String, StreamError> {
    keys.iter()
        .find_map(|k| str_at(v, k))
        .or(fallback)
        .map(str::to_string)
        .ok_or_else(|| StreamError::ParseError {
            exchange: exchange.to_string(),
            reason: format!("message has no symbol field ({})", keys.join(", ")),
        })
}

/// A frame that is either one message or a JSON array of messages.
fn messages(frame: &Value) -> Vec<&Value> {
    match frame {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

fn collect(exchange: Exchange, frame: &Value, kind: Kind) -> Result<Vec<Item<'_>>, StreamError> {
    let mut out = Vec::new();
    match exchange {
        Exchange::Binance => {
            // Combined streams wrap the event: {"stream": "...", "data": {...}}.
            let msg = match frame.get("data") {
                Some(d) if frame.get("stream").is_some() => d,
                _ => frame,
            };
            let event = str_at(msg, "e");
            let wanted = match kind {
                Kind::Trade => matches!(event, Some("trade") | Some("aggTrade")),
                Kind::Quote => match event {
                    Some(e) => e == "bookTicker" || e == "24hrTicker",
                    // Spot bookTicker frames have no event type.
                    None => ["u", "b", "a"].iter().all(|k| msg.get(*k).is_some()),
                },
            };
            if wanted {
                out.push(Item {
                    symbol: symbol(exchange, msg, &["s"], None)?,
                    payload: msg,
                    envelope_ts_ms: None,
                });
            }
        }
        Exchange::Coinbase => {
            let msg_type = str_at(frame, "type");
            match (kind, str_at(frame, "channel")) {
                (Kind::Trade, Some("market_trades")) => {
                    for item in events(frame, "trades") {
                        out.push(Item {
                            symbol: symbol(exchange, item, &["product_id"], None)?,
                            payload: item,
                            envelope_ts_ms: None,
                        });
                    }
                }
                (Kind::Quote, Some("ticker" | "ticker_batch")) => {
                    let ts = rfc3339_ms_field(frame, "timestamp");
                    for item in events(frame, "tickers") {
                        out.push(Item {
                            symbol: symbol(exchange, item, &["product_id"], None)?,
                            payload: item,
                            envelope_ts_ms: ts,
                        });
                    }
                }
                (Kind::Trade, None) if matches!(msg_type, Some("match" | "last_match")) => {
                    out.push(Item {
                        symbol: symbol(exchange, frame, &["product_id"], None)?,
                        payload: frame,
                        envelope_ts_ms: None,
                    });
                }
                (Kind::Quote, None) if msg_type == Some("ticker") => {
                    out.push(Item {
                        symbol: symbol(exchange, frame, &["product_id"], None)?,
                        payload: frame,
                        envelope_ts_ms: None,
                    });
                }
                _ => {}
            }
        }
        Exchange::Alpaca => {
            let tag = if kind == Kind::Trade { "t" } else { "q" };
            for msg in messages(frame) {
                if str_at(msg, "T") == Some(tag) {
                    out.push(Item {
                        symbol: symbol(exchange, msg, &["S"], None)?,
                        payload: msg,
                        envelope_ts_ms: None,
                    });
                }
            }
        }
        Exchange::Polygon => {
            let tags: [&str; 2] = if kind == Kind::Trade {
                ["T", "XT"]
            } else {
                ["Q", "XQ"]
            };
            for msg in messages(frame) {
                if str_at(msg, "ev").is_some_and(|ev| tags.contains(&ev)) {
                    out.push(Item {
                        symbol: symbol(exchange, msg, &["sym", "pair"], None)?,
                        payload: msg,
                        envelope_ts_ms: None,
                    });
                }
            }
        }
        Exchange::Kraken => {
            let channel = if kind == Kind::Trade {
                "trade"
            } else {
                "ticker"
            };
            if let Some(arr) = frame.as_array() {
                // v1: [channelID, [[price, volume, time, side, type, misc], ...], "trade", "XBT/USD"]
                if kind == Kind::Trade
                    && arr.len() >= 4
                    && arr.get(arr.len() - 2).and_then(Value::as_str) == Some("trade")
                {
                    let pair = arr.last().and_then(Value::as_str);
                    if let Some(trades) = arr.get(1).and_then(Value::as_array) {
                        for t in trades {
                            out.push(Item {
                                symbol: symbol(exchange, t, &[], pair)?,
                                payload: t,
                                envelope_ts_ms: None,
                            });
                        }
                    }
                }
            } else if str_at(frame, "channel") == Some(channel) {
                for item in frame
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    out.push(Item {
                        symbol: symbol(exchange, item, &["symbol"], None)?,
                        payload: item,
                        envelope_ts_ms: None,
                    });
                }
            }
        }
        Exchange::Okx => {
            let arg = frame.get("arg");
            let channel = arg.and_then(|a| str_at(a, "channel"));
            let wanted = match kind {
                Kind::Trade => matches!(channel, Some("trades" | "trades-all")),
                Kind::Quote => channel == Some("tickers"),
            };
            if wanted {
                let inst = arg.and_then(|a| str_at(a, "instId"));
                for item in frame
                    .get("data")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    out.push(Item {
                        symbol: symbol(exchange, item, &["instId"], inst)?,
                        payload: item,
                        envelope_ts_ms: None,
                    });
                }
            }
        }
        Exchange::Bybit => {
            let topic = str_at(frame, "topic").unwrap_or("");
            let ts = ms_field(frame, "ts");
            match kind {
                Kind::Trade => {
                    if let Some(sym) = topic.strip_prefix("publicTrade.") {
                        for item in frame
                            .get("data")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            out.push(Item {
                                symbol: symbol(exchange, item, &["s"], Some(sym))?,
                                payload: item,
                                envelope_ts_ms: ts,
                            });
                        }
                    }
                }
                Kind::Quote => {
                    if let Some(sym) = topic.strip_prefix("tickers.") {
                        if let Some(item) = frame.get("data") {
                            if item.get("bid1Price").is_some() && item.get("ask1Price").is_some() {
                                out.push(Item {
                                    symbol: symbol(exchange, item, &["symbol"], Some(sym))?,
                                    payload: item,
                                    envelope_ts_ms: ts,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Every `events[].{key}[]` entry of a Coinbase Advanced Trade frame.
fn events<'a>(frame: &'a Value, key: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    frame
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(move |e| e.get(key).and_then(Value::as_array))
        .flatten()
}
