//! Extended market microstructure: adverse selection and LOB dynamics.
//!
//! Provides:
//! - Trade direction classification (tick rule, Lee-Ready)
//! - Adverse selection metrics (PIN estimation)
//! - LOB imbalance and market pressure
//! - Price impact models
//! - Hasbrouck information share
//! - Flow toxicity (VPIN, Kyle lambda)

/// Direction of a trade as seen from the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeDirection {
    /// Trade was buyer-initiated (aggressor was a buyer).
    BuyInitiated,
    /// Trade was seller-initiated (aggressor was a seller).
    SellInitiated,
    /// Direction cannot be determined.
    Unknown,
}

/// A single trade with microstructure metadata.
#[derive(Debug, Clone)]
pub struct Trade {
    /// Trade price.
    pub price: f64,
    /// Trade quantity.
    pub qty: f64,
    /// Classified trade direction.
    pub direction: TradeDirection,
    /// Timestamp in milliseconds.
    pub timestamp_ms: u64,
}

/// Classify trade direction using the tick rule.
///
/// - `current_price > prev_price` → `BuyInitiated` (up-tick)
/// - `current_price < prev_price` → `SellInitiated` (down-tick)
/// - `current_price == prev_price` → `Unknown` (zero-tick)
pub fn tick_rule(current_price: f64, prev_price: f64) -> TradeDirection {
    if current_price > prev_price {
        TradeDirection::BuyInitiated
    } else if current_price < prev_price {
        TradeDirection::SellInitiated
    } else {
        TradeDirection::Unknown
    }
}

/// Classify trade direction using the Lee-Ready rule.
///
/// - `trade_price > mid` → `BuyInitiated`
/// - `trade_price < mid` → `SellInitiated`
/// - `trade_price == mid` → `Unknown`
pub fn lee_ready_rule(trade_price: f64, mid: f64) -> TradeDirection {
    if trade_price > mid {
        TradeDirection::BuyInitiated
    } else if trade_price < mid {
        TradeDirection::SellInitiated
    } else {
        TradeDirection::Unknown
    }
}

/// Adverse selection metrics from the PIN (Probability of Informed Trading) model.
#[derive(Debug, Clone)]
pub struct AdverseSelectionMetrics {
    /// Probability of informed trading: PIN = alpha*mu / (alpha*mu + epsilon_b + epsilon_s).
    pub pin: f64,
    /// Fraction of trading days with informed trading (alpha).
    pub alpha: f64,
    /// Probability of good news given informed trading (delta).
    pub delta: f64,
    /// Informed trader arrival rate (mu).
    pub mu: f64,
}

/// Estimate adverse selection metrics using a simplified PIN model.
///
/// Given sequences of buy and sell volumes per period:
/// - epsilon_b = mean(buys), epsilon_s = mean(sells) (uninformed arrival rates)
/// - mu = max(mean(buys), mean(sells)) (informed arrival rate approximation)
/// - alpha = estimated from imbalance
/// - PIN = alpha*mu / (alpha*mu + epsilon_b + epsilon_s)
pub fn estimate_pin(buys: &[f64], sells: &[f64]) -> AdverseSelectionMetrics {
    let n = buys.len().min(sells.len()) as f64;
    if n < 1.0 {
        return AdverseSelectionMetrics {
            pin: 0.0,
            alpha: 0.0,
            delta: 0.5,
            mu: 0.0,
        };
    }

    let mean_buy: f64 = buys.iter().take(n as usize).sum::<f64>() / n;
    let mean_sell: f64 = sells.iter().take(n as usize).sum::<f64>() / n;

    let epsilon_b = mean_buy;
    let epsilon_s = mean_sell;

    // Informed arrival rate: mu ≈ |mean_buy - mean_sell|
    let mu = (mean_buy - mean_sell).abs().max(0.0);

    // Alpha: fraction of informed vs uninformed
    let total = epsilon_b + epsilon_s + mu;
    let alpha = if total > 0.0 { mu / total } else { 0.0 };
    let alpha = alpha.clamp(0.0, 1.0);

    // Delta: probability of good news
    let delta = if mean_buy > mean_sell { 0.5 + 0.5 * alpha } else { 0.5 - 0.5 * alpha };
    let delta = delta.clamp(0.0, 1.0);

    // PIN = alpha * mu / (alpha * mu + epsilon_b + epsilon_s)
    let pin_denom = alpha * mu + epsilon_b + epsilon_s;
    let pin = if pin_denom > 0.0 {
        (alpha * mu / pin_denom).clamp(0.0, 1.0)
    } else {
        0.0
    };

    AdverseSelectionMetrics { pin, alpha, delta, mu }
}

