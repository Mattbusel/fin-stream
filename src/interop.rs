//! Hand fin-stream ticks to other Rust trading crates.
//!
//! | feature | target | conversion |
//! |---|---|---|
//! | `fin-primitives` (default) | [`fin_primitives::tick::Tick`] | `TryFrom<&NormalizedTick>`, then use fin-primitives' bar aggregator, 700+ indicators, order book and risk tools on live data |
//! | `barter` | `barter_data::subscription::trade::PublicTrade` | `TryFrom<&NormalizedTick>`, so strategies written against barter's trade type can consume fin-stream's Alpaca and Polygon (US equity) feeds, which barter-data does not cover |
//!
//! Both conversions fail with [`StreamError::InvalidInput`] instead of guessing
//! when the tick has no aggressor side, because both targets require one. Use
//! [`NormalizedTick::with_side_if_unknown`] first if you want a default.

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
            .ok_or_else(|| StreamError::InvalidInput(format!("timestamp {} ms is out of range", t.best_timestamp_ms())))?;
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
            d.to_f64().ok_or_else(|| StreamError::InvalidInput(format!("{d} does not fit f64")))
        };
        Ok(Self {
            id: t.trade_id.clone().unwrap_or_default(),
            price: f(t.price)?,
            amount: f(t.quantity)?,
            side,
        })
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
        assert_eq!(tick(None).with_side_if_unknown(TradeSide::Sell).side, Some(TradeSide::Sell));
        assert_eq!(tick(Some(TradeSide::Buy)).with_side_if_unknown(TradeSide::Sell).side, Some(TradeSide::Buy));
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
        let mut agg = OhlcvAggregator::new(Symbol::new("AAPL").unwrap(), Timeframe::Minutes(1)).unwrap();
        assert!(agg.push_tick(&t).unwrap().is_empty());
        assert_eq!(agg.current_bar().unwrap().close.value(), dec!(227.53));

        assert!(matches!(fin_primitives::tick::Tick::try_from(&tick(None)), Err(StreamError::InvalidInput(_))));
        let mut zero = tick(Some(TradeSide::Sell));
        zero.price = dec!(0);
        assert!(matches!(fin_primitives::tick::Tick::try_from(&zero), Err(StreamError::FinPrimitives(_))));
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
}
