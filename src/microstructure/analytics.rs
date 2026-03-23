//! # Module: microstructure::analytics
//!
//! Higher-level market microstructure measures:
//! - Hasbrouck (1995) Information Share (simplified via log-price innovations)
//! - Amihud (2002) illiquidity ratio: standalone and rolling
//! - Kyle (1985) lambda: OLS price-impact coefficient
//! - Effective bid-ask spread, realized spread, and price impact
//! - `MicrostructureAnalyzer`: rolling-window aggregator for all measures
//!
//! ## NOT Responsible For
//! - Full vector-error-correction model for Information Share
//! - Order-book reconstruction (use the `book` module)

use std::collections::VecDeque;

// ─── InformationShare ─────────────────────────────────────────────────────────

/// Hasbrouck (1995) price discovery information share — simplified.
///
/// The simplified measure uses the variance of log-price innovations (first
/// differences of log prices) as a proxy for each venue's contribution to
/// price discovery.
///
/// `is_a + is_b = 1.0`
pub struct InformationShare;

impl InformationShare {
    /// Compute the information share for two price series of equal length.
    ///
    /// Returns `(is_a, is_b)` where values sum to 1.0.
    /// Falls back to `(0.5, 0.5)` if both series have zero variance.
    ///
    /// # Arguments
    /// - `price_series_a`: prices for venue / series A.
    /// - `price_series_b`: prices for venue / series B.
    pub fn compute(price_series_a: &[f64], price_series_b: &[f64]) -> (f64, f64) {
        let innov_a = Self::innovations(price_series_a);
        let innov_b = Self::innovations(price_series_b);
        let var_a = variance(&innov_a);
        let var_b = variance(&innov_b);
        let total = var_a + var_b;
        if total < f64::EPSILON {
            return (0.5, 0.5);
        }
        (var_a / total, var_b / total)
    }

    /// Compute first differences of log prices.
    ///
    /// `innovations[t] = ln(price[t]) - ln(price[t-1])`
    ///
    /// Returns an empty `Vec` if `prices.len() < 2`.
    pub fn innovations(prices: &[f64]) -> Vec<f64> {
        if prices.len() < 2 {
            return vec![];
        }
        prices
            .windows(2)
            .filter_map(|w| {
                if w[0] > 0.0 && w[1] > 0.0 {
                    Some((w[1] / w[0]).ln())
                } else {
                    None
                }
            })
            .collect()
    }
}

// ─── AmihudIlliquidity (standalone) ─────────────────────────────────────────

/// Amihud (2002) illiquidity ratio — standalone (no streaming state).
///
/// `ILLIQ = (1/T) * Σ |r_t| / V_t * scale`
///
/// where `scale = 1e6` by convention (adjusts for typical volume magnitudes).
pub struct AmihudIlliquidityCalc;

impl AmihudIlliquidityCalc {
    /// Compute the daily average Amihud illiquidity ratio.
    ///
    /// Returns 0.0 if the inputs are empty or all volumes are zero.
    ///
    /// `returns[t]` should be the log-return or price-change for bar `t`.
    /// `volumes[t]` is the traded volume for bar `t`.
    pub fn compute(returns: &[f64], volumes: &[f64]) -> f64 {
        let n = returns.len().min(volumes.len());
        if n == 0 {
            return 0.0;
        }
        let scale = 1e6_f64;
        let sum: f64 = returns[..n]
            .iter()
            .zip(volumes[..n].iter())
            .filter_map(|(&r, &v)| {
                if v > 0.0 { Some(r.abs() / v * scale) } else { None }
            })
            .sum();
        let count = returns[..n]
            .iter()
            .zip(volumes[..n].iter())
            .filter(|(_, &v)| v > 0.0)
            .count();
        if count == 0 { 0.0 } else { sum / count as f64 }
    }

    /// Compute a rolling Amihud illiquidity ratio with the given window size.
    ///
    /// The returned vector has length `returns.len()` with `0.0` for positions
    /// where fewer than `window` valid observations are available.
    pub fn rolling(returns: &[f64], volumes: &[f64], window: usize) -> Vec<f64> {
        let n = returns.len().min(volumes.len());
        if window == 0 || n == 0 {
            return vec![0.0; n];
        }
        let mut result = vec![0.0_f64; n];
        for i in 0..n {
            let start = i.saturating_sub(window - 1);
            let r_slice = &returns[start..=i];
            let v_slice = &volumes[start..=i];
            result[i] = Self::compute(r_slice, v_slice);
        }
        result
    }
}

// ─── KyleLambda ──────────────────────────────────────────────────────────────

