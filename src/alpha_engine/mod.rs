//! Alpha signal generation engine.
//!
//! Provides momentum, mean-reversion, and volatility alpha computations,
//! with a concurrent [`AlphaEngine`] that stores signals per symbol using DashMap.

use dashmap::DashMap;

// ---------------------------------------------------------------------------
// Signal strength
// ---------------------------------------------------------------------------

/// Strength of an alpha signal.
#[derive(Debug, Clone)]
pub enum SignalStrength {
    /// Strong signal with associated magnitude.
    Strong(f64),
    /// Moderate signal with associated magnitude.
    Moderate(f64),
    /// Weak signal with associated magnitude.
    Weak(f64),
    /// No meaningful signal.
    Neutral,
}

impl SignalStrength {
    /// Return the numeric value of the signal strength (0.0 for `Neutral`).
    pub fn value(&self) -> f64 {
        match self {
            SignalStrength::Strong(v) => *v,
            SignalStrength::Moderate(v) => *v,
            SignalStrength::Weak(v) => *v,
            SignalStrength::Neutral => 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Signal types
// ---------------------------------------------------------------------------

/// Category of an alpha signal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AlphaSignalType {
    /// Price momentum signal.
    Momentum,
    /// Mean-reversion signal.
    MeanReversion,
    /// Quality factor signal.
    Quality,
    /// Value factor signal.
    Value,
    /// Volatility signal.
    Volatility,
    /// Sentiment signal.
    Sentiment,
    /// Technical indicator signal.
    Technical,
}

/// A single alpha signal for a symbol.
#[derive(Debug, Clone)]
pub struct AlphaSignal {
    /// Instrument symbol.
    pub symbol: String,
    /// Type of alpha signal.
    pub signal_type: AlphaSignalType,
    /// Strength and magnitude of the signal.
    pub strength: SignalStrength,
    /// Confidence in this signal (0.0 – 1.0).
    pub confidence: f64,
    /// Unix timestamp (nanoseconds) when the signal was generated.
    pub generated_at: u64,
}

// ---------------------------------------------------------------------------
// Momentum alpha
// ---------------------------------------------------------------------------

/// Momentum-based alpha computations.
pub struct MomentumAlpha;

impl MomentumAlpha {
    /// Compute raw momentum: `(last_price - first_price) / first_price`.
    ///
    /// Returns `0.0` if `prices` has fewer than `lookback + 1` elements or first price is zero.
    pub fn compute(prices: &[f64], lookback: usize) -> f64 {
        if prices.len() < lookback + 1 || lookback == 0 {
            return 0.0;
        }
        let start = prices.len() - lookback - 1;
        let first = prices[start];
        let last = *prices.last().unwrap_or(&0.0);
        if first.abs() < 1e-12 {
            return 0.0;
        }
        (last - first) / first
    }

    /// Risk-adjusted momentum: raw momentum divided by recent volatility.
    pub fn risk_adjusted(prices: &[f64], lookback: usize) -> f64 {
        if prices.len() < lookback + 2 {
            return 0.0;
        }
        let raw = Self::compute(prices, lookback);
        let window = &prices[prices.len().saturating_sub(lookback)..];
        let vol = realized_vol_from_prices(window);
        if vol < 1e-12 {
            raw
        } else {
            raw / vol
        }
    }

    /// Compute cross-sectional percentile ranks for a slice of momentum values.
    ///
    /// Returns ranks in `[0.0, 1.0]` where 1.0 is the highest momentum.
    pub fn cross_sectional_rank(all_momenta: &[f64]) -> Vec<f64> {
        let n = all_momenta.len();
        if n == 0 {
            return vec![];
        }
        let mut indexed: Vec<(usize, f64)> = all_momenta.iter().cloned().enumerate().collect();
        indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut ranks = vec![0.0_f64; n];
        for (rank, (orig_idx, _)) in indexed.iter().enumerate() {
            ranks[*orig_idx] = rank as f64 / (n - 1).max(1) as f64;
        }
        ranks
    }
}

// ---------------------------------------------------------------------------
// Mean reversion alpha
// ---------------------------------------------------------------------------

/// Mean-reversion alpha computations.
pub struct MeanReversionAlpha;

impl MeanReversionAlpha {
    /// Compute the z-score of the last price relative to a rolling window mean and std.
    pub fn z_score(prices: &[f64], window: usize) -> f64 {
        if prices.len() < window || window == 0 {
            return 0.0;
        }
        let slice = &prices[prices.len() - window..];
        let mean = slice.iter().sum::<f64>() / window as f64;
        let var = slice.iter().map(|&p| (p - mean).powi(2)).sum::<f64>() / window as f64;
        let std = var.sqrt();
        if std < 1e-12 {
            return 0.0;
        }
        let last = *prices.last().unwrap_or(&0.0);
        (last - mean) / std
    }

    /// Estimate the half-life of mean reversion via OLS AR(1): `dp_t = alpha + beta * p_{t-1}`.
    ///
    /// Half-life = `-ln(2) / ln(|beta|)`. Returns `f64::INFINITY` if |beta| >= 1 or estimation fails.
    pub fn half_life(prices: &[f64]) -> f64 {
        let n = prices.len();
        if n < 3 {
            return f64::INFINITY;
        }
        // OLS: regress dp on p_{t-1}
        // y_i = dp_i = prices[i] - prices[i-1]
        // x_i = prices[i-1]
        let y: Vec<f64> = (1..n).map(|i| prices[i] - prices[i - 1]).collect();
        let x: Vec<f64> = (1..n).map(|i| prices[i - 1]).collect();
        let m = x.len();

        let mx = x.iter().sum::<f64>() / m as f64;
        let my = y.iter().sum::<f64>() / m as f64;
        let num: f64 = x.iter().zip(y.iter()).map(|(&xi, &yi)| (xi - mx) * (yi - my)).sum();
        let den: f64 = x.iter().map(|&xi| (xi - mx).powi(2)).sum();

        if den.abs() < 1e-12 {
            return f64::INFINITY;
        }
        let beta = num / den;
        // beta from AR(1): dp = beta * p + alpha  ⟹  p_t = (1 + beta) * p_{t-1} + alpha
        // so AR coefficient rho = 1 + beta
        let rho = 1.0 + beta;
        if rho.abs() >= 1.0 || rho <= 0.0 {
            return f64::INFINITY;
        }
        -std::f64::consts::LN_2 / rho.ln()
    }

    /// Returns `true` if the series is mean-reverting: half-life > 0 and < 60.
    pub fn is_reverting(prices: &[f64]) -> bool {
        let hl = Self::half_life(prices);
        hl > 0.0 && hl < 60.0
    }
}

// ---------------------------------------------------------------------------
// Volatility alpha
// ---------------------------------------------------------------------------

/// Volatility-based alpha computations.
pub struct VolatilityAlpha;

impl VolatilityAlpha {
    /// Compute realized annualized volatility from returns over a window.
    pub fn realized_vol(returns: &[f64], window: usize) -> f64 {
        if returns.len() < window || window == 0 {
            return 0.0;
        }
        let slice = &returns[returns.len() - window..];
        let mean = slice.iter().sum::<f64>() / window as f64;
        let var = slice.iter().map(|&r| (r - mean).powi(2)).sum::<f64>() / window as f64;
        // Annualized: multiply by sqrt(252)
        var.sqrt() * 252_f64.sqrt()
    }

    /// Classify the current volatility regime based on the last 20 days of prices.
    pub fn vol_regime(prices: &[f64]) -> &'static str {
        let window = 20.min(prices.len());
        if window < 2 {
            return "normal";
        }
        let returns: Vec<f64> = prices
            .windows(2)
            .rev()
            .take(window)
            .map(|w| {
                if w[0].abs() > 1e-12 {
                    (w[1] - w[0]) / w[0]
                } else {
                    0.0
                }
            })
            .collect();
        let ann_vol = Self::realized_vol(&returns, returns.len().min(window));
        match ann_vol {
            v if v < 0.10 => "low",
            v if v < 0.20 => "normal",
            v if v < 0.40 => "high",
            _ => "extreme",
        }
    }

    /// Compute volatility of volatility: std dev of rolling vol estimates.
    pub fn vol_of_vol(prices: &[f64], window: usize, vvol_window: usize) -> f64 {
        let n = prices.len();
        if n < window + vvol_window || window == 0 || vvol_window == 0 {
            return 0.0;
        }
        // Compute a rolling vol series
        let mut vols = Vec::with_capacity(vvol_window);
        for k in 0..vvol_window {
            let start = n - window - vvol_window + k;
            let end = start + window;
            if end > n {
                break;
            }
            let slice = &prices[start..end];
            let returns: Vec<f64> = slice
                .windows(2)
                .map(|w| {
                    if w[0].abs() > 1e-12 {
                        (w[1] - w[0]) / w[0]
                    } else {
                        0.0
                    }
                })
                .collect();
            vols.push(Self::realized_vol(&returns, returns.len()));
        }
        let m = vols.len();
        if m < 2 {
            return 0.0;
        }
        let mean = vols.iter().sum::<f64>() / m as f64;
        let var = vols.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / m as f64;
        var.sqrt()
    }
}

// ---------------------------------------------------------------------------
// Alpha engine
// ---------------------------------------------------------------------------

/// Concurrent alpha signal generation engine.
///
/// Stores computed signals per symbol in a [`DashMap`] and provides
/// methods for computing, querying, and combining signals.
pub struct AlphaEngine {
    signals: DashMap<String, Vec<AlphaSignal>>,
}

impl AlphaEngine {
    /// Create a new empty `AlphaEngine`.
    pub fn new() -> Self {
        Self {
            signals: DashMap::new(),
        }
    }

