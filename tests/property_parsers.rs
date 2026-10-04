//! Untrusted-input properties: the exchange JSON normalizers, the generic JSON
//! event adapter and the FIX parser must return errors, never panic, whatever
//! bytes arrive from the network.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fin_stream::fix::FixParser;
use fin_stream::protocol::JsonStreamAdapter;
use fin_stream::tick::{Exchange, RawTick, TickNormalizer};
use proptest::prelude::*;
use serde_json::{json, Value};

/// Arbitrary JSON, weighted toward the shapes and keys exchanges actually send.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        "[-+0-9.eE]{0,30}".prop_map(Value::from),
        ".{0,20}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(
                prop_oneof![
                    Just("p".to_string()), Just("q".to_string()), Just("m".to_string()),
                    Just("T".to_string()), Just("t".to_string()), Just("s".to_string()),
                    Just("price".to_string()), Just("size".to_string()), Just("side".to_string()),
                    Just("time".to_string()), Just("trade_id".to_string()), Just("product_id".to_string()),
                    Just("x".to_string()), Just("i".to_string()), Just("bid".to_string()), Just("ask".to_string()),
                    Just("open".to_string()), Just("high".to_string()), Just("low".to_string()), Just("close".to_string()),
                    Just("bids".to_string()), Just("asks".to_string()), Just("type".to_string()), Just("status".to_string()),
                    "[a-z]{1,6}",
                ],
                inner,
                0..10,
            )
            .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn exchange() -> impl Strategy<Value = Exchange> {
    prop_oneof![Just(Exchange::Binance), Just(Exchange::Coinbase), Just(Exchange::Alpaca), Just(Exchange::Polygon)]
}

/// A FIX 4.2 frame with a correct BodyLength and checksum around arbitrary fields,
/// so the parser gets past framing and has to cope with the field contents.
fn fix_frame(fields: &[(u32, String)]) -> Vec<u8> {
    let mut body = String::new();
    for (tag, val) in fields {
        body.push_str(&format!("{tag}={val}\x01"));
    }
    let mut msg = format!("8=FIX.4.2\x019={}\x01{body}", body.len());
    let sum: u32 = msg.bytes().map(u32::from).sum::<u32>() % 256;
    msg.push_str(&format!("10={sum:03}\x01"));
    msg.into_bytes()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    #[test]
    fn normalizer_never_panics(ex in exchange(), payload in json_value(), symbol in ".{0,12}", ts in any::<u64>()) {
        let raw = RawTick { exchange: ex, symbol, payload, received_at_ms: ts };
        let _ = TickNormalizer::new().normalize(raw);
    }

    #[test]
    fn normalizer_never_panics_on_trade_shaped_payloads(
        ex in exchange(),
        p in json_value(), q in json_value(), m in json_value(), t in json_value()
    ) {
        let payload = json!({"p": p, "q": q, "m": m, "T": t, "price": p, "size": q, "side": m, "time": t, "s": q, "t": t});
        let raw = RawTick { exchange: ex, symbol: "BTC-USD".into(), payload, received_at_ms: 1 };
        if let Ok(tick) = TickNormalizer::new().normalize(raw) {
            // Whatever got through must be a usable trade.
            prop_assert!(tick.price > rust_decimal::Decimal::ZERO);
            prop_assert!(tick.quantity >= rust_decimal::Decimal::ZERO);
        }
    }

    #[test]
    fn json_adapter_never_panics(text in ".{0,200}", v in json_value()) {
        let a = JsonStreamAdapter::new("ws://test", "test");
        let _ = a.parse_event(&text);
        let _ = a.parse_event(&v.to_string());
    }

    #[test]
    fn fix_parser_never_panics_on_bytes(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let _ = FixParser::new().parse(&bytes);
    }

    #[test]
    fn fix_parser_never_panics_on_framed_fields(
        fields in prop::collection::vec((prop_oneof![Just(35u32), Just(49), Just(56), Just(34), Just(52), Just(55), Just(270), Just(271), Just(269), 1u32..1000], "[ -~]{0,16}"), 0..20)
    ) {
        let parser = FixParser::new();
        if let Ok(msg) = parser.parse(&fix_frame(&fields)) {
            // A parsed message must serialize and parse back to the same type.
            let again = parser.parse(&parser.serialize(&msg));
            prop_assert!(again.is_ok(), "re-parse failed: {again:?}");
        }
    }
}

#[test]
fn fix_frame_helper_builds_frames_the_parser_accepts() {
    // Guards the property above: its frames must be valid enough to reach field parsing.
    let fields = vec![
        (35, "W".to_string()),
        (49, "SENDER".to_string()),
        (56, "TARGET".to_string()),
        (34, "1".to_string()),
        (55, "BTC-USD".to_string()),
        (270, "64250.5".to_string()),
        (271, "0.25".to_string()),
    ];
    let msg = FixParser::new().parse(&fix_frame(&fields)).unwrap();
    assert_eq!(msg.msg_type, "W");
    assert_eq!(msg.get(270).unwrap(), "64250.5");
}