/// Kyle (1985) lambda: price impact coefficient.
///
/// Estimated via OLS regression of price changes on signed order flow:
/// `Δp = λ * Q + ε`
///
/// Higher lambda = less liquid market (more price impact per unit volume).
pub struct KyleLambda;

impl KyleLambda {
    /// Estimate Kyle's lambda via OLS.
    ///
    /// Returns `None` if there are fewer than 2 observations or if the
    /// signed volume has zero variance (regression undefined).
    ///
    /// - `price_changes`: Δp for each bar.
    /// - `signed_volume`: net signed order flow Q for each bar (positive = buy).
    pub fn estimate(price_changes: &[f64], signed_volume: &[f64]) -> Option<f64> {
        let n = price_changes.len().min(signed_volume.len());
        if n < 2 {
            return None;
        }
        ols_slope(&signed_volume[..n], &price_changes[..n])
    }
}

/// OLS slope β = Σ(x - x̄)(y - ȳ) / Σ(x - x̄)²
fn ols_slope(x: &[f64], y: &[f64]) -> Option<f64> {
    let n = x.len() as f64;
    let mean_x = x.iter().sum::<f64>() / n;
    let mean_y = y.iter().sum::<f64>() / n;
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (&xi, &yi) in x.iter().zip(y.iter()) {
        num += (xi - mean_x) * (yi - mean_y);
        den += (xi - mean_x).powi(2);
    }
    if den.abs() < f64::EPSILON {
        return None;
    }
    Some(num / den)
}

fn variance(data: &[f64]) -> f64 {
    let n = data.len();
    if n < 2 {
        return 0.0;
    }
    let mean = data.iter().sum::<f64>() / n as f64;
    data.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64
}

// ─── EffectiveBidAskSpread ───────────────────────────────────────────────────

/// Effective bid-ask spread and related price-impact decomposition.
///
/// Decomposes the total spread into:
/// - **Effective half-spread**: `|trade_price - mid|`
/// - **Realized half-spread**: compensation for adverse selection
/// - **Price impact**: permanent price change after the trade
pub struct EffectiveBidAskSpread;

impl EffectiveBidAskSpread {
    /// Effective full spread = `2 * |trade_price - prevailing_mid|`.
    pub fn compute(trade_price: f64, prevailing_mid: f64) -> f64 {
        2.0 * (trade_price - prevailing_mid).abs()
    }

    /// Realized spread = `2 * direction * (trade_price - future_mid)`.
    ///
    /// - `direction`: +1 for buyer-initiated, -1 for seller-initiated.
    /// - `future_mid`: mid-price some time after the trade (e.g. 5 minutes).
    ///
    /// Positive realized spread = market maker was compensated.
    pub fn realized_spread(trade_price: f64, future_mid: f64, direction: f64) -> f64 {
        2.0 * direction * (trade_price - future_mid)
    }

    /// Price impact = `2 * direction * (future_mid - prevailing_mid)`.
    ///
    /// Measures the permanent price change caused by the informed trade.
    pub fn price_impact(
        trade_price: f64,
        prevailing_mid: f64,
        future_mid: f64,
        direction: f64,
    ) -> f64 {
        let _ = trade_price; // included for symmetry; not used in this formula
        2.0 * direction * (future_mid - prevailing_mid)
    }
}

// ─── MicrostructureAnalyzer ──────────────────────────────────────────────────

/// Observation stored per trade.
struct TradeObs {
    price: f64,
    volume: f64,
    direction: f64,
    mid: f64,
}

/// Rolling-window microstructure analyzer.
///
/// Accumulates trade observations and computes Amihud, Kyle lambda, and
/// effective spread over a sliding window.
pub struct MicrostructureAnalyzer {
    window: usize,
    trades: VecDeque<TradeObs>,
}

impl MicrostructureAnalyzer {
    /// Create a new analyzer with the given rolling window size.
    pub fn new(window: usize) -> Self {
        let window = window.max(2);
        Self { window, trades: VecDeque::with_capacity(window + 1) }
    }

    /// Push a new trade observation.
    ///
    /// - `price`: trade price.
    /// - `volume`: trade volume (absolute, positive).
    /// - `direction`: +1 buyer-initiated, -1 seller-initiated.
    /// - `mid`: prevailing mid-price at the time of the trade.
    pub fn push_trade(&mut self, price: f64, volume: f64, direction: f64, mid: f64) {
        self.trades.push_back(TradeObs { price, volume, direction, mid });
        if self.trades.len() > self.window {
            self.trades.pop_front();
        }
    }

