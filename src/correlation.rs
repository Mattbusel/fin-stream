//! Streaming correlation matrix — online Pearson-r for arbitrary asset pairs.
//!
//! ## Responsibility
//! Maintain an NxN rolling-window correlation matrix across all registered
//! assets. Each `update` call ingests a new price tick for one asset and
//! updates all pairwise statistics involving that asset in O(N) time.
//! Correlation lookups are O(1) via DashMap.
//!
//! ## Algorithm
//! Welford's online algorithm is used to maintain per-asset mean and variance.
//! Cross-covariance between every pair (A, B) is maintained via an online
//! update of `Σ(x_A - μ_A)(x_B - μ_B)`. Pearson r is then:
//!
//! ```text
//! r(A,B) = cov(A,B) / (σ_A * σ_B)
//! ```
//!
//! Values for A == B are defined as `1.0` by convention.
//!
//! ## Concurrency
//! `StreamingCorrelationMatrix` is wrapped in `Arc` and uses `DashMap` for
//! all shared state so multiple WebSocket feed tasks can call `update`
//! concurrently without a global lock.

use dashmap::DashMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

/// A single pairwise Pearson correlation reading.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorrelationPair {
    /// First asset identifier.
    pub asset_a: String,
    /// Second asset identifier.
    pub asset_b: String,
    /// Pearson r in [-1.0, 1.0]. `None` when insufficient data.
    pub pearson_r: f64,
    /// Rolling window size used for this calculation.
    pub window_size: usize,
}

/// Per-asset rolling-window state for Welford's algorithm.
#[derive(Debug, Clone)]
struct AssetState {
    /// Circular buffer of the most recent `window_size` prices.
    window: VecDeque<f64>,
    /// Welford running mean.
    mean: f64,
    /// Welford running sum-of-squares (M2 = Σ(x - mean)²).
    m2: f64,
    /// Number of ticks observed so far (capped at window_size for rolling).
    count: usize,
}

impl AssetState {
    fn new(window_size: usize) -> Self {
        Self {
            window: VecDeque::with_capacity(window_size),
            mean: 0.0,
            m2: 0.0,
            count: 0,
        }
    }

    /// Ingest one new price, evicting the oldest value if at capacity.
    /// Returns `(old_value_evicted, new_value)` for cross-covariance updates.
    fn push(&mut self, price: f64, window_size: usize) -> (Option<f64>, f64) {
        let evicted = if self.window.len() == window_size {
            self.window.pop_front()
        } else {
            None
        };
        self.window.push_back(price);

        // Full recompute of mean/variance from the window to stay numerically stable.
        // For window_size <= 200 this is cheap and avoids drift from incremental updates.
        let n = self.window.len();
        self.count = n;
        if n == 0 {
            self.mean = 0.0;
            self.m2 = 0.0;
        } else {
            let mean = self.window.iter().sum::<f64>() / n as f64;
            let m2 = self.window.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>();
            self.mean = mean;
            self.m2 = m2;
        }
        (evicted, price)
    }

    /// Population standard deviation. Returns 0.0 when count < 2.
    fn std_dev(&self) -> f64 {
        if self.count < 2 {
            return 0.0;
        }
        (self.m2 / self.count as f64).sqrt()
    }
}

/// Per-pair cross-covariance state.
#[derive(Debug, Clone, Default)]
struct PairState {
    /// Σ(x_a - μ_a)(x_b - μ_b) over the shared window.
    cross_sum: f64,
}

/// Canonical key for a pair — always stored with the lexicographically smaller
/// asset ID first so (A,B) and (B,A) share a single entry.
fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Streaming NxN Pearson correlation matrix backed by DashMap.
///
/// ## Example
/// ```rust
/// use fin_stream::correlation::StreamingCorrelationMatrix;
///
/// let matrix = StreamingCorrelationMatrix::new(50);
/// matrix.update("BTC", 30_000.0, 1_000_000);
/// matrix.update("ETH", 2_000.0,  1_000_001);
/// matrix.update("BTC", 30_100.0, 1_000_002);
/// matrix.update("ETH", 2_010.0,  1_000_003);
///
/// if let Some(r) = matrix.correlation("BTC", "ETH") {
///     println!("BTC/ETH r = {r:.4}");
/// }
/// ```
#[derive(Debug, Clone)]
pub struct StreamingCorrelationMatrix {
    /// Rolling window size (number of ticks per asset).
    window_size: usize,
    /// Per-asset state keyed by asset identifier.
    assets: Arc<DashMap<String, AssetState>>,
    /// Pairwise cross-covariance state. Key is the canonical `(min, max)` pair.
    pairs: Arc<DashMap<(String, String), PairState>>,
    /// Latest price per asset (for cross-covariance recalculation).
    latest_prices: Arc<DashMap<String, Vec<f64>>>,
}

impl StreamingCorrelationMatrix {
    /// Create a new matrix with the given rolling window size.
    ///
    /// # Arguments
    /// * `window_size` — number of ticks to retain per asset (minimum 2).
    ///   Values less than 2 are silently clamped to 2.
    pub fn new(window_size: usize) -> Self {
        let window_size = window_size.max(2);
        Self {
            window_size,
            assets: Arc::new(DashMap::new()),
            pairs: Arc::new(DashMap::new()),
            latest_prices: Arc::new(DashMap::new()),
        }
    }

