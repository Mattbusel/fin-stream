//! # Module: cross_asset — Cross-Asset Correlation Streaming
//!
//! ## Responsibility
//! Real-time streaming Pearson correlation for asset pairs and universes,
//! with correlation breakdown detection using Welford online statistics.
//!
//! ## NOT Responsible For
//! - Historical data storage or retrieval
//! - Order routing or execution

use std::collections::HashMap;

// ─────────────────────────────────────────
//  AssetPair
// ─────────────────────────────────────────

/// An ordered pair of asset identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssetPair {
    /// First asset symbol.
    pub asset_a: String,
    /// Second asset symbol.
    pub asset_b: String,
}

impl AssetPair {
    /// Create a new asset pair.
    pub fn new(a: impl Into<String>, b: impl Into<String>) -> Self {
        Self { asset_a: a.into(), asset_b: b.into() }
    }
}

// ─────────────────────────────────────────
//  OnlineCorrelation — Welford-based Pearson correlation
// ─────────────────────────────────────────

/// Online Welford-based Pearson correlation estimator for a pair of series.
///
/// Processes observations one at a time with O(1) update cost.
/// Returns `None` from `correlation()` until at least 2 samples have been seen,
/// or when the standard deviation of either series is zero.
///
/// # Example
/// ```rust
/// use fin_stream::cross_asset::OnlineCorrelation;
///
/// let mut oc = OnlineCorrelation::new();
/// oc.update(1.0, 1.0);
/// oc.update(2.0, 2.0);
/// oc.update(3.0, 3.0);
/// let cor = oc.correlation().unwrap();
/// assert!((cor - 1.0).abs() < 1e-9, "cor={cor}");
/// ```
#[derive(Debug, Default, Clone)]
pub struct OnlineCorrelation {
    n: usize,
    mean_x: f64,
    mean_y: f64,
    m2_x: f64,
    m2_y: f64,
    c_xy: f64,  // co-moment: Σ(xᵢ - x̄ₙ)(yᵢ - ȳₙ)
}

impl OnlineCorrelation {
    /// Create a new online Pearson correlation estimator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Incorporate a new (x, y) observation using Welford's online algorithm.
    pub fn update(&mut self, x: f64, y: f64) {
        self.n += 1;
        let n = self.n as f64;
        let dx = x - self.mean_x;
        let dy = y - self.mean_y;
        self.mean_x += dx / n;
        self.mean_y += dy / n;
        // Update co-moment using new means
        let dx2 = x - self.mean_x;
        self.c_xy += dx * (y - self.mean_y);
        self.m2_x += dx * dx2;
        self.m2_y += dy * (y - self.mean_y);
    }

    /// Return the Pearson correlation, or `None` if fewer than 2 samples or zero variance.
    pub fn correlation(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let var_x = self.m2_x;
        let var_y = self.m2_y;
        if var_x == 0.0 || var_y == 0.0 {
            return None;
        }
        let cor = self.c_xy / (var_x.sqrt() * var_y.sqrt());
        // Clamp to [-1, 1] to guard against floating-point drift
        Some(cor.max(-1.0).min(1.0))
    }

    /// Return the number of observations processed.
    pub fn n_samples(&self) -> usize {
        self.n
    }

