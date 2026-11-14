//! Roll a live barter-data trade stream into fin-stream 1-minute OHLCV bars.
//!
//! barter-data connects to the exchange and normalises its public trades; each
//! trade converts to a `NormalizedTick` (exact `Decimal` price and size) and goes
//! into `OhlcvAggregator`, which prints every bar as its minute closes.
//!
//! ```text
//! cargo run --example barter_candles --features barter            # 3 bars of BTC-USD on Coinbase
//! cargo run --example barter_candles --features barter -- eth 5   # 5 bars of ETH-USD
//! ```
//!
//! Needs network access to `wss://ws-feed.exchange.coinbase.com`. The first bar
//! printed is partial, because the stream starts partway through a minute.

use barter_data::exchange::coinbase::Coinbase;
use barter_data::streams::reconnect::{stream::ReconnectingStream, Event};
use barter_data::streams::Streams;
use barter_data::subscription::trade::PublicTrades;
use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;
use fin_stream::{NormalizedTick, OhlcvAggregator, Timeframe};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let base = args.next().unwrap_or_else(|| "btc".to_string()).to_lowercase();
    let mut bars_left: usize = match args.next() {
        Some(n) => n.parse()?,
        None => 3,
    };

    let streams = Streams::<PublicTrades>::builder()
        .subscribe([(Coinbase, base.as_str(), "usd", MarketDataInstrumentKind::Spot, PublicTrades)])
        .init()
        .await?;
    let mut trades = streams
        .select_all()
        .with_error_handler(|error| eprintln!("barter-data stream error: {error:?}"));

    // `NormalizedTick::try_from` uses the barter instrument's Display form as the
    // symbol: base_quote_kind, e.g. "btc_usd_spot".
    let symbol = format!("{base}_usd_spot");
    let mut bars = OhlcvAggregator::new(symbol.as_str(), Timeframe::Minutes(1))?;
    println!("aggregating {symbol} trades from Coinbase into 1-minute bars; waiting for {bars_left} to close");

    let mut trade_count = 0u64;
    while let Some(event) = trades.next().await {
        let trade = match event {
            Event::Item(trade) => trade,
            Event::Reconnecting(exchange) => {
                eprintln!("{exchange} disconnected; barter-data is reconnecting");
                continue;
            }
        };
        let tick = NormalizedTick::try_from(&trade)?;
        trade_count += 1;
        for bar in bars.feed(&tick)? {
            println!(
                "{} {}  O {}  H {}  L {}  C {}  V {}  trades {}  vwap {}",
                bar.bar_start_ms,
                bar.symbol,
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume,
                bar.trade_count,
                bar.vwap.map(|v| v.round_dp(2).to_string()).unwrap_or_default(),
            );
            bars_left = bars_left.saturating_sub(1);
        }
        if bars_left == 0 {
            break;
        }
    }
    println!("{trade_count} trades, {} late ticks dropped", bars.late_tick_count());
    Ok(())
}
