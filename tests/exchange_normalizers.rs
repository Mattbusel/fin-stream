//! Trade and quote normalization against sample messages from each venue's
//! public WebSocket documentation (fixtures in `tests/fixtures/exchanges/`).
//!
//! Needs no network and no `net` feature.
#![allow(clippy::unwrap_used, clippy::panic)]

use fin_stream::error::StreamError;
use fin_stream::protocol::QuoteEvent;
use fin_stream::tick::{Exchange, NormalizedQuote, RawTick, TickNormalizer, TradeSide};
use rust_decimal_macros::dec;
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/exchanges/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn n() -> TickNormalizer {
    TickNormalizer::new()
}

fn raw(exchange: Exchange, symbol: &str, payload: Value) -> RawTick {
    RawTick {
        exchange,
        symbol: symbol.into(),
        payload,
        received_at_ms: 42,
    }
}

// ---------------------------------------------------------------- Coinbase fix

#[test]
fn coinbase_exchange_match_side_is_maker_side_and_is_inverted() {
    // `side: "sell"` on the matches channel means the resting maker order was a
    // sell, so the taker (aggressor) bought.
    let ticks = n()
        .normalize_trade_frame(Exchange::Coinbase, &fixture("coinbase_exchange_match.json"))
        .unwrap();
    assert_eq!(ticks.len(), 1);
    let t = &ticks[0];
    assert_eq!(t.symbol, "BTC-USD");
    assert_eq!(t.side, Some(TradeSide::Buy));
    assert_eq!((t.price, t.quantity), (dec!(400.23), dec!(5.23512)));
    assert_eq!(
        t.trade_id.as_deref(),
        Some("10"),
        "integer trade_id is kept"
    );
    assert_eq!(t.exchange_ts_ms, Some(1_415_348_367_028));
}

#[test]
fn coinbase_exchange_match_buy_maker_is_sell_aggressor() {
    let mut payload = fixture("coinbase_exchange_match.json");
    payload["side"] = json!("buy");
    let t = n()
        .normalize(raw(Exchange::Coinbase, "BTC-USD", payload))
        .unwrap();
    assert_eq!(t.side, Some(TradeSide::Sell));

    // `last_match` (sent once on subscribe) has the same semantics.
    let payload = json!({"type": "last_match", "price": "1", "size": "1", "side": "sell"});
    let t = n()
        .normalize(raw(Exchange::Coinbase, "BTC-USD", payload))
        .unwrap();
    assert_eq!(t.side, Some(TradeSide::Buy));
}

#[test]
fn coinbase_advanced_trade_uppercase_side_is_taker_side() {
    let ticks = n()
        .normalize_trade_frame(
            Exchange::Coinbase,
            &fixture("coinbase_advanced_market_trades.json"),
        )
        .unwrap();
    assert_eq!(ticks.len(), 2);
    assert_eq!(
        ticks[0].side,
        Some(TradeSide::Buy),
        "BUY must not fall through to Sell"
    );
    assert_eq!(ticks[1].side, Some(TradeSide::Sell));
    assert_eq!(ticks[0].symbol, "ETH-USD");
    assert_eq!(ticks[0].trade_id.as_deref(), Some("000000000"));
    assert_eq!(ticks[0].exchange_ts_ms, Some(1_565_815_347_265));
    assert_eq!(ticks[0].price, dec!(1260.01));
}

#[test]
fn coinbase_unknown_side_is_none_not_sell() {
    let payload = json!({"price": "1", "size": "1", "side": "UNKNOWN_ORDER_SIDE"});
    let t = n()
        .normalize(raw(Exchange::Coinbase, "BTC-USD", payload))
        .unwrap();
    assert_eq!(t.side, None);
}

// ---------------------------------------------------------------- new venues: trades

#[test]
fn kraken_v2_trade_frame() {
    let ticks = n()
        .normalize_trade_frame(Exchange::Kraken, &fixture("kraken_v2_trade.json"))
        .unwrap();
    assert_eq!(ticks.len(), 1);
    let t = &ticks[0];
    assert_eq!(
        (t.exchange, t.symbol.as_str()),
        (Exchange::Kraken, "MATIC/USD")
    );
    assert_eq!((t.price, t.quantity), (dec!(0.5117), dec!(40)));
    assert_eq!(t.side, Some(TradeSide::Sell));
    assert_eq!(t.trade_id.as_deref(), Some("4665906"));
    assert_eq!(t.exchange_ts_ms, Some(1_695_628_177_708));
}