    /// Default constructor with `window_size = 100`.
    pub fn with_default_window() -> Self {
        Self::new(100)
    }

    /// Ingest a new price tick for `asset_id`.
    ///
    /// Updates all pairwise cross-covariance entries involving this asset
    /// in O(N) time, where N is the number of currently registered assets.
    pub fn update(&self, asset_id: &str, price: f64, _timestamp: u64) {
        // Ensure the asset entry exists, then push the new price.
        let mut asset_entry = self
            .assets
            .entry(asset_id.to_string())
            .or_insert_with(|| AssetState::new(self.window_size));
        asset_entry.push(price, self.window_size);
        // Store the latest price window for cross-covariance.
        drop(asset_entry);

        // Snapshot of this asset's current window.
        let window_a: Vec<f64> = {
            let entry = self.assets.get(asset_id);
            match entry {
                Some(e) => e.window.iter().copied().collect(),
                None => return,
            }
        };

        // Update cross-covariance with every other registered asset.
        let other_ids: Vec<String> = self
            .assets
            .iter()
            .filter(|e| e.key() != asset_id)
            .map(|e| e.key().clone())
            .collect();

        for other_id in &other_ids {
            let window_b: Vec<f64> = {
                let entry = self.assets.get(other_id.as_str());
                match entry {
                    Some(e) => e.window.iter().copied().collect(),
                    None => continue,
                }
            };

            // Recompute cross-sum over the overlapping suffix of both windows.
            let key = pair_key(asset_id, other_id);
            let n = window_a.len().min(window_b.len());
            if n < 2 {
                self.pairs.entry(key).or_default();
                continue;
            }

            // Align windows from their ends (most recent ticks line up).
            let a_slice = &window_a[window_a.len() - n..];
            let b_slice = &window_b[window_b.len() - n..];

            let mean_a: f64 = a_slice.iter().sum::<f64>() / n as f64;
            let mean_b: f64 = b_slice.iter().sum::<f64>() / n as f64;
            let cross_sum: f64 = a_slice
                .iter()
                .zip(b_slice.iter())
                .map(|(a, b)| (a - mean_a) * (b - mean_b))
                .sum();

            self.pairs
                .entry(key)
                .or_default()
                .cross_sum = cross_sum;
        }
    }

    /// Look up the Pearson r for the pair `(a, b)`.
    ///
    /// Returns `None` when either asset is unknown or the window is too small
    /// (fewer than 2 ticks for either asset). Returns `1.0` when `a == b`.
    pub fn correlation(&self, a: &str, b: &str) -> Option<f64> {
        if a == b {
            return Some(1.0);
        }

        let state_a = self.assets.get(a)?;
        let state_b = self.assets.get(b)?;

        let n = state_a.count.min(state_b.count);
        if n < 2 {
            return None;
        }

        let std_a = state_a.std_dev();
        let std_b = state_b.std_dev();
        if std_a == 0.0 || std_b == 0.0 {
            return None;
        }

        let key = pair_key(a, b);
        let pair = self.pairs.get(&key)?;
        let cov = pair.cross_sum / n as f64;
        let r = cov / (std_a * std_b);
        // Clamp to [-1, 1] to handle floating-point drift.
        Some(r.clamp(-1.0, 1.0))
    }

    /// Return a full NxN snapshot of all pairwise correlations.
    ///
    /// The map contains entries for every (A, B) pair where A != B and
    /// sufficient data exists. The diagonal (A == A) is not included.
    pub fn full_matrix(&self) -> HashMap<(String, String), f64> {
        let mut result = HashMap::new();
        let asset_ids: Vec<String> = self.assets.iter().map(|e| e.key().clone()).collect();
        for i in 0..asset_ids.len() {
            for j in (i + 1)..asset_ids.len() {
                let a = &asset_ids[i];
                let b = &asset_ids[j];
                if let Some(r) = self.correlation(a, b) {
                    result.insert((a.clone(), b.clone()), r);
                    result.insert((b.clone(), a.clone()), r);
                }
            }
        }
        result
    }

    /// Return the `n` assets most positively correlated with `asset`.
    ///
    /// Results are sorted descending by correlation (highest first).
    /// Returns an empty `Vec` if `asset` is not registered or has no peers.
    pub fn top_correlated(&self, asset: &str, n: usize) -> Vec<(String, f64)> {
        let mut peers = self.all_correlations(asset);
        peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        peers.truncate(n);
        peers
    }

    /// Return the `n` assets most negatively correlated (or least correlated)
    /// with `asset`, sorted ascending by correlation (most decorrelated first).
    ///
    /// Useful for portfolio diversification — assets near r = -1 hedge the
    /// reference asset; assets near r = 0 are orthogonal.
    pub fn top_decorrelated(&self, asset: &str, n: usize) -> Vec<(String, f64)> {
        let mut peers = self.all_correlations(asset);
        peers.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        peers.truncate(n);
        peers
    }