/// Limit order book depth imbalance metrics.
#[derive(Debug, Clone)]
pub struct LOBImbalance {
    /// Total weighted bid depth.
    pub bid_depth: f64,
    /// Total weighted ask depth.
    pub ask_depth: f64,
    /// Imbalance ratio: (bid - ask) / (bid + ask). Range [-1, 1].
    pub imbalance: f64,
    /// Buy pressure indicator: imbalance scaled to [0, 1] (0.5 = neutral).
    pub pressure: f64,
}

/// Compute LOB imbalance with exponential weighting across price levels.
///
/// `bid_depths` and `ask_depths` are depth quantities at successive price levels.
/// `weights` are the level weights (typically exponentially decaying).
pub fn compute_lob_imbalance(
    bid_depths: &[f64],
    ask_depths: &[f64],
    weights: &[f64],
) -> LOBImbalance {
    let n = bid_depths.len().min(ask_depths.len()).min(weights.len());

    let mut bid_depth = 0.0;
    let mut ask_depth = 0.0;

    for i in 0..n {
        bid_depth += weights[i] * bid_depths[i];
        ask_depth += weights[i] * ask_depths[i];
    }

    let total = bid_depth + ask_depth;
    let imbalance = if total > 0.0 {
        (bid_depth - ask_depth) / total
    } else {
        0.0
    };
    let pressure = (imbalance + 1.0) / 2.0; // map [-1,1] → [0,1]

    LOBImbalance {
        bid_depth,
        ask_depth,
        imbalance,
        pressure,
    }
}

/// Estimate price impact using the square-root market impact model.
///
/// Impact = kappa * sigma * sqrt(trade_size / daily_volume)
///
/// where kappa ≈ 1.0 is an empirical constant.
pub fn price_impact_model(trade_size: f64, sigma: f64, daily_volume: f64) -> f64 {
    if daily_volume <= 0.0 || trade_size <= 0.0 {
        return 0.0;
    }
    let kappa = 1.0;
    kappa * sigma * (trade_size / daily_volume).sqrt()
}

/// Compute Hasbrouck's information share for a venue.
///
/// IS = (var_venue - cov) / var_efficient
///
/// Measures the contribution of a venue to price discovery.
pub fn information_share(var_efficient: f64, var_venue: f64, cov: f64) -> f64 {
    if var_efficient <= 0.0 {
        return 0.0;
    }
    ((var_venue - cov) / var_efficient).clamp(0.0, 1.0)
}

/// Flow toxicity metrics.
#[derive(Debug, Clone)]
pub struct FlowToxicity {
    /// Volume-synchronized probability of informed trading (VPIN).
    pub vpin: f64,
    /// Kyle's lambda (price impact coefficient).
    pub kyle_lambda: f64,
    /// Adverse selection component of the bid-ask spread.
    pub adverse_sel_component: f64,
}