#[test]
fn kraken_v1_trade_array_frame() {
    let ticks = n()
        .normalize_trade_frame(Exchange::Kraken, &fixture("kraken_v1_trade.json"))
        .unwrap();
    assert_eq!(ticks.len(), 2);
    assert!(ticks.iter().all(|t| t.symbol == "XBT/USD"));
    assert_eq!(
        (ticks[0].price, ticks[0].quantity),
        (dec!(5541.20000), dec!(0.15850568))
    );
    assert_eq!(ticks[0].side, Some(TradeSide::Sell));
    assert_eq!(ticks[0].exchange_ts_ms, Some(1_534_614_057_321));
    assert_eq!(ticks[1].side, Some(TradeSide::Buy));
    assert_eq!(ticks[1].exchange_ts_ms, Some(1_534_614_057_324));
    assert_eq!(ticks[1].trade_id, None);
}

#[test]
fn kraken_number_in_scientific_notation_parses_exactly() {
    let payload =
        json!({"symbol": "BTC/USD", "side": "buy", "price": 64000.5, "qty": 1e-8, "trade_id": 1});
    let t = n()
        .normalize(raw(Exchange::Kraken, "BTC/USD", payload))
        .unwrap();
    assert_eq!(t.quantity, dec!(0.00000001));
}

#[test]
fn okx_trades_frame() {
    let ticks = n()
        .normalize_trade_frame(Exchange::Okx, &fixture("okx_trades.json"))
        .unwrap();
    assert_eq!(ticks.len(), 1);
    let t = &ticks[0];
    assert_eq!((t.exchange, t.symbol.as_str()), (Exchange::Okx, "BTC-USDT"));
    assert_eq!((t.price, t.quantity), (dec!(42219.9), dec!(0.12060306)));
    assert_eq!(t.side, Some(TradeSide::Buy));
    assert_eq!(t.trade_id.as_deref(), Some("130639474"));
    assert_eq!(t.exchange_ts_ms, Some(1_630_048_897_897));
}

#[test]
fn bybit_public_trade_frame() {
    let ticks = n()
        .normalize_trade_frame(Exchange::Bybit, &fixture("bybit_public_trade.json"))
        .unwrap();
    assert_eq!(ticks.len(), 1);
    let t = &ticks[0];
    assert_eq!(
        (t.exchange, t.symbol.as_str()),
        (Exchange::Bybit, "BTCUSDT")
    );
    assert_eq!((t.price, t.quantity), (dec!(16578.50), dec!(0.001)));
    assert_eq!(t.side, Some(TradeSide::Buy));
    assert_eq!(
        t.trade_id.as_deref(),
        Some("20f43950-d8dd-5b31-9112-a178eb6023af")
    );
    assert_eq!(t.exchange_ts_ms, Some(1_672_304_486_865));
}

#[test]
fn new_venues_normalize_single_elements_through_raw_tick() {
    let okx = json!({"instId": "ETH-USDT", "tradeId": "7", "px": "2500.5", "sz": "2", "side": "sell", "ts": "1700000000000"});
    let t = n().normalize(raw(Exchange::Okx, "ETH-USDT", okx)).unwrap();
    assert_eq!((t.side, t.received_at_ms), (Some(TradeSide::Sell), 42));

    let bybit = json!({"T": 1700000000000u64, "s": "ETHUSDT", "S": "Sell", "v": "0.5", "p": "2500", "i": "x"});
    let t = n()
        .normalize(raw(Exchange::Bybit, "ETHUSDT", bybit))
        .unwrap();
    assert_eq!(t.side, Some(TradeSide::Sell));
}