    /// Reset the estimator.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// ─────────────────────────────────────────
//  CorrelationMatrix — NxN online correlation tracker
// ─────────────────────────────────────────

/// Tracks pairwise online Pearson correlations for a dynamic universe of assets.
///
/// Observations are submitted per-symbol; when multiple symbols receive a new
/// observation at the same timestamp the matrix is updated for all pairs that
/// have seen at least one overlapping observation.
///
/// Symbols are tracked in alphabetical order for stable matrix output.
///
/// # Example
/// ```rust
/// use fin_stream::cross_asset::CorrelationMatrix;
///
/// let mut cm = CorrelationMatrix::new();
/// for i in 0..5u64 {
///     cm.add_observation("SPY", i as f64 * 0.01, i * 1000);
///     cm.add_observation("QQQ", i as f64 * 0.012, i * 1000);
/// }
/// let cor = cm.correlation("SPY", "QQQ");
/// assert!(cor.is_some());
/// ```
#[derive(Debug, Default)]
pub struct CorrelationMatrix {
    /// Latest value per symbol (used to pair with other symbols on next tick).
    latest: HashMap<String, f64>,
    /// Pairwise correlators, keyed by (smaller_symbol, larger_symbol).
    pairs: HashMap<(String, String), OnlineCorrelation>,
}

impl CorrelationMatrix {
    /// Create an empty correlation matrix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an observation for `symbol` at `timestamp_ms`.
    ///
    /// When a new value arrives for `symbol`, it is correlated against the
    /// most recent stored value for every other known symbol.
    pub fn add_observation(&mut self, symbol: &str, value: f64, _timestamp_ms: u64) {
        // Pair the new value with all existing symbols
        let others: Vec<(String, f64)> = self
            .latest
            .iter()
            .filter(|(s, _)| s.as_str() != symbol)
            .map(|(s, v)| (s.clone(), *v))
            .collect();

        for (other_sym, other_val) in others {
            let key = Self::pair_key(symbol, &other_sym);
            self.pairs
                .entry(key)
                .or_insert_with(OnlineCorrelation::new)
                .update(value, other_val);
        }

        self.latest.insert(symbol.to_owned(), value);
    }

    /// Return the Pearson correlation between two symbols, or `None` if unavailable.
    pub fn correlation(&self, a: &str, b: &str) -> Option<f64> {
        let key = Self::pair_key(a, b);
        self.pairs.get(&key)?.correlation()
    }

    /// Return the full NxN correlation matrix with symbols sorted alphabetically.
    ///
    /// Diagonal entries are `1.0`. Off-diagonal entries are `0.0` when the pair
    /// has insufficient data.
    pub fn correlation_matrix(&self) -> Vec<Vec<f64>> {
        let mut symbols: Vec<String> = self.latest.keys().cloned().collect();
        symbols.sort();
        let n = symbols.len();
        let mut mat = vec![vec![0.0_f64; n]; n];
        for i in 0..n {
            mat[i][i] = 1.0;
            for j in (i + 1)..n {
                let cor = self.correlation(&symbols[i], &symbols[j]).unwrap_or(0.0);
                mat[i][j] = cor;
                mat[j][i] = cor;
            }
        }
        mat
    }

