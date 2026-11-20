//! Hand fin-stream ticks to other Rust trading crates.
//!
//! | feature | target | conversion |
//! |---|---|---|
//! | `fin-primitives` (default) | [`fin_primitives::tick::Tick`] | `TryFrom<&NormalizedTick>`, then use fin-primitives' bar aggregator, 700+ indicators, order book and risk tools on live data |
//! | `barter` | `barter_data::subscription::trade::PublicTrade` | `TryFrom<&NormalizedTick>`, so strategies written against barter's trade type can consume fin-stream's Alpaca and Polygon (US equity) feeds, which barter-data does not cover |
//! | `barter` | `NormalizedTick` from barter-data `MarketEvent<_, PublicTrade>` | `TryFrom<&MarketEvent<..>>` and `NormalizedTick::from_barter_trade`, so trades from a barter-data `MarketStream` can feed [`OhlcvAggregator`](crate::ohlcv::OhlcvAggregator) and the rest of fin-stream |
//!
//! The conversions to fin-primitives and barter fail with
//! an [`InvalidInput`](crate::error::StreamError::InvalidInput) error instead of guessing when the tick has no
//! aggressor side, because both targets require one. Use
//! [`NormalizedTick::with_side_if_unknown`] first if you want a default.
//!
//! # Candles from a barter-data trade stream
//!
//! barter-data streams normalised public trades from many crypto venues but does
//! not build candles from them. With the `barter` feature each trade converts to
//! a [`NormalizedTick`] (prices become exact `Decimal`s) and goes straight into
//! fin-stream's [`OhlcvAggregator`](crate::ohlcv::OhlcvAggregator):
//!
//! ```no_run
//! # #[cfg(feature = "barter")]
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use barter_data::exchange::coinbase::Coinbase;
//! use barter_data::streams::reconnect::{stream::ReconnectingStream, Event};
//! use barter_data::streams::Streams;
//! use barter_data::subscription::trade::PublicTrades;
//! use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;
//! use fin_stream::{NormalizedTick, OhlcvAggregator, Timeframe};
//! use futures_util::StreamExt;
//!
//! let streams = Streams::<PublicTrades>::builder()
//!     .subscribe([(Coinbase, "btc", "usd", MarketDataInstrumentKind::Spot, PublicTrades)])
//!     .init()
//!     .await?;
//! let mut trades = streams.select_all().with_error_handler(|e| eprintln!("{e:?}"));
//!
//! // The symbol is the barter instrument's Display form: base_quote_kind.
//! let mut bars = OhlcvAggregator::new("btc_usd_spot", Timeframe::Minutes(1))?;
//! while let Some(event) = trades.next().await {
//!     // Event::Reconnecting means barter-data is reconnecting; nothing to aggregate.
//!     let Event::Item(trade) = event else { continue };
//!     for bar in bars.feed(&NormalizedTick::try_from(&trade)?)? {
//!         println!("{} O {} H {} L {} C {} V {}", bar.bar_start_ms, bar.open, bar.high, bar.low, bar.close, bar.volume);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! `examples/barter_candles.rs` is the runnable version
//! (`cargo run --example barter_candles --features barter`, needs network).
//!
//! Only venues that have an [`Exchange`](crate::tick::Exchange) variant convert:
//! barter's `BinanceSpot`, `BinanceFuturesUsd` and `BinanceFuturesCoin` become
//! `Exchange::Binance`, `Coinbase` becomes `Exchange::Coinbase`, `Kraken` and `Okx`
//! become `Exchange::Kraken` and `Exchange::Okx`, and `BybitSpot` and
//! `BybitPerpetualsUsd` become `Exchange::Bybit`. Any other
//! venue is rejected with [`UnknownExchange`](crate::error::StreamError::UnknownExchange) rather than mislabelled.
//! The symbol keeps barter's instrument kind (`btc_usdt_spot` and
//! `btc_usdt_perpetual` stay apart), so spot and futures trades on Binance do not
//! mix in one aggregator.
//!
//! The `barter` feature needs Rust 1.88 or later (barter-instrument 0.3 uses
//! edition 2024 and let chains); the rest of the crate keeps the MSRV in `Cargo.toml`.

#[cfg(any(feature = "fin-primitives", feature = "barter"))]
use crate::error::StreamError;
use crate::tick::{NormalizedTick, TradeSide};