    /// Returns `true` when the window is fully populated.
    pub fn is_ready(&self) -> bool {
        self.trades.len() >= self.window
    }

    /// Rolling Amihud illiquidity ratio over the current window.
    ///
    /// Returns `None` if fewer than 2 trades are buffered.
    pub fn amihud_ratio(&self) -> Option<f64> {
        if self.trades.len() < 2 {
            return None;
        }
        let prices: Vec<f64> = self.trades.iter().map(|t| t.price).collect();
        let volumes: Vec<f64> = self.trades.iter().map(|t| t.volume).collect();
        // Compute log-returns from consecutive prices
        let returns: Vec<f64> = prices.windows(2)
            .filter_map(|w| if w[0] > 0.0 && w[1] > 0.0 { Some((w[1] / w[0]).ln()) } else { None })
            .collect();
        let vols: Vec<f64> = volumes[1..].to_vec(); // align with returns
        let ratio = AmihudIlliquidityCalc::compute(&returns, &vols);
        Some(ratio)
    }

    /// Kyle lambda (OLS estimate) over the current window.
    ///
    /// Returns `None` if fewer than 2 trades are buffered.
    pub fn kyle_lambda(&self) -> Option<f64> {
        if self.trades.len() < 2 {
            return None;
        }
        let prices: Vec<f64> = self.trades.iter().map(|t| t.price).collect();
        let price_changes: Vec<f64> = prices.windows(2).map(|w| w[1] - w[0]).collect();
        let signed_vols: Vec<f64> = self.trades
            .iter()
            .skip(1) // align with price changes
            .map(|t| t.direction * t.volume)
            .collect();
        KyleLambda::estimate(&price_changes, &signed_vols)
    }