#[test]
fn new_venues_reject_bad_trades() {
    let missing_px = json!({"instId": "BTC-USDT", "sz": "1", "side": "buy"});
    assert!(matches!(
        n().normalize(raw(Exchange::Okx, "BTC-USDT", missing_px)),
        Err(StreamError::ParseError { .. })
    ));
    let zero_price = json!({"p": "0", "v": "1", "S": "Buy"});
    assert!(matches!(
        n().normalize(raw(Exchange::Bybit, "BTCUSDT", zero_price)),
        Err(StreamError::InvalidTick { .. })
    ));
    let short_v1 = json!(["5541.2"]);
    assert!(matches!(
        n().normalize(raw(Exchange::Kraken, "XBT/USD", short_v1)),
        Err(StreamError::ParseError { .. })
    ));
    // A frame with a bad trade fails as a whole.
    let frame = json!({"arg": {"channel": "trades", "instId": "BTC-USDT"}, "data": [{"sz": "1"}]});
    assert!(n().normalize_trade_frame(Exchange::Okx, &frame).is_err());
}

// ---------------------------------------------------------------- existing venues: frames

#[test]
fn binance_trade_and_combined_agg_trade_frames() {
    let t = &n()
        .normalize_trade_frame(Exchange::Binance, &fixture("binance_trade.json"))
        .unwrap()[0];
    assert_eq!(t.symbol, "BNBBTC");
    assert_eq!(
        t.side,
        Some(TradeSide::Sell),
        "buyer is maker, seller aggressed"
    );
    assert_eq!(t.trade_id.as_deref(), Some("12345"));
    assert_eq!(t.exchange_ts_ms, Some(1_672_515_782_136));

    let t = &n()
        .normalize_trade_frame(
            Exchange::Binance,
            &fixture("binance_combined_agg_trade.json"),
        )
        .unwrap()[0];
    assert_eq!(t.side, Some(TradeSide::Buy));
    assert_eq!(
        t.trade_id.as_deref(),
        Some("12345"),
        "aggTrade id comes from `a`"
    );
}

#[test]
fn alpaca_and_polygon_frames_pick_out_trades() {
    let ticks = n()
        .normalize_trade_frame(Exchange::Alpaca, &fixture("alpaca_messages.json"))
        .unwrap();
    assert_eq!(ticks.len(), 1, "the quote message is skipped");
    assert_eq!(
        (ticks[0].symbol.as_str(), ticks[0].price),
        ("AAPL", dec!(126.55))
    );
    assert_eq!(ticks[0].exchange_ts_ms, Some(1_614_009_104_208));

    let ticks = n()
        .normalize_trade_frame(Exchange::Polygon, &fixture("polygon_events.json"))
        .unwrap();
    assert_eq!(ticks.len(), 1);
    assert_eq!(
        (ticks[0].symbol.as_str(), ticks[0].price),
        ("MSFT", dec!(114.125))
    );
    // Polygon's WebSocket sends ms; nanosecond values still convert.
    assert_eq!(ticks[0].exchange_ts_ms, Some(1_536_036_818_784));
}