impl NormalizedTick {
    /// A copy of this tick with `side` filled in when the exchange did not report one.
    #[must_use]
    pub fn with_side_if_unknown(&self, side: TradeSide) -> NormalizedTick {
        let mut t = self.clone();
        t.side.get_or_insert(side);
        t
    }

    /// Exchange timestamp when present, otherwise the local receive time (ms).
    #[must_use]
    pub fn best_timestamp_ms(&self) -> u64 {
        self.exchange_ts_ms.unwrap_or(self.received_at_ms)
    }
}

#[cfg(any(feature = "fin-primitives", feature = "barter"))]
fn missing_side(t: &NormalizedTick) -> StreamError {
    StreamError::InvalidInput(format!(
        "{} tick for {} has no aggressor side; call with_side_if_unknown first",
        t.exchange, t.symbol
    ))
}

#[cfg(feature = "fin-primitives")]
#[cfg_attr(docsrs, doc(cfg(feature = "fin-primitives")))]
impl TryFrom<&NormalizedTick> for fin_primitives::tick::Tick {
    type Error = StreamError;

    /// Buyer-initiated trades become [`Side::Bid`](fin_primitives::types::Side::Bid)
    /// aggressors and seller-initiated ones `Ask`. The timestamp is the exchange time
    /// when present, else the receive time, in nanoseconds.
    ///
    /// # Errors
    /// [`StreamError::InvalidInput`] for a missing side or a timestamp past year 2262;
    /// [`StreamError::FinPrimitives`] if fin-primitives rejects the symbol, a zero or
    /// negative price, or a negative quantity.
    fn try_from(t: &NormalizedTick) -> Result<Self, Self::Error> {
        use fin_primitives::types::{NanoTimestamp, Price, Quantity, Side, Symbol};
        let side = match t.side.ok_or_else(|| missing_side(t))? {
            TradeSide::Buy => Side::Bid,
            TradeSide::Sell => Side::Ask,
        };
        let nanos = i64::try_from(t.best_timestamp_ms())
            .ok()
            .and_then(|ms| ms.checked_mul(1_000_000))
            .ok_or_else(|| {
                StreamError::InvalidInput(format!(
                    "timestamp {} ms is out of range",
                    t.best_timestamp_ms()
                ))
            })?;
        Ok(fin_primitives::tick::Tick::new(
            Symbol::new(&t.symbol)?,
            Price::new(t.price)?,
            Quantity::new(t.quantity)?,
            side,
            NanoTimestamp::new(nanos),
        ))
    }
}

#[cfg(feature = "barter")]
#[cfg_attr(docsrs, doc(cfg(feature = "barter")))]
impl TryFrom<&NormalizedTick> for barter_data::subscription::trade::PublicTrade {
    type Error = StreamError;

    /// Price and amount become `f64` (barter's representation); the trade id is the
    /// exchange's id, or an empty string when the venue does not send one.
    ///
    /// # Errors
    /// [`StreamError::InvalidInput`] for a missing side or a price that does not fit `f64`.
    fn try_from(t: &NormalizedTick) -> Result<Self, Self::Error> {
        use rust_decimal::prelude::ToPrimitive;
        let side = match t.side.ok_or_else(|| missing_side(t))? {
            TradeSide::Buy => barter_instrument::Side::Buy,
            TradeSide::Sell => barter_instrument::Side::Sell,
        };
        let f = |d: rust_decimal::Decimal| {
            d.to_f64()
                .ok_or_else(|| StreamError::InvalidInput(format!("{d} does not fit f64")))
        };
        Ok(Self {
            id: t.trade_id.clone().unwrap_or_default(),
            price: f(t.price)?,
            amount: f(t.quantity)?,
            side,
        })
    }
}

#[cfg(feature = "barter")]
fn exchange_from_barter(
    id: barter_instrument::exchange::ExchangeId,
) -> Result<crate::tick::Exchange, StreamError> {
    use crate::tick::Exchange;
    use barter_instrument::exchange::ExchangeId as X;
    match id {
        X::BinanceSpot | X::BinanceFuturesUsd | X::BinanceFuturesCoin => Ok(Exchange::Binance),
        X::Coinbase => Ok(Exchange::Coinbase),
        X::Kraken => Ok(Exchange::Kraken),
        X::Okx => Ok(Exchange::Okx),
        X::BybitSpot | X::BybitPerpetualsUsd => Ok(Exchange::Bybit),
        other => Err(StreamError::UnknownExchange(format!(
            "barter-data venue {} has no fin_stream::Exchange variant",
            other.as_str()
        ))),
    }
}