    /// Compute momentum, mean-reversion, and volatility signals for a symbol.
    ///
    /// Signals are stored internally and also returned.
    pub fn compute_signals(&self, symbol: &str, prices: &[f64]) -> Vec<AlphaSignal> {
        let mut result = Vec::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        // --- Momentum ---
        let lookback = 20.min(prices.len().saturating_sub(1));
        let mom = MomentumAlpha::compute(prices, lookback);
        let mom_strength = classify_strength(mom);
        result.push(AlphaSignal {
            symbol: symbol.to_string(),
            signal_type: AlphaSignalType::Momentum,
            confidence: (mom.abs()).min(1.0),
            strength: mom_strength,
            generated_at: now,
        });

        // --- Mean reversion ---
        let z = MeanReversionAlpha::z_score(prices, 20.min(prices.len()));
        let rev_strength = classify_strength(-z); // negative z → positive reversion signal
        result.push(AlphaSignal {
            symbol: symbol.to_string(),
            signal_type: AlphaSignalType::MeanReversion,
            confidence: (z.abs() / 3.0).min(1.0),
            strength: rev_strength,
            generated_at: now,
        });

        // --- Volatility ---
        let returns: Vec<f64> = prices
            .windows(2)
            .map(|w| {
                if w[0].abs() > 1e-12 {
                    (w[1] - w[0]) / w[0]
                } else {
                    0.0
                }
            })
            .collect();
        let vol = VolatilityAlpha::realized_vol(&returns, returns.len().min(20));
        let vol_strength = classify_strength(vol);
        result.push(AlphaSignal {
            symbol: symbol.to_string(),
            signal_type: AlphaSignalType::Volatility,
            confidence: (vol * 5.0).min(1.0),
            strength: vol_strength,
            generated_at: now,
        });

        self.signals
            .entry(symbol.to_string())
            .or_default()
            .extend(result.clone());

        result
    }