    /// Return every computable pairwise correlation for `asset`.
    fn all_correlations(&self, asset: &str) -> Vec<(String, f64)> {
        self.assets
            .iter()
            .filter(|e| e.key() != asset)
            .filter_map(|e| {
                let other = e.key().clone();
                self.correlation(asset, &other).map(|r| (other, r))
            })
            .collect()
    }

    /// Number of distinct assets currently tracked.
    pub fn asset_count(&self) -> usize {
        self.assets.len()
    }

    /// Returns `true` if `asset_id` is currently registered.
    pub fn contains_asset(&self, asset_id: &str) -> bool {
        self.assets.contains_key(asset_id)
    }

    /// Returns the configured rolling window size.
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Returns the tick count buffered for `asset_id`, or `None` if unknown.
    pub fn tick_count(&self, asset_id: &str) -> Option<usize> {
        self.assets.get(asset_id).map(|s| s.count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed perfectly correlated series and check r ≈ 1.0.
    #[test]
    fn perfect_positive_correlation() {
        let m = StreamingCorrelationMatrix::new(10);
        for i in 0..10u64 {
            let v = i as f64 * 10.0;
            m.update("A", v, i);
            m.update("B", v * 2.0, i);
        }
        let r = m.correlation("A", "B").expect("should have correlation");
        assert!((r - 1.0).abs() < 1e-9, "expected ~1.0, got {r}");
    }

    /// Feed perfectly anti-correlated series and check r ≈ -1.0.
    #[test]
    fn perfect_negative_correlation() {
        let m = StreamingCorrelationMatrix::new(10);
        for i in 0..10u64 {
            let v = i as f64;
            m.update("A", v, i);
            m.update("B", 100.0 - v, i);
        }
        let r = m.correlation("A", "B").expect("should have correlation");
        assert!((r + 1.0).abs() < 1e-9, "expected ~-1.0, got {r}");
    }

    /// r(X, X) must be 1.0 by definition.
    #[test]
    fn self_correlation_is_one() {
        let m = StreamingCorrelationMatrix::new(10);
        m.update("X", 1.0, 0);
        assert_eq!(m.correlation("X", "X"), Some(1.0));
    }

    /// Unknown asset pair returns None.
    #[test]
    fn unknown_asset_returns_none() {
        let m = StreamingCorrelationMatrix::new(10);
        assert_eq!(m.correlation("MISSING", "ALSO_MISSING"), None);
    }

    /// Constant price series → std_dev == 0 → None (degenerate).
    #[test]
    fn constant_series_returns_none() {
        let m = StreamingCorrelationMatrix::new(5);
        for i in 0..5u64 {
            m.update("FLAT", 100.0, i);
            m.update("MOVING", i as f64, i);
        }
        assert_eq!(m.correlation("FLAT", "MOVING"), None);
    }

    #[test]
    fn full_matrix_contains_symmetric_entries() {
        let m = StreamingCorrelationMatrix::new(5);
        for i in 0..5u64 {
            m.update("BTC", i as f64, i);
            m.update("ETH", i as f64 * 1.5, i);
        }
        let matrix = m.full_matrix();
        let r_ab = matrix.get(&("BTC".to_string(), "ETH".to_string()));
        let r_ba = matrix.get(&("ETH".to_string(), "BTC".to_string()));
        assert!(r_ab.is_some() && r_ba.is_some());
        assert_eq!(r_ab, r_ba);
    }

    #[test]
    fn top_correlated_and_decorrelated_sorted() {
        let m = StreamingCorrelationMatrix::new(20);
        for i in 0..20u64 {
            let v = i as f64;
            m.update("BASE", v, i);
            m.update("CORR", v * 2.0, i);         // r ≈ +1
            m.update("ANTI", 100.0 - v * 2.0, i); // r ≈ -1
            m.update("RAND", (v * 7.0) % 13.0, i); // partially correlated
        }
        let top = m.top_correlated("BASE", 2);
        let bot = m.top_decorrelated("BASE", 2);
        // top should be sorted descending
        if top.len() >= 2 {
            assert!(top[0].1 >= top[1].1);
        }
        // bot should be sorted ascending
        if bot.len() >= 2 {
            assert!(bot[0].1 <= bot[1].1);
        }
    }

    #[test]
    fn window_size_getter() {
        let m = StreamingCorrelationMatrix::new(42);
        assert_eq!(m.window_size(), 42);
    }

    #[test]
    fn window_size_minimum_clamped_to_two() {
        let m = StreamingCorrelationMatrix::new(0);
        assert_eq!(m.window_size(), 2);
    }

    #[test]
    fn tick_count_tracking() {
        let m = StreamingCorrelationMatrix::new(5);
        m.update("A", 1.0, 0);
        m.update("A", 2.0, 1);
        m.update("A", 3.0, 2);
        assert_eq!(m.tick_count("A"), Some(3));
        // Overflow the window
        m.update("A", 4.0, 3);
        m.update("A", 5.0, 4);
        m.update("A", 6.0, 5);
        assert_eq!(m.tick_count("A"), Some(5)); // capped at window_size
    }
}