#[test]
fn control_and_other_channel_frames_give_no_ticks() {
    let frames = [
        (
            Exchange::Okx,
            json!({"event": "subscribe", "arg": {"channel": "trades", "instId": "BTC-USDT"}, "connId": "a4d3ae55"}),
        ),
        (
            Exchange::Bybit,
            json!({"success": true, "ret_msg": "", "op": "subscribe", "conn_id": "x"}),
        ),
        (Exchange::Kraken, json!({"channel": "heartbeat"})),
        (
            Exchange::Kraken,
            json!({"method": "subscribe", "result": {"channel": "trade"}, "success": true}),
        ),
        (
            Exchange::Coinbase,
            json!({"type": "subscriptions", "channels": []}),
        ),
        (
            Exchange::Coinbase,
            json!({"channel": "heartbeats", "events": []}),
        ),
        (Exchange::Binance, json!({"result": null, "id": 1})),
        (
            Exchange::Alpaca,
            json!([{"T": "success", "msg": "authenticated"}]),
        ),
        (
            Exchange::Polygon,
            json!([{"ev": "status", "status": "connected"}]),
        ),
    ];
    for (ex, frame) in frames {
        assert!(
            n().normalize_trade_frame(ex, &frame).unwrap().is_empty(),
            "{ex}: {frame}"
        );
        assert!(
            n().normalize_quote_frame(ex, &frame).unwrap().is_empty(),
            "{ex}: {frame}"
        );
    }
    // Trade frames are not quotes and quote frames are not trades.
    assert!(n()
        .normalize_quote_frame(Exchange::Okx, &fixture("okx_trades.json"))
        .unwrap()
        .is_empty());
    assert!(n()
        .normalize_trade_frame(Exchange::Okx, &fixture("okx_tickers.json"))
        .unwrap()
        .is_empty());
    assert!(n()
        .normalize_trade_frame(Exchange::Binance, &fixture("binance_book_ticker.json"))
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------- quotes

fn one_quote(exchange: Exchange, name: &str) -> NormalizedQuote {
    let quotes = n().normalize_quote_frame(exchange, &fixture(name)).unwrap();
    assert_eq!(quotes.len(), 1, "{name}");
    quotes.into_iter().next().unwrap()
}

#[test]
fn binance_book_ticker_quote() {
    let q = one_quote(Exchange::Binance, "binance_book_ticker.json");
    assert_eq!(q.symbol, "BNBUSDT");
    assert_eq!(
        (q.bid_price, q.bid_qty),
        (dec!(25.35190000), Some(dec!(31.21000000)))
    );
    assert_eq!(
        (q.ask_price, q.ask_qty),
        (dec!(25.36520000), Some(dec!(40.66000000)))
    );
    assert_eq!(q.exchange_ts_ms, None);
}

#[test]
fn coinbase_exchange_and_advanced_trade_ticker_quotes() {
    let q = one_quote(Exchange::Coinbase, "coinbase_exchange_ticker.json");
    assert_eq!(
        (q.symbol.as_str(), q.bid_price, q.ask_price),
        ("ETH-USD", dec!(1285.04), dec!(1285.27))
    );
    assert_eq!(
        (q.bid_qty, q.ask_qty),
        (Some(dec!(0.46688654)), Some(dec!(1.56637040)))
    );
    assert_eq!(q.exchange_ts_ms, Some(1_666_222_102_061));

    let q = one_quote(Exchange::Coinbase, "coinbase_advanced_ticker.json");
    assert_eq!(
        (q.symbol.as_str(), q.bid_price, q.ask_price),
        ("BTC-USD", dec!(21931.98), dec!(21933.98))
    );
    assert_eq!(
        q.bid_qty,
        Some(dec!(8000.21000000)),
        "best_bid_quantity spelling"
    );
    assert_eq!(
        q.exchange_ts_ms,
        Some(1_675_974_637_167),
        "time from the envelope"
    );
}

#[test]
fn alpaca_and_polygon_quotes() {
    let q = one_quote(Exchange::Alpaca, "alpaca_messages.json");
    assert_eq!(
        (q.symbol.as_str(), q.bid_price, q.ask_price),
        ("AMD", dec!(87.66), dec!(87.68))
    );
    assert_eq!((q.bid_qty, q.ask_qty), (Some(dec!(1)), Some(dec!(4))));
    assert_eq!(q.exchange_ts_ms, Some(1_614_009_105_335));

    let quotes = n()
        .normalize_quote_frame(Exchange::Polygon, &fixture("polygon_events.json"))
        .unwrap();
    assert_eq!(quotes.len(), 2, "stocks Q and crypto XQ");
    assert_eq!(
        (quotes[0].symbol.as_str(), quotes[0].ask_price),
        ("MSFT", dec!(114.128))
    );
    assert_eq!(quotes[0].exchange_ts_ms, Some(1_536_036_818_784));
    assert_eq!(
        (quotes[1].symbol.as_str(), quotes[1].bid_price),
        ("BTC-USD", dec!(33052.79))
    );
}

#[test]
fn kraken_okx_bybit_quotes() {
    let q = one_quote(Exchange::Kraken, "kraken_v2_ticker.json");
    assert_eq!(
        (q.symbol.as_str(), q.bid_price, q.ask_price),
        ("ALGO/USD", dec!(0.10025), dec!(0.10035))
    );
    assert_eq!(q.bid_qty, Some(dec!(740)));

    let q = one_quote(Exchange::Okx, "okx_tickers.json");
    assert_eq!(
        (q.symbol.as_str(), q.bid_price, q.ask_price),
        ("BTC-USDT", dec!(8888.88), dec!(9999.99))
    );
    assert_eq!((q.bid_qty, q.ask_qty), (Some(dec!(5)), Some(dec!(11))));
    assert_eq!(q.exchange_ts_ms, Some(1_597_026_383_085));

    let q = one_quote(Exchange::Bybit, "bybit_tickers.json");
    assert_eq!(
        (q.symbol.as_str(), q.bid_price, q.ask_price),
        ("BTCUSDT", dec!(17215.50), dec!(17216.00))
    );
    assert_eq!(q.ask_qty, Some(dec!(83.020)));
    assert_eq!(
        q.exchange_ts_ms,
        Some(1_673_272_861_686),
        "time from the envelope"
    );
}

#[test]
fn bybit_ticker_delta_without_both_sides_is_skipped() {
    let delta = json!({"topic": "tickers.BTCUSDT", "type": "delta", "ts": 1, "data": {"symbol": "BTCUSDT", "lastPrice": "17216.00"}});
    assert!(n()
        .normalize_quote_frame(Exchange::Bybit, &delta)
        .unwrap()
        .is_empty());
}

#[test]
fn quote_validation() {
    let no_ask = json!({"b": "1", "B": "1"});
    assert!(matches!(
        n().normalize_quote(raw(Exchange::Binance, "X", no_ask)),
        Err(StreamError::ParseError { .. })
    ));
    let zero_bid = json!({"bidPx": "0", "askPx": "1"});
    assert!(matches!(
        n().normalize_quote(raw(Exchange::Okx, "X", zero_bid)),
        Err(StreamError::InvalidTick { .. })
    ));
    let neg_size = json!({"bid": 1, "ask": 2, "bid_qty": -1});
    assert!(matches!(
        n().normalize_quote(raw(Exchange::Kraken, "X", neg_size)),
        Err(StreamError::InvalidTick { .. })
    ));
    // Sizes are optional.
    let bare = json!({"bp": 10, "ap": 10.5});
    let q = n()
        .normalize_quote(raw(Exchange::Alpaca, "X", bare))
        .unwrap();
    assert_eq!((q.bid_qty, q.ask_qty, q.received_at_ms), (None, None, 42));
}

#[test]
fn quote_helpers_and_quote_event_conversion() {
    let q = one_quote(Exchange::Okx, "okx_tickers.json");
    assert_eq!(q.spread(), dec!(1111.11));
    assert_eq!(q.mid_price(), dec!(9444.435));
    assert!(!q.is_crossed() && !q.is_locked());
    assert!(q.spread_bps().unwrap() > dec!(1176) && q.spread_bps().unwrap() < dec!(1177));

    let e = QuoteEvent::from(&q);
    assert_eq!(
        (e.symbol.as_str(), e.exchange.as_str()),
        ("BTC-USDT", "OKX")
    );
    assert_eq!(
        (e.bid, e.ask, e.bid_size, e.ask_size),
        (8888.88, 9999.99, 5.0, 11.0)
    );
    assert_eq!(e.timestamp_ns, 1_597_026_383_085_000_000);

    let crossed = NormalizedQuote {
        bid_price: dec!(2),
        ask_price: dec!(1),
        ..q
    };
    assert!(crossed.is_crossed());
}

#[test]
fn quote_serde_round_trip() {
    let q = one_quote(Exchange::Bybit, "bybit_tickers.json");
    let back: NormalizedQuote = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
    assert_eq!(back, q);
}

// ---------------------------------------------------------------- Exchange enum

#[test]
fn new_exchange_variants_parse_display_and_list() {
    for (s, ex, shown) in [
        ("kraken", Exchange::Kraken, "Kraken"),
        ("OKX", Exchange::Okx, "OKX"),
        ("Bybit", Exchange::Bybit, "Bybit"),
    ] {
        assert_eq!(s.parse::<Exchange>().unwrap(), ex);
        assert_eq!(ex.to_string(), shown);
        assert_eq!(
            shown.parse::<Exchange>().unwrap(),
            ex,
            "Display round-trips through FromStr"
        );
        assert!(ex.is_crypto());
        assert!(Exchange::all().contains(&ex));
    }
    assert_eq!(Exchange::all().len(), 7);
}