/// Estimate flow toxicity from trade prices and volumes.
///
/// Buckets trades by volume (`bucket_size`) and computes:
/// - VPIN = mean(|buy_vol - sell_vol|) / bucket_size
/// - Kyle lambda via price impact regression
/// - Adverse selection component ≈ VPIN * mean_spread
pub fn estimate_flow_toxicity(prices: &[f64], volumes: &[f64], bucket_size: f64) -> FlowToxicity {
    if prices.len() < 2 || volumes.is_empty() || bucket_size <= 0.0 {
        return FlowToxicity {
            vpin: 0.0,
            kyle_lambda: 0.0,
            adverse_sel_component: 0.0,
        };
    }

    let n = prices.len().min(volumes.len());

    // Build volume buckets
    let mut buy_vols: Vec<f64> = Vec::new();
    let mut sell_vols: Vec<f64> = Vec::new();
    let mut bucket_buy = 0.0;
    let mut bucket_sell = 0.0;
    let mut bucket_vol = 0.0;

    for i in 1..n {
        let dir = tick_rule(prices[i], prices[i - 1]);
        let v = volumes[i];
        match dir {
            TradeDirection::BuyInitiated => bucket_buy += v,
            TradeDirection::SellInitiated => bucket_sell += v,
            TradeDirection::Unknown => {
                bucket_buy += v * 0.5;
                bucket_sell += v * 0.5;
            }
        }
        bucket_vol += v;

        if bucket_vol >= bucket_size {
            buy_vols.push(bucket_buy);
            sell_vols.push(bucket_sell);
            bucket_buy = 0.0;
            bucket_sell = 0.0;
            bucket_vol = 0.0;
        }
    }

    // VPIN = mean |buy - sell| / bucket_size
    let vpin = if buy_vols.is_empty() {
        0.0
    } else {
        let imbal_sum: f64 = buy_vols
            .iter()
            .zip(sell_vols.iter())
            .map(|(b, s)| (b - s).abs())
            .sum();
        (imbal_sum / buy_vols.len() as f64) / bucket_size
    };
    let vpin = vpin.clamp(0.0, 1.0);

    // Kyle lambda: regress price changes on signed order flow
    let mut sum_xy = 0.0;
    let mut sum_xx = 0.0;
    for i in 1..n {
        let dp = prices[i] - prices[i - 1];
        let dir = tick_rule(prices[i], prices[i - 1]);
        let signed_vol = match dir {
            TradeDirection::BuyInitiated => volumes[i],
            TradeDirection::SellInitiated => -volumes[i],
            TradeDirection::Unknown => 0.0,
        };
        sum_xy += dp * signed_vol;
        sum_xx += signed_vol * signed_vol;
    }
    let kyle_lambda = if sum_xx > 0.0 { sum_xy / sum_xx } else { 0.0 };

    // Adverse selection component ≈ VPIN * average price change
    let avg_price_change: f64 = if n > 1 {
        (prices[n - 1] - prices[0]).abs() / (n - 1) as f64
    } else {
        0.0
    };
    let adverse_sel_component = vpin * avg_price_change;

    FlowToxicity {
        vpin,
        kyle_lambda,
        adverse_sel_component,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_rule_all_cases() {
        assert_eq!(tick_rule(101.0, 100.0), TradeDirection::BuyInitiated);
        assert_eq!(tick_rule(99.0, 100.0), TradeDirection::SellInitiated);
        assert_eq!(tick_rule(100.0, 100.0), TradeDirection::Unknown);
    }

    #[test]
    fn test_lee_ready_rule() {
        assert_eq!(lee_ready_rule(100.5, 100.0), TradeDirection::BuyInitiated);
        assert_eq!(lee_ready_rule(99.5, 100.0), TradeDirection::SellInitiated);
        assert_eq!(lee_ready_rule(100.0, 100.0), TradeDirection::Unknown);
    }

    #[test]
    fn test_pin_in_range() {
        let buys = vec![150.0, 130.0, 180.0, 120.0, 160.0];
        let sells = vec![80.0, 90.0, 70.0, 100.0, 85.0];
        let metrics = estimate_pin(&buys, &sells);
        assert!(metrics.pin >= 0.0 && metrics.pin <= 1.0, "PIN must be in [0,1]");
        assert!(metrics.alpha >= 0.0 && metrics.alpha <= 1.0, "Alpha must be in [0,1]");
        assert!(metrics.delta >= 0.0 && metrics.delta <= 1.0, "Delta must be in [0,1]");
    }

    #[test]
    fn test_lob_imbalance_bounds() {
        let bid_depths = vec![100.0, 50.0, 25.0];
        let ask_depths = vec![80.0, 40.0, 20.0];
        let weights = vec![1.0, 0.5, 0.25];
        let imb = compute_lob_imbalance(&bid_depths, &ask_depths, &weights);
        assert!(imb.imbalance >= -1.0 && imb.imbalance <= 1.0);
        assert!(imb.pressure >= 0.0 && imb.pressure <= 1.0);
        assert!(imb.bid_depth > 0.0);
        assert!(imb.ask_depth > 0.0);
    }

    #[test]
    fn test_lob_imbalance_neutral_equal_sides() {
        let depths = vec![100.0, 50.0];
        let weights = vec![1.0, 1.0];
        let imb = compute_lob_imbalance(&depths, &depths, &weights);
        assert!((imb.imbalance).abs() < 1e-10, "Equal sides should give zero imbalance");
        assert!((imb.pressure - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_price_impact_positive() {
        let impact = price_impact_model(1000.0, 0.02, 1_000_000.0);
        assert!(impact > 0.0, "Price impact should be positive");
    }

    #[test]
    fn test_price_impact_scales_with_size() {
        let small = price_impact_model(100.0, 0.01, 1_000_000.0);
        let large = price_impact_model(10_000.0, 0.01, 1_000_000.0);
        assert!(large > small, "Larger trade should have larger price impact");
    }

    #[test]
    fn test_flow_toxicity_vpin_bounds() {
        let prices = vec![100.0, 100.5, 100.3, 100.8, 100.6, 101.0, 100.7, 101.2];
        let volumes = vec![50.0, 120.0, 80.0, 200.0, 60.0, 150.0, 90.0, 180.0];
        let tox = estimate_flow_toxicity(&prices, &volumes, 200.0);
        assert!(tox.vpin >= 0.0 && tox.vpin <= 1.0, "VPIN must be in [0,1]");
    }

    #[test]
    fn test_information_share_bounds() {
        let is = information_share(0.001, 0.0008, 0.0002);
        assert!(is >= 0.0 && is <= 1.0, "Information share must be in [0,1]");
    }
}