#[cfg(feature = "barter")]
fn barter_ms(t: chrono::DateTime<chrono::Utc>, what: &str) -> Result<u64, StreamError> {
    u64::try_from(t.timestamp_millis())
        .map_err(|_| StreamError::InvalidInput(format!("{what} {t} is before the Unix epoch")))
}

#[cfg(feature = "barter")]
#[cfg_attr(docsrs, doc(cfg(feature = "barter")))]
impl NormalizedTick {
    /// Build a tick from a barter-data public trade event, using `symbol` as the
    /// tick's symbol.
    ///
    /// Use this when the event's instrument key does not implement `Display`, or
    /// when you want your own symbol (for example `"BTC-USD"`) instead of barter's
    /// `btc_usd_spot`. With a `Display` key, `NormalizedTick::try_from(&event)` does
    /// the same and takes the symbol from the key.
    ///
    /// The event's `kind` may be an owned `PublicTrade` or a reference to one, so the
    /// `MarketEvent<&I, &PublicTrade>` returned by barter-data's
    /// `MarketEvent<I, DataKind>::as_public_trade` converts as well.
    ///
    /// | barter field | tick field |
    /// |---|---|
    /// | `exchange` | `exchange` (see the [module docs](crate::interop) for which venues map) |
    /// | `kind.price`, `kind.amount` (`f64`) | `price`, `quantity` (`Decimal` via `Decimal::from_f64`, so `0.1` stays `0.1`) |
    /// | `kind.side` | `side` (always `Some`) |
    /// | `kind.id` | `trade_id` (`None` when empty) |
    /// | `time_exchange` | `exchange_ts_ms` (what [`OhlcvAggregator`](crate::ohlcv::OhlcvAggregator) uses for bar boundaries) |
    /// | `time_received` | `received_at_ms` |
    ///
    /// # Errors
    /// - [`StreamError::UnknownExchange`] for a venue outside [`Exchange`](crate::tick::Exchange).
    /// - [`StreamError::InvalidInput`] for a NaN, infinite, zero or negative price, a
    ///   NaN, infinite or negative amount, a value outside `Decimal`'s range, or a
    ///   timestamp before 1970.
    pub fn from_barter_trade<I, T>(
        event: &barter_data::event::MarketEvent<I, T>,
        symbol: impl Into<String>,
    ) -> Result<Self, StreamError>
    where
        T: std::borrow::Borrow<barter_data::subscription::trade::PublicTrade>,
    {
        use rust_decimal::prelude::FromPrimitive;
        use rust_decimal::Decimal;
        let trade = event.kind.borrow();
        let exchange = exchange_from_barter(event.exchange)?;
        let decimal = |v: f64, what: &str| {
            Decimal::from_f64(v).ok_or_else(|| {
                StreamError::InvalidInput(format!(
                    "barter trade {what} {v} is not a finite decimal"
                ))
            })
        };
        let price = decimal(trade.price, "price")?;
        if price <= Decimal::ZERO {
            return Err(StreamError::InvalidInput(format!(
                "barter trade price {price} is not positive"
            )));
        }
        let quantity = decimal(trade.amount, "amount")?;
        if quantity < Decimal::ZERO {
            return Err(StreamError::InvalidInput(format!(
                "barter trade amount {quantity} is negative"
            )));
        }
        Ok(NormalizedTick {
            exchange,
            symbol: symbol.into(),
            price,
            quantity,
            side: Some(match trade.side {
                barter_instrument::Side::Buy => TradeSide::Buy,
                barter_instrument::Side::Sell => TradeSide::Sell,
            }),
            trade_id: (!trade.id.is_empty()).then(|| trade.id.clone()),
            exchange_ts_ms: Some(barter_ms(event.time_exchange, "time_exchange")?),
            received_at_ms: barter_ms(event.time_received, "time_received")?,
        })
    }
}