    /// Return the pair with the highest correlation and its value.
    pub fn max_correlation_pair(&self) -> Option<(AssetPair, f64)> {
        self.pairs
            .iter()
            .filter_map(|((a, b), oc)| {
                oc.correlation().map(|c| (AssetPair::new(a, b), c))
            })
            .max_by(|(_, c1), (_, c2)| c1.partial_cmp(c2).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Return the pair with the lowest (most negative) correlation and its value.
    pub fn min_correlation_pair(&self) -> Option<(AssetPair, f64)> {
        self.pairs
            .iter()
            .filter_map(|((a, b), oc)| {
                oc.correlation().map(|c| (AssetPair::new(a, b), c))
            })
            .min_by(|(_, c1), (_, c2)| c1.partial_cmp(c2).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Canonical sorted pair key so (A,B) and (B,A) map to the same entry.
    fn pair_key(a: &str, b: &str) -> (String, String) {
        if a <= b {
            (a.to_owned(), b.to_owned())
        } else {
            (b.to_owned(), a.to_owned())
        }
    }
}

// ─────────────────────────────────────────
//  CorrelationBreakdown — detect regime shift in pairwise correlation
// ─────────────────────────────────────────

/// Detects when a streaming correlation deviates more than 2 standard deviations
/// from its historical mean (Welford online mean and variance).
///
/// # Example
/// ```rust
/// use fin_stream::cross_asset::CorrelationBreakdown;
///
/// let mut cb = CorrelationBreakdown::new();
/// for c in &[0.8, 0.82, 0.79, 0.81, 0.80] {
///     cb.update(*c);
/// }
/// // Stable series: no breakdown
/// assert!(!cb.is_breakdown());
/// cb.update(-0.5); // large deviation
/// assert!(cb.is_breakdown());
/// ```
#[derive(Debug, Default)]
pub struct CorrelationBreakdown {
    n: u64,
    mean: f64,
    m2: f64,
    last: f64,
}

impl CorrelationBreakdown {
    /// Create a new breakdown detector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with a new correlation observation using Welford's algorithm.
    pub fn update(&mut self, correlation: f64) {
        self.n += 1;
        self.last = correlation;
        let delta = correlation - self.mean;
        self.mean += delta / self.n as f64;
        let delta2 = correlation - self.mean;
        self.m2 += delta * delta2;
    }

    /// Return `true` when the last observation deviates more than 2σ from the mean.
    ///
    /// Returns `false` when fewer than 3 observations have been seen (insufficient
    /// history to estimate variance reliably).
    pub fn is_breakdown(&self) -> bool {
        if self.n < 3 {
            return false;
        }
        let variance = self.m2 / (self.n - 1) as f64;
        let std_dev = variance.sqrt();
        if std_dev == 0.0 {
            return false;
        }
        let z = (self.last - self.mean).abs() / std_dev;
        z > 2.0
    }

    /// Return the running mean of correlations seen so far.
    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// Return the number of observations.
    pub fn n_samples(&self) -> u64 {
        self.n
    }

    /// Reset the detector.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AssetPair ────────────────────────────────────────────────────────────

    #[test]
    fn test_asset_pair_equality() {
        let p = AssetPair::new("SPY", "QQQ");
        assert_eq!(p.asset_a, "SPY");
        assert_eq!(p.asset_b, "QQQ");
    }

    // ── OnlineCorrelation ────────────────────────────────────────────────────

    #[test]
    fn test_online_correlation_perfect_positive() {
        let mut oc = OnlineCorrelation::new();
        for i in 0..10 {
            oc.update(i as f64, i as f64 * 2.0);
        }
        let cor = oc.correlation().unwrap();
        assert!((cor - 1.0).abs() < 1e-9, "cor={cor}");
    }

    #[test]
    fn test_online_correlation_perfect_negative() {
        let mut oc = OnlineCorrelation::new();
        for i in 0..10 {
            oc.update(i as f64, -(i as f64));
        }
        let cor = oc.correlation().unwrap();
        assert!((cor + 1.0).abs() < 1e-9, "cor={cor}");
    }

    #[test]
    fn test_online_correlation_insufficient_data() {
        let mut oc = OnlineCorrelation::new();
        oc.update(1.0, 2.0);
        assert!(oc.correlation().is_none());
    }

    #[test]
    fn test_online_correlation_zero_variance_returns_none() {
        let mut oc = OnlineCorrelation::new();
        oc.update(1.0, 5.0);
        oc.update(1.0, 6.0);
        oc.update(1.0, 7.0);
        // x is constant → zero variance → None
        assert!(oc.correlation().is_none());
    }

    #[test]
    fn test_online_correlation_n_samples() {
        let mut oc = OnlineCorrelation::new();
        oc.update(1.0, 2.0);
        oc.update(3.0, 4.0);
        assert_eq!(oc.n_samples(), 2);
    }

    #[test]
    fn test_online_correlation_reset() {
        let mut oc = OnlineCorrelation::new();
        oc.update(1.0, 2.0);
        oc.update(2.0, 4.0);
        oc.reset();
        assert_eq!(oc.n_samples(), 0);
        assert!(oc.correlation().is_none());
    }

    // ── CorrelationMatrix ────────────────────────────────────────────────────

    #[test]
    fn test_correlation_matrix_two_assets() {
        let mut cm = CorrelationMatrix::new();
        for i in 0..10u64 {
            cm.add_observation("SPY", i as f64, i * 1000);
            cm.add_observation("QQQ", i as f64 * 1.1, i * 1000);
        }
        let cor = cm.correlation("SPY", "QQQ");
        assert!(cor.is_some(), "correlation should be available");
        let c = cor.unwrap();
        assert!((c - 1.0).abs() < 1e-6, "perfectly correlated assets: cor={c}");
    }

    #[test]
    fn test_correlation_matrix_nxn_diagonal_ones() {
        let mut cm = CorrelationMatrix::new();
        for i in 0..5u64 {
            cm.add_observation("A", i as f64, i);
            cm.add_observation("B", i as f64 * 2.0, i);
            cm.add_observation("C", i as f64 * 3.0, i);
        }
        let mat = cm.correlation_matrix();
        let n = mat.len();
        for i in 0..n {
            assert_eq!(mat[i][i], 1.0, "diagonal[{i}] should be 1.0");
        }
    }

    #[test]
    fn test_correlation_matrix_symmetry() {
        let mut cm = CorrelationMatrix::new();
        for i in 0..8u64 {
            cm.add_observation("X", i as f64, i);
            cm.add_observation("Y", (i as f64 * 0.5).sin(), i);
        }
        let mat = cm.correlation_matrix();
        let n = mat.len();
        for i in 0..n {
            for j in 0..n {
                assert!(
                    (mat[i][j] - mat[j][i]).abs() < 1e-12,
                    "matrix not symmetric at ({i},{j})"
                );
            }
        }
    }

    #[test]
    fn test_max_min_correlation_pair() {
        let mut cm = CorrelationMatrix::new();
        for i in 0..10u64 {
            let x = i as f64;
            cm.add_observation("A", x, i);       // trending up
            cm.add_observation("B", x * 2.0, i); // trending up faster (high cor with A)
            cm.add_observation("C", -x, i);      // trending down (neg cor with A)
        }
        let max = cm.max_correlation_pair();
        let min = cm.min_correlation_pair();
        assert!(max.is_some());
        assert!(min.is_some());
        let (_, max_c) = max.unwrap();
        let (_, min_c) = min.unwrap();
        assert!(max_c >= min_c, "max_c={max_c} should be >= min_c={min_c}");
    }

    #[test]
    fn test_correlation_unknown_pair_returns_none() {
        let cm = CorrelationMatrix::new();
        assert!(cm.correlation("FOO", "BAR").is_none());
    }

    // ── CorrelationBreakdown ─────────────────────────────────────────────────

    #[test]
    fn test_correlation_breakdown_stable_no_alarm() {
        let mut cb = CorrelationBreakdown::new();
        for _ in 0..20 {
            cb.update(0.80);
        }
        assert!(!cb.is_breakdown(), "stable series should not trigger breakdown");
    }

    #[test]
    fn test_correlation_breakdown_large_deviation_triggers() {
        let mut cb = CorrelationBreakdown::new();
        for _ in 0..20 {
            cb.update(0.80);
        }
        cb.update(-0.80); // extreme deviation
        assert!(cb.is_breakdown(), "large deviation should trigger breakdown");
    }

    #[test]
    fn test_correlation_breakdown_insufficient_data() {
        let mut cb = CorrelationBreakdown::new();
        cb.update(0.5);
        cb.update(-0.9); // extreme, but < 3 samples
        assert!(!cb.is_breakdown(), "< 3 samples → no breakdown");
    }

    #[test]
    fn test_correlation_breakdown_reset() {
        let mut cb = CorrelationBreakdown::new();
        for _ in 0..5 {
            cb.update(0.8);
        }
        cb.update(-0.8);
        assert!(cb.is_breakdown());
        cb.reset();
        assert_eq!(cb.n_samples(), 0);
        assert!(!cb.is_breakdown());
    }

    #[test]
    fn test_correlation_breakdown_mean_tracks_correctly() {
        let mut cb = CorrelationBreakdown::new();
        cb.update(0.4);
        cb.update(0.6);
        // mean should be ~0.5
        assert!((cb.mean() - 0.5).abs() < 1e-9, "mean={}", cb.mean());
    }
}
