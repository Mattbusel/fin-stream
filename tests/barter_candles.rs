//! barter-data `PublicTrade` events in, fin-stream 1-minute bars out, offline.
//!
//! `fixtures/barter_public_trades.ndjson` holds `MarketEvent<MarketDataInstrument,
//! PublicTrade>` values in barter-data 0.13's serde format (what its streams emit
//! for Coinbase BTC-USD). The prices and times are hand-picked so the bars below
//! can be checked by hand: four trades in 15:00, one late trade for 15:00 that
//! arrives during 15:01, two trades in 15:01, nothing in 15:02, one trade in 15:03.
#![cfg(feature = "barter")]

use barter_data::event::MarketEvent;
use barter_data::subscription::trade::PublicTrade;
use barter_instrument::instrument::market_data::MarketDataInstrument;
use fin_stream::tick::TradeSide;
use fin_stream::{Exchange, NormalizedTick, OhlcvAggregator, OhlcvBar, Timeframe};
use rust_decimal_macros::dec;

const FIXTURE: &str = include_str!("fixtures/barter_public_trades.ndjson");
/// 2026-01-05 15:00:00 UTC.
const T15_00: u64 = 1_767_625_200_000;
const MINUTE: u64 = 60_000;

type Error = Box<dyn std::error::Error>;

fn events() -> Result<Vec<MarketEvent<MarketDataInstrument, PublicTrade>>, Error> {
    FIXTURE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

/// Convert every fixture trade and feed it to `agg`, collecting the bars it closes.
fn run(mut agg: OhlcvAggregator) -> Result<(Vec<OhlcvBar>, OhlcvAggregator), Error> {
    let mut bars = Vec::new();
    for event in events()? {
        bars.extend(agg.feed(&NormalizedTick::try_from(&event)?)?);
    }
    Ok((bars, agg))
}

#[test]
fn fixture_trades_convert_exactly() {
    let ticks: Vec<NormalizedTick> = events().unwrap().iter().map(|e| NormalizedTick::try_from(e).unwrap()).collect();
    assert_eq!(ticks.len(), 8);
    let first = &ticks[0];
    assert_eq!(first.exchange, Exchange::Coinbase);
    assert_eq!(first.symbol, "btc_usd_spot");
    assert_eq!(first.price, dec!(97000.1));
    assert_eq!(first.quantity, dec!(0.01));
    assert_eq!(first.side, Some(TradeSide::Buy));
    assert_eq!(first.trade_id.as_deref(), Some("912000001"));
    assert_eq!(first.exchange_ts_ms, Some(T15_00 + 1_250));
    assert_eq!(first.received_at_ms, T15_00 + 1_262);
    assert_eq!(ticks[1].price, dec!(97010.55));
    assert_eq!(ticks[1].side, Some(TradeSide::Sell));
}

#[test]
fn one_minute_bars_from_barter_trades() {
    let agg = OhlcvAggregator::new("btc_usd_spot", Timeframe::Minutes(1)).unwrap();
    let (bars, mut agg) = run(agg).unwrap();

    // 15:00 closes when the 15:01:00.000 trade arrives; 15:01 closes at 15:03:05.
    assert_eq!(bars.len(), 2);
    let b0 = &bars[0];
    assert_eq!(b0.bar_start_ms, T15_00);
    assert_eq!(
        (b0.open, b0.high, b0.low, b0.close),
        (dec!(97000.1), dec!(97010.55), dec!(96995.0), dec!(97001.2))
    );
    assert_eq!(b0.volume, dec!(0.40));
    assert_eq!(b0.trade_count, 4);
    // (97000.1*0.01 + 97010.55*0.25 + 96995*0.1 + 97001.2*0.04) / 0.4
    assert_eq!(b0.vwap, Some(dec!(97005.46625)));
    assert!(b0.is_complete && !b0.is_gap_fill);

    let b1 = &bars[1];
    assert_eq!(b1.bar_start_ms, T15_00 + MINUTE);
    assert_eq!(
        (b1.open, b1.high, b1.low, b1.close),
        (dec!(97002.0), dec!(97050.75), dec!(97002.0), dec!(97050.75))
    );
    assert_eq!(b1.volume, dec!(0.8));
    assert_eq!(b1.trade_count, 2);
    assert_eq!(b1.vwap, Some(dec!(97020.28125)));

    // The 15:00:59 trade that arrived during 15:01 is dropped, not booked into 15:01.
    assert_eq!(agg.late_tick_count(), 1);

    let last = agg.flush().expect("15:03 bar is open");
    assert_eq!(last.bar_start_ms, T15_00 + 3 * MINUTE);
    assert_eq!((last.open, last.close, last.volume, last.trade_count), (dec!(97040.0), dec!(97040.0), dec!(0.2), 1));
}

#[test]
fn empty_minutes_can_be_filled() {
    let agg = OhlcvAggregator::new("btc_usd_spot", Timeframe::Minutes(1)).unwrap().with_emit_empty_bars(true);
    let (bars, _) = run(agg).unwrap();
    let starts: Vec<u64> = bars.iter().map(|b| b.bar_start_ms).collect();
    assert_eq!(starts, [T15_00, T15_00 + MINUTE, T15_00 + 2 * MINUTE]);
    let gap = &bars[2];
    assert!(gap.is_gap_fill);
    assert_eq!((gap.open, gap.close, gap.volume, gap.trade_count), (dec!(97050.75), dec!(97050.75), dec!(0), 0));
}

#[test]
fn venues_without_an_exchange_variant_are_rejected() {
    let mut e = events().unwrap().remove(0);
    e.exchange = barter_instrument::exchange::ExchangeId::Okx;
    assert!(matches!(NormalizedTick::try_from(&e), Err(fin_stream::StreamError::UnknownExchange(_))));
}