#[cfg(feature = "barter")]
#[cfg_attr(docsrs, doc(cfg(feature = "barter")))]
impl<I, T> TryFrom<&barter_data::event::MarketEvent<I, T>> for NormalizedTick
where
    I: std::fmt::Display,
    T: std::borrow::Borrow<barter_data::subscription::trade::PublicTrade>,
{
    type Error = StreamError;

    /// [`NormalizedTick::from_barter_trade`] with the instrument key's `Display`
    /// form as the symbol (`btc_usdt_spot` for barter's `MarketDataInstrument`).
    ///
    /// # Errors
    /// As for [`NormalizedTick::from_barter_trade`].
    fn try_from(event: &barter_data::event::MarketEvent<I, T>) -> Result<Self, Self::Error> {
        Self::from_barter_trade(event, event.instrument.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::Exchange;
    use rust_decimal_macros::dec;

    fn tick(side: Option<TradeSide>) -> NormalizedTick {
        NormalizedTick {
            exchange: Exchange::Alpaca,
            symbol: "AAPL".into(),
            price: dec!(227.53),
            quantity: dec!(100),
            side,
            trade_id: Some("52983525029461".into()),
            exchange_ts_ms: Some(1_767_625_200_123),
            received_at_ms: 1_767_625_200_130,
        }
    }

    #[test]
    fn side_default_only_fills_missing_sides() {
        assert_eq!(
            tick(None).with_side_if_unknown(TradeSide::Sell).side,
            Some(TradeSide::Sell)
        );
        assert_eq!(
            tick(Some(TradeSide::Buy))
                .with_side_if_unknown(TradeSide::Sell)
                .side,
            Some(TradeSide::Buy)
        );
    }

    #[cfg(feature = "fin-primitives")]
    #[test]
    fn converts_to_fin_primitives_tick_and_feeds_its_bar_aggregator() {
        use fin_primitives::ohlcv::{OhlcvAggregator, Timeframe};
        use fin_primitives::types::{Side, Symbol};
        let t = fin_primitives::tick::Tick::try_from(&tick(Some(TradeSide::Buy))).unwrap();
        assert_eq!(t.side, Side::Bid);
        assert_eq!(t.price.value(), dec!(227.53));
        assert_eq!(t.timestamp.nanos(), 1_767_625_200_123_000_000);
        let mut agg =
            OhlcvAggregator::new(Symbol::new("AAPL").unwrap(), Timeframe::Minutes(1)).unwrap();
        assert!(agg.push_tick(&t).unwrap().is_empty());
        assert_eq!(agg.current_bar().unwrap().close.value(), dec!(227.53));

        assert!(matches!(
            fin_primitives::tick::Tick::try_from(&tick(None)),
            Err(StreamError::InvalidInput(_))
        ));
        let mut zero = tick(Some(TradeSide::Sell));
        zero.price = dec!(0);
        assert!(matches!(
            fin_primitives::tick::Tick::try_from(&zero),
            Err(StreamError::FinPrimitives(_))
        ));
    }

    #[cfg(feature = "barter")]
    #[test]
    fn converts_to_barter_public_trade() {
        use barter_data::subscription::trade::PublicTrade;
        let p = PublicTrade::try_from(&tick(Some(TradeSide::Sell))).unwrap();
        assert_eq!(p.id, "52983525029461");
        assert_eq!(p.price, 227.53);
        assert_eq!(p.amount, 100.0);
        assert_eq!(p.side, barter_instrument::Side::Sell);
        assert!(PublicTrade::try_from(&tick(None)).is_err());
    }

    #[cfg(feature = "barter")]
    fn barter_event(
        exchange: barter_instrument::exchange::ExchangeId,
        price: f64,
        amount: f64,
    ) -> barter_data::event::MarketEvent<
        barter_instrument::instrument::market_data::MarketDataInstrument,
        barter_data::subscription::trade::PublicTrade,
    > {
        use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;
        use chrono::TimeZone;
        barter_data::event::MarketEvent {
            time_exchange: chrono::Utc.timestamp_millis_opt(1_767_625_200_123).unwrap(),
            time_received: chrono::Utc.timestamp_millis_opt(1_767_625_200_130).unwrap(),
            exchange,
            instrument: ("btc", "usdt", MarketDataInstrumentKind::Spot).into(),
            kind: barter_data::subscription::trade::PublicTrade {
                id: "4567".into(),
                price,
                amount,
                side: barter_instrument::Side::Buy,
            },
        }
    }

    #[cfg(feature = "barter")]
    #[test]
    fn converts_from_barter_public_trade_event() {
        use barter_instrument::exchange::ExchangeId;
        let e = barter_event(ExchangeId::BinanceSpot, 64_123.45, 0.1);
        let t = NormalizedTick::try_from(&e).unwrap();
        assert_eq!(t.exchange, Exchange::Binance);
        assert_eq!(t.symbol, "btc_usdt_spot");
        // The shortest decimal that round-trips, not the f64's binary expansion.
        assert_eq!(t.price, dec!(64123.45));
        assert_eq!(t.quantity, dec!(0.1));
        assert_eq!(t.side, Some(TradeSide::Buy));
        assert_eq!(t.trade_id.as_deref(), Some("4567"));
        assert_eq!(t.exchange_ts_ms, Some(1_767_625_200_123));
        assert_eq!(t.received_at_ms, 1_767_625_200_130);

        let mine = NormalizedTick::from_barter_trade(&e, "BTC-USDT").unwrap();
        assert_eq!(mine.symbol, "BTC-USDT");

        // And back to barter's type without loss.
        let back = barter_data::subscription::trade::PublicTrade::try_from(&t).unwrap();
        assert_eq!(back, e.kind);

        let mut no_id = barter_event(ExchangeId::Coinbase, 227.53, 1.0);
        no_id.kind.id.clear();
        let t = NormalizedTick::try_from(&no_id).unwrap();
        assert_eq!(
            (t.exchange, t.price, t.trade_id),
            (Exchange::Coinbase, dec!(227.53), None)
        );
    }

    #[cfg(feature = "barter")]
    #[test]
    fn converts_barter_data_kind_events_by_reference() {
        use barter_data::event::DataKind;
        use barter_instrument::exchange::ExchangeId;
        let e =
            barter_event(ExchangeId::BinanceFuturesUsd, 64_000.5, 0.25).map_kind(DataKind::Trade);
        let trade = e.as_public_trade().unwrap();
        let t = NormalizedTick::try_from(&trade).unwrap();
        assert_eq!(
            (t.exchange, t.price, t.quantity),
            (Exchange::Binance, dec!(64000.5), dec!(0.25))
        );
    }

    #[cfg(feature = "barter")]
    #[test]
    fn rejects_barter_trades_it_cannot_represent() {
        use barter_instrument::exchange::ExchangeId;
        let unknown = NormalizedTick::try_from(&barter_event(ExchangeId::Bitfinex, 1.0, 1.0));
        assert!(
            matches!(unknown, Err(StreamError::UnknownExchange(ref m)) if m.contains("bitfinex"))
        );
        for (id, want) in [
            (ExchangeId::Kraken, Exchange::Kraken),
            (ExchangeId::Okx, Exchange::Okx),
            (ExchangeId::BybitSpot, Exchange::Bybit),
            (ExchangeId::BybitPerpetualsUsd, Exchange::Bybit),
        ] {
            let t = NormalizedTick::try_from(&barter_event(id, 1.0, 1.0)).unwrap();
            assert_eq!(t.exchange, want);
        }
        let bad = [
            (f64::NAN, 1.0),
            (f64::INFINITY, 1.0),
            (0.0, 1.0),
            (-1.0, 1.0),
            (1e300, 1.0),
            (1.0, f64::NAN),
            (1.0, -0.5),
        ];
        for (price, amount) in bad {
            let r = NormalizedTick::try_from(&barter_event(ExchangeId::BinanceSpot, price, amount));
            assert!(
                matches!(r, Err(StreamError::InvalidInput(_))),
                "{price} {amount} gave {r:?}"
            );
        }
        let mut before_epoch = barter_event(ExchangeId::BinanceSpot, 1.0, 1.0);
        before_epoch.time_received = chrono::DateTime::<chrono::Utc>::MIN_UTC;
        assert!(matches!(
            NormalizedTick::try_from(&before_epoch),
            Err(StreamError::InvalidInput(_))
        ));
        // A zero amount passes (some venues report one); only negative amounts fail.
        assert!(NormalizedTick::try_from(&barter_event(ExchangeId::BinanceSpot, 1.0, 0.0)).is_ok());
    }
}