    /// Compute a weighted combination of signal values for a symbol.
    ///
    /// Matches stored signals by type and computes the weighted sum.
    pub fn combined_alpha(&self, symbol: &str, weights: &[(AlphaSignalType, f64)]) -> f64 {
        let entry = match self.signals.get(symbol) {
            Some(e) => e,
            None => return 0.0,
        };
        let mut total = 0.0_f64;
        let mut weight_sum = 0.0_f64;
        for (signal_type, w) in weights {
            if let Some(sig) = entry.iter().rev().find(|s| &s.signal_type == signal_type) {
                total += w * sig.strength.value();
                weight_sum += w.abs();
            }
        }
        if weight_sum < 1e-12 {
            0.0
        } else {
            total / weight_sum
        }
    }

    /// Return the top `n` signals across all symbols, ranked by absolute strength.
    pub fn top_signals(&self, n: usize) -> Vec<AlphaSignal> {
        let mut all: Vec<AlphaSignal> = self
            .signals
            .iter()
            .flat_map(|entry| entry.value().clone())
            .collect();
        all.sort_by(|a, b| {
            b.strength
                .value()
                .abs()
                .partial_cmp(&a.strength.value().abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all.truncate(n);
        all
    }

    /// Compute Pearson correlation between two signal value series.
    pub fn signal_correlation(signals_a: &[f64], signals_b: &[f64]) -> f64 {
        let n = signals_a.len().min(signals_b.len());
        if n == 0 {
            return 0.0;
        }
        let mean_a = signals_a[..n].iter().sum::<f64>() / n as f64;
        let mean_b = signals_b[..n].iter().sum::<f64>() / n as f64;
        let mut cov = 0.0_f64;
        let mut var_a = 0.0_f64;
        let mut var_b = 0.0_f64;
        for i in 0..n {
            let da = signals_a[i] - mean_a;
            let db = signals_b[i] - mean_b;
            cov += da * db;
            var_a += da * da;
            var_b += db * db;
        }
        let denom = (var_a * var_b).sqrt();
        if denom < 1e-12 {
            0.0
        } else {
            (cov / denom).clamp(-1.0, 1.0)
        }
    }
}

impl Default for AlphaEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Classify a numeric value into a [`SignalStrength`].
fn classify_strength(v: f64) -> SignalStrength {
    let abs = v.abs();
    if abs >= 0.5 {
        SignalStrength::Strong(v)
    } else if abs >= 0.2 {
        SignalStrength::Moderate(v)
    } else if abs >= 0.05 {
        SignalStrength::Weak(v)
    } else {
        SignalStrength::Neutral
    }
}

/// Compute realized vol (annualized std dev) from a price series.
fn realized_vol_from_prices(prices: &[f64]) -> f64 {
    if prices.len() < 2 {
        return 0.0;
    }
    let returns: Vec<f64> = prices
        .windows(2)
        .map(|w| {
            if w[0].abs() > 1e-12 {
                (w[1] - w[0]) / w[0]
            } else {
                0.0
            }
        })
        .collect();
    let n = returns.len();
    let mean = returns.iter().sum::<f64>() / n as f64;
    let var = returns.iter().map(|&r| (r - mean).powi(2)).sum::<f64>() / n as f64;
    var.sqrt() * 252_f64.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_strength_value() {
        assert!((SignalStrength::Strong(0.8).value() - 0.8).abs() < 1e-9);
        assert_eq!(SignalStrength::Neutral.value(), 0.0);
    }

    #[test]
    fn momentum_compute_basic() {
        let prices = vec![100.0, 102.0, 104.0, 106.0, 110.0];
        let m = MomentumAlpha::compute(&prices, 4);
        assert!((m - 0.10).abs() < 1e-9);
    }

    #[test]
    fn cross_sectional_rank_length() {
        let mom = vec![0.05, -0.02, 0.10];
        let ranks = MomentumAlpha::cross_sectional_rank(&mom);
        assert_eq!(ranks.len(), 3);
        // Highest momentum (0.10) should get rank 1.0
        assert!((ranks[2] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn z_score_zero_for_flat() {
        let prices = vec![100.0; 20];
        let z = MeanReversionAlpha::z_score(&prices, 20);
        assert_eq!(z, 0.0);
    }

    #[test]
    fn vol_regime_low() {
        // Very flat prices → low volatility
        let prices: Vec<f64> = (0..30).map(|i| 100.0 + i as f64 * 0.001).collect();
        let regime = VolatilityAlpha::vol_regime(&prices);
        assert_eq!(regime, "low");
    }

    #[test]
    fn alpha_engine_compute_returns_three_signals() {
        let engine = AlphaEngine::new();
        let prices: Vec<f64> = (0..50).map(|i| 100.0 + i as f64 * 0.5).collect();
        let signals = engine.compute_signals("AAPL", &prices);
        assert_eq!(signals.len(), 3);
    }

    #[test]
    fn signal_correlation_perfect_negative() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![3.0, 2.0, 1.0];
        let c = AlphaEngine::signal_correlation(&a, &b);
        assert!((c - (-1.0)).abs() < 1e-9);
    }
}
