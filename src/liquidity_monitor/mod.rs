//! Real-time bid-ask spread, market depth, and liquidity impact monitoring.

use std::collections::VecDeque;

/// A single quote update from the exchange.
pub struct QuoteUpdate {
    /// Best bid price.
    pub bid: f64,
    /// Best ask price.
    pub ask: f64,
    /// Bid-side depth (shares / contracts available at best bid).
    pub bid_size: f64,
    /// Ask-side depth (shares / contracts available at best ask).
    pub ask_size: f64,
    /// Wall-clock time of the quote.
    pub timestamp: std::time::Instant,
}

/// Summary of bid-ask spread metrics.
pub struct SpreadMetrics {
    /// Absolute spread: ask - bid.
    pub current_spread: f64,
    /// Spread in basis points relative to mid-price.
    pub spread_bps: f64,
    /// Exponentially weighted moving average of the spread.
    pub ema_spread: f64,
    /// Maximum spread observed in the rolling 1-hour window.
    pub max_spread_1h: f64,
    /// Minimum spread observed in the rolling 1-hour window.
    pub min_spread_1h: f64,
}

/// Order book depth summary.
pub struct DepthMetrics {
    /// Total size available on the bid side.
    pub total_bid_depth: f64,
    /// Total size available on the ask side.
    pub total_ask_depth: f64,
    /// Signed order imbalance: (bid - ask) / (bid + ask), range [-1, 1].
    pub imbalance: f64,
}

/// Estimate Kyle's lambda (price impact) via OLS regression of price changes on order flow.
///
/// Returns the slope coefficient (Δprice / order_flow unit).
pub fn kyle_lambda(price_changes: &[f64], order_flow: &[f64]) -> f64 {
    let n = price_changes.len().min(order_flow.len());
    if n < 2 {
        return 0.0;
    }
    let mean_x: f64 = order_flow[..n].iter().sum::<f64>() / n as f64;
    let mean_y: f64 = price_changes[..n].iter().sum::<f64>() / n as f64;
    let mut cov = 0.0_f64;
    let mut var_x = 0.0_f64;
    for i in 0..n {
        let dx = order_flow[i] - mean_x;
        let dy = price_changes[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
    }
    if var_x.abs() < 1e-14 {
        0.0
    } else {
        cov / var_x
    }
}

/// Real-time liquidity monitor: spread EMA, depth imbalance, and impact cost.
pub struct LiquidityMonitor {
    /// Rolling history of recent quotes.
    pub quote_history: VecDeque<QuoteUpdate>,
    /// Current EWMA spread estimate.
    pub spread_ema: f64,
    /// EMA smoothing factor (per quote update).
    pub spread_alpha: f64,
    /// Maximum number of quote snapshots to retain.
    pub max_history: usize,
}

impl LiquidityMonitor {
    /// Create a new monitor.
    ///
    /// `spread_alpha` — EMA weight in (0, 1); smaller = slower adaptation.
    /// `max_history`  — rolling window length (e.g. 3 600 for 1-hour at 1 Hz).
    pub fn new(spread_alpha: f64, max_history: usize) -> Self {
        LiquidityMonitor {
            quote_history: VecDeque::with_capacity(max_history),
            spread_ema: 0.0,
            spread_alpha,
            max_history,
        }
    }

    /// Ingest a new quote snapshot.
    pub fn update_quote(&mut self, q: QuoteUpdate) {
        let spread = q.ask - q.bid;
        if self.quote_history.is_empty() {
            self.spread_ema = spread;
        } else {
            self.spread_ema = self.spread_alpha * spread + (1.0 - self.spread_alpha) * self.spread_ema;
        }
        if self.quote_history.len() == self.max_history {
            self.quote_history.pop_front();
        }
        self.quote_history.push_back(q);
    }

    /// Compute spread metrics from the current quote history.
    pub fn spread_metrics(&self) -> SpreadMetrics {
        if self.quote_history.is_empty() {
            return SpreadMetrics {
                current_spread: 0.0,
                spread_bps: 0.0,
                ema_spread: 0.0,
                max_spread_1h: 0.0,
                min_spread_1h: 0.0,
            };
        }
        let last = self.quote_history.back().unwrap();
        let current_spread = last.ask - last.bid;
        let mid = (last.bid + last.ask) / 2.0;
        let spread_bps = if mid > 1e-10 {
            current_spread / mid * 10_000.0
        } else {
            0.0
        };

        let mut max_s = f64::NEG_INFINITY;
        let mut min_s = f64::INFINITY;
        for q in &self.quote_history {
            let s = q.ask - q.bid;
            if s > max_s { max_s = s; }
            if s < min_s { min_s = s; }
        }

        SpreadMetrics {
            current_spread,
            spread_bps,
            ema_spread: self.spread_ema,
            max_spread_1h: max_s,
            min_spread_1h: min_s,
        }
    }

    /// Compute depth imbalance from the latest quote. Returns `None` if no quotes.
    pub fn depth_metrics(&self) -> Option<DepthMetrics> {
        let q = self.quote_history.back()?;
        let total = q.bid_size + q.ask_size;
        let imbalance = if total > 1e-10 {
            (q.bid_size - q.ask_size) / total
        } else {
            0.0
        };
        Some(DepthMetrics {
            total_bid_depth: q.bid_size,
            total_ask_depth: q.ask_size,
            imbalance,
        })
    }

    /// Estimate market impact for a given trade size using a square-root model.
    ///
    /// `impact ≈ kyle_lambda_approx × sqrt(trade_size / mid_price)`
    pub fn market_impact_estimate(&self, trade_size: f64, mid_price: f64) -> f64 {
        if mid_price < 1e-10 || self.quote_history.len() < 2 {
            return 0.0;
        }
        // Approximate Kyle lambda from recent spread EMA as a proxy
        let kyle_lambda_approx = self.spread_ema / mid_price;
        kyle_lambda_approx * (trade_size / mid_price).sqrt()
    }

    /// Composite liquidity score in [0, 1].
    ///
    /// Higher score = tighter spreads + balanced depth + lower impact.
    pub fn liquidity_score(&self) -> f64 {
        let metrics = self.spread_metrics();
        let depth = match self.depth_metrics() {
            Some(d) => d,
            None => return 0.0,
        };

        // Spread score: tight spread (< 5 bps) = 1.0, > 50 bps = 0.0
        let spread_score = 1.0 - (metrics.spread_bps / 50.0).min(1.0);

        // Depth balance score: perfectly balanced = 1.0, fully one-sided = 0.0
        let balance_score = 1.0 - depth.imbalance.abs();

        // Combine with equal weights
        (spread_score + balance_score) / 2.0
    }
}