    /// Average effective spread over the current window.
    ///
    /// Returns `None` if no trades are buffered.
    pub fn effective_spread(&self) -> Option<f64> {
        if self.trades.is_empty() {
            return None;
        }
        let sum: f64 = self.trades
            .iter()
            .map(|t| EffectiveBidAskSpread::compute(t.price, t.mid))
            .sum();
        Some(sum / self.trades.len() as f64)
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_information_share_sums_to_one() {
        let prices_a = vec![100.0, 100.1, 99.9, 100.2, 100.0, 100.3];
        let prices_b = vec![100.0, 100.05, 100.0, 100.15, 99.95, 100.25];
        let (is_a, is_b) = InformationShare::compute(&prices_a, &prices_b);
        assert!((is_a + is_b - 1.0).abs() < 1e-9, "is_a={is_a} is_b={is_b}");
        assert!(is_a >= 0.0 && is_b >= 0.0);
    }

    #[test]
    fn test_information_share_equal_when_same_series() {
        let prices = vec![100.0, 101.0, 99.0, 102.0, 98.0];
        let (is_a, is_b) = InformationShare::compute(&prices, &prices);
        assert!((is_a - 0.5).abs() < 1e-9);
        assert!((is_b - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_information_share_zero_variance_fallback() {
        let flat = vec![100.0, 100.0, 100.0, 100.0];
        let (is_a, is_b) = InformationShare::compute(&flat, &flat);
        assert!((is_a - 0.5).abs() < 1e-9);
        assert!((is_b - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_innovations_length() {
        let prices = vec![100.0, 101.0, 99.5, 102.0];
        let innov = InformationShare::innovations(&prices);
        assert_eq!(innov.len(), 3);
    }

    #[test]
    fn test_amihud_formula() {
        // |r| / V * scale; with one return of 0.01 and volume 1000
        // ILLIQ = 0.01 / 1000 * 1e6 = 10
        let returns = vec![0.01_f64];
        let volumes = vec![1000.0_f64];
        let illiq = AmihudIlliquidityCalc::compute(&returns, &volumes);
        assert!((illiq - 10.0).abs() < 1e-9, "illiq={illiq}");
    }

    #[test]
    fn test_amihud_zero_volume_skipped() {
        let returns = vec![0.01, 0.02];
        let volumes = vec![0.0, 1000.0]; // first has zero volume
        let illiq = AmihudIlliquidityCalc::compute(&returns, &volumes);
        // Only second contributes: 0.02 / 1000 * 1e6 = 20
        assert!((illiq - 20.0).abs() < 1e-9, "illiq={illiq}");
    }

    #[test]
    fn test_amihud_rolling_length() {
        let returns = vec![0.01, 0.02, 0.03, 0.04, 0.05];
        let volumes = vec![1000.0; 5];
        let rolling = AmihudIlliquidityCalc::rolling(&returns, &volumes, 3);
        assert_eq!(rolling.len(), 5);
    }

    #[test]
    fn test_kyle_lambda_ols() {
        // Δp = 2 * Q → lambda should be ~2.0
        let price_changes = vec![2.0, 4.0, -2.0, -4.0, 6.0];
        let signed_volume  = vec![1.0, 2.0, -1.0, -2.0, 3.0];
        let lambda = KyleLambda::estimate(&price_changes, &signed_volume).unwrap();
        assert!((lambda - 2.0).abs() < 1e-6, "lambda={lambda}");
    }

    #[test]
    fn test_kyle_lambda_insufficient_data() {
        assert!(KyleLambda::estimate(&[1.0], &[1.0]).is_none());
        assert!(KyleLambda::estimate(&[], &[]).is_none());
    }

    #[test]
    fn test_kyle_lambda_zero_volume_variance() {
        // All signed volumes identical → zero variance in X → None
        let price_changes = vec![1.0, 2.0, 3.0];
        let signed_volume  = vec![5.0, 5.0, 5.0];
        assert!(KyleLambda::estimate(&price_changes, &signed_volume).is_none());
    }

    #[test]
    fn test_effective_spread_formula() {
        // trade at 100.5, mid at 100.0 → effective spread = 2 * 0.5 = 1.0
        let spread = EffectiveBidAskSpread::compute(100.5, 100.0);
        assert!((spread - 1.0).abs() < 1e-9, "spread={spread}");
    }

    #[test]
    fn test_realized_spread() {
        // Buy at 100.5, future mid 100.2, direction=+1
        // realized = 2 * 1 * (100.5 - 100.2) = 0.6
        let rs = EffectiveBidAskSpread::realized_spread(100.5, 100.2, 1.0);
        assert!((rs - 0.6).abs() < 1e-9, "rs={rs}");
    }

    #[test]
    fn test_price_impact_formula() {
        // direction=+1, prevailing_mid=100.0, future_mid=100.3
        // impact = 2 * 1 * (100.3 - 100.0) = 0.6
        let impact = EffectiveBidAskSpread::price_impact(100.5, 100.0, 100.3, 1.0);
        assert!((impact - 0.6).abs() < 1e-9, "impact={impact}");
    }

    #[test]
    fn test_spread_plus_impact_approx_equals_effective_spread() {
        // Effective spread ≈ realized spread + price impact
        let trade_price = 100.5;
        let mid = 100.0;
        let future_mid = 100.2;
        let direction = 1.0;
        let eff = EffectiveBidAskSpread::compute(trade_price, mid);
        let rs = EffectiveBidAskSpread::realized_spread(trade_price, future_mid, direction);
        let pi = EffectiveBidAskSpread::price_impact(trade_price, mid, future_mid, direction);
        assert!((eff - rs - pi).abs() < 1e-9, "eff={eff} rs={rs} pi={pi}");
    }

    #[test]
    fn test_analyzer_amihud_requires_two_trades() {
        let mut analyzer = MicrostructureAnalyzer::new(10);
        analyzer.push_trade(100.0, 1000.0, 1.0, 100.0);
        assert!(analyzer.amihud_ratio().is_none());
        analyzer.push_trade(100.5, 500.0, 1.0, 100.25);
        assert!(analyzer.amihud_ratio().is_some());
    }

    #[test]
    fn test_analyzer_kyle_lambda() {
        let mut analyzer = MicrostructureAnalyzer::new(10);
        // Push data where Δp = 2 * signed_volume
        for i in 0..5 {
            let v = (i + 1) as f64;
            let price = 100.0 + 2.0 * v;
            analyzer.push_trade(price, v, 1.0, price - 1.0);
        }
        let lambda = analyzer.kyle_lambda();
        assert!(lambda.is_some());
        assert!(lambda.unwrap() > 0.0);
    }

    #[test]
    fn test_analyzer_effective_spread() {
        let mut analyzer = MicrostructureAnalyzer::new(5);
        // Trade 1.0 above mid each time
        for _ in 0..5 {
            analyzer.push_trade(101.0, 1000.0, 1.0, 100.0);
        }
        let spread = analyzer.effective_spread().unwrap();
        // 2 * |101 - 100| = 2.0
        assert!((spread - 2.0).abs() < 1e-9, "spread={spread}");
    }

    #[test]
    fn test_analyzer_effective_spread_none_when_empty() {
        let analyzer = MicrostructureAnalyzer::new(5);
        assert!(analyzer.effective_spread().is_none());
    }
}
