//! # Module: network
//!
//! ## Responsibility
//! Track price correlations as a weighted graph (nodes = assets, edges = rolling
//! Pearson correlation), detect correlation regime changes, identify market-leading
//! assets via centrality, and flag contagion cascades.
//!
//! ## Components
//! - `CorrelationGraph`: maintains pairwise rolling correlations for N assets.
//! - `CentralityScores`: degree centrality and weighted centrality per asset.
//! - `RegimeChangeDetector`: detects when graph topology shifts significantly.
//! - `ContagionDetector`: identifies cascade failures in correlated clusters.
//!
//! ## NOT Responsible For
//! - Full graph persistence
//! - Non-Pearson correlation measures (Spearman, etc.)

use crate::error::StreamError;
use std::collections::HashMap;

// ─── rolling correlation ──────────────────────────────────────────────────────

/// Welford online algorithm state for rolling Pearson correlation between two series.
#[derive(Debug, Clone, Default)]
struct PairStats {
    n: usize,
    mean_x: f64,
    mean_y: f64,
    m2_x: f64,
    m2_y: f64,
    c_xy: f64, // running covariance accumulator
}

impl PairStats {
    fn update(&mut self, x: f64, y: f64) {
        self.n += 1;
        let dx = x - self.mean_x;
        let dy = y - self.mean_y;
        self.mean_x += dx / self.n as f64;
        self.mean_y += dy / self.n as f64;
        let dx2 = x - self.mean_x;
        let dy2 = y - self.mean_y;
        self.m2_x += dx * dx2;
        self.m2_y += dy * dy2;
        self.c_xy += dx * dy2;
    }

    fn correlation(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let var_x = self.m2_x / (self.n as f64 - 1.0);
        let var_y = self.m2_y / (self.n as f64 - 1.0);
        let denom = (var_x * var_y).sqrt();
        if denom < f64::EPSILON {
            return None;
        }
        let cov = self.c_xy / (self.n as f64 - 1.0);
        Some((cov / denom).clamp(-1.0, 1.0))
    }
}

// ─── correlation graph ────────────────────────────────────────────────────────

/// An edge in the correlation graph.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorrelationEdge {
    /// First asset symbol.
    pub asset_a: String,
    /// Second asset symbol.
    pub asset_b: String,
    /// Current Pearson correlation [-1, 1].
    pub correlation: f64,
    /// Number of observations used.
    pub n: usize,
}

/// Rolling correlation graph for a set of assets.
///
/// Each time `update` is called with a full price snapshot (all assets for
/// one bar/tick), pairwise correlations are updated via Welford's algorithm.
pub struct CorrelationGraph {
    assets: Vec<String>,
    /// Pair key = (min_idx, max_idx) for canonical ordering.
    pairs: HashMap<(usize, usize), PairStats>,
    /// Minimum correlation magnitude to include an edge.
    min_correlation: f64,
}

impl CorrelationGraph {
    /// Create a new correlation graph.
    ///
    /// - `assets`: ordered list of asset symbols.
    /// - `min_correlation`: edges with |correlation| below this are considered absent.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if fewer than 2 assets are provided.
    pub fn new(
        assets: Vec<String>,
        min_correlation: f64,
    ) -> Result<Self, StreamError> {
        if assets.len() < 2 {
            return Err(StreamError::InvalidInput(
                "CorrelationGraph requires at least 2 assets".to_owned(),
            ));
        }
        Ok(Self { assets, pairs: HashMap::new(), min_correlation })
    }

    /// Update all pairwise correlations with a new price snapshot.
    ///
    /// `prices` must be in the same order as `assets` passed to `new`.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if the prices slice length does not match
    /// the number of assets, or any price is non-positive.
    pub fn update(&mut self, prices: &[f64]) -> Result<(), StreamError> {
        if prices.len() != self.assets.len() {
            return Err(StreamError::InvalidInput(format!(
                "Expected {} prices, got {}",
                self.assets.len(),
                prices.len()
            )));
        }
        for &p in prices {
            if p <= 0.0 {
                return Err(StreamError::InvalidInput(
                    "All prices must be positive".to_owned(),
                ));
            }
        }

        let n = prices.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let key = (i, j);
                let stats = self.pairs.entry(key).or_default();
                // Use log-returns if we have a previous price, else raw price
                stats.update(prices[i].ln(), prices[j].ln());
            }
        }
        Ok(())
    }

    /// Return all edges whose |correlation| exceeds `min_correlation`.
    pub fn edges(&self) -> Vec<CorrelationEdge> {
        self.pairs
            .iter()
            .filter_map(|(&(i, j), stats)| {
                let corr = stats.correlation()?;
                if corr.abs() < self.min_correlation {
                    return None;
                }
                Some(CorrelationEdge {
                    asset_a: self.assets[i].clone(),
                    asset_b: self.assets[j].clone(),
                    correlation: corr,
                    n: stats.n,
                })
            })
            .collect()
    }

    /// Return the correlation between two assets, or `None` if not enough data.
    pub fn get_correlation(&self, asset_a: &str, asset_b: &str) -> Option<f64> {
        let i = self.assets.iter().position(|a| a == asset_a)?;
        let j = self.assets.iter().position(|a| a == asset_b)?;
        let (lo, hi) = if i < j { (i, j) } else { (j, i) };
        self.pairs.get(&(lo, hi))?.correlation()
    }

    /// Return all asset names.
    pub fn assets(&self) -> &[String] {
        &self.assets
    }
}

// ─── centrality scores ────────────────────────────────────────────────────────

/// Centrality scores for each asset in the correlation graph.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CentralityScores {
    /// Asset symbol → (degree_centrality, weighted_centrality).
    pub scores: HashMap<String, (f64, f64)>,
    /// Asset with the highest weighted centrality ("market leader").
    pub leader: String,
}

impl CentralityScores {
    /// Compute centrality from a set of edges.
    ///
    /// - **Degree centrality**: fraction of other assets this asset is correlated with.
    /// - **Weighted centrality**: sum of |correlation| for all edges of this asset.
    pub fn compute(assets: &[String], edges: &[CorrelationEdge]) -> Self {
        let n = assets.len();
        let mut degree: HashMap<String, usize> = HashMap::new();
        let mut weighted: HashMap<String, f64> = HashMap::new();

        for asset in assets {
            degree.insert(asset.clone(), 0);
            weighted.insert(asset.clone(), 0.0);
        }

        for edge in edges {
            *degree.entry(edge.asset_a.clone()).or_insert(0) += 1;
            *degree.entry(edge.asset_b.clone()).or_insert(0) += 1;
            *weighted.entry(edge.asset_a.clone()).or_insert(0.0) += edge.correlation.abs();
            *weighted.entry(edge.asset_b.clone()).or_insert(0.0) += edge.correlation.abs();
        }

        let max_degree = (n - 1).max(1) as f64;
        let mut scores = HashMap::new();

        for asset in assets {
            let deg = *degree.get(asset).unwrap_or(&0);
            let wt = *weighted.get(asset).unwrap_or(&0.0);
            scores.insert(asset.clone(), (deg as f64 / max_degree, wt));
        }

        let leader = scores
            .iter()
            .max_by(|a, b| a.1.1.partial_cmp(&b.1.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, _)| k.clone())
            .unwrap_or_default();

        Self { scores, leader }
    }
}

// ─── regime change detector ───────────────────────────────────────────────────

/// Detects significant changes in graph topology (correlation regime changes).
///
/// Tracks the mean absolute correlation across all edges and emits a regime
/// change signal when it shifts by more than `threshold` relative to its
/// exponential moving average.
pub struct RegimeChangeDetector {
    threshold: f64,
    alpha: f64,
    ema_corr: Option<f64>,
}

/// Result of a regime change check.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegimeCheckResult {
    /// Current mean absolute correlation across all graph edges.
    pub mean_abs_correlation: f64,
    /// EMA of mean absolute correlation.
    pub ema_correlation: f64,
    /// Whether a regime change was detected.
    pub regime_change: bool,
    /// Magnitude of the deviation from EMA (positive = more correlated than usual).
    pub deviation: f64,
}

impl RegimeChangeDetector {
    /// Create a new regime change detector.
    ///
    /// - `threshold`: minimum |deviation| from EMA to declare a regime change.
    /// - `ema_span`: number of observations for the EMA (α = 2/(span+1)).
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if threshold ≤ 0 or span < 2.
    pub fn new(threshold: f64, ema_span: usize) -> Result<Self, StreamError> {
        if threshold <= 0.0 {
            return Err(StreamError::InvalidInput("threshold must be positive".to_owned()));
        }
        if ema_span < 2 {
            return Err(StreamError::InvalidInput("ema_span must be at least 2".to_owned()));
        }
        let alpha = 2.0 / (ema_span as f64 + 1.0);
        Ok(Self { threshold, alpha, ema_corr: None })
    }

    /// Check for a regime change given the current graph edges.
    ///
    /// Returns `None` until the first update.
    pub fn check(&mut self, edges: &[CorrelationEdge]) -> Option<RegimeCheckResult> {
        if edges.is_empty() {
            return None;
        }
        let mean_abs =
            edges.iter().map(|e| e.correlation.abs()).sum::<f64>() / edges.len() as f64;

        let ema = match self.ema_corr {
            None => {
                self.ema_corr = Some(mean_abs);
                mean_abs
            }
            Some(prev) => {
                let new_ema = self.alpha * mean_abs + (1.0 - self.alpha) * prev;
                self.ema_corr = Some(new_ema);
                new_ema
            }
        };

        let deviation = mean_abs - ema;
        Some(RegimeCheckResult {
            mean_abs_correlation: mean_abs,
            ema_correlation: ema,
            regime_change: deviation.abs() > self.threshold,
            deviation,
        })
    }
}

// ─── contagion detector ───────────────────────────────────────────────────────

/// A detected contagion event: a cascade of correlated price shocks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContagionEvent {
    /// Assets involved in the contagion cluster.
    pub affected_assets: Vec<String>,
    /// The triggering asset (largest price shock).
    pub trigger: String,
    /// Mean absolute shock across the affected cluster.
    pub mean_shock: f64,
    /// Contagion score: mean_shock × mean_abs_correlation.
    pub contagion_score: f64,
}

/// Contagion detector: identifies assets whose price moves simultaneously
/// and are strongly correlated — a signature of cascade failures.
///
/// On each call to `check`, it looks for clusters of assets where:
/// 1. Each member's absolute log-return exceeds `shock_threshold`.
/// 2. Members are pairwise correlated above `min_correlation` in the graph.
pub struct ContagionDetector {
    shock_threshold: f64,
    min_correlation: f64,
}

impl ContagionDetector {
    /// Create a new contagion detector.
    ///
    /// - `shock_threshold`: minimum |log-return| to consider an asset shocked.
    /// - `min_correlation`: minimum correlation for two assets to be in the same cluster.
    ///
    /// # Errors
    /// `StreamError::InvalidInput` if either parameter is non-positive.
    pub fn new(shock_threshold: f64, min_correlation: f64) -> Result<Self, StreamError> {
        if shock_threshold <= 0.0 || min_correlation <= 0.0 {
            return Err(StreamError::InvalidInput(
                "shock_threshold and min_correlation must be positive".to_owned(),
            ));
        }
        Ok(Self { shock_threshold, min_correlation })
    }

    /// Check for contagion given current log-returns and the correlation graph.
    ///
    /// `returns` maps asset symbol → log-return for the current period.
    ///
    /// Returns a list of contagion events (may be empty).
    pub fn check(
        &self,
        returns: &HashMap<String, f64>,
        graph: &CorrelationGraph,
    ) -> Vec<ContagionEvent> {
        // Identify shocked assets
        let shocked: Vec<&String> = returns
            .iter()
            .filter(|(_, &r)| r.abs() >= self.shock_threshold)
            .map(|(s, _)| s)
            .collect();

        if shocked.len() < 2 {
            return Vec::new();
        }

        // Build clusters: BFS/union on assets connected by high correlation
        let mut visited: HashMap<&String, bool> = HashMap::new();
        let mut events = Vec::new();

        for start in &shocked {
            if *visited.get(*start).unwrap_or(&false) {
                continue;
            }
            let mut cluster = vec![(*start).clone()];
            visited.insert(start, true);

            for other in &shocked {
                if *other == *start || *visited.get(*other).unwrap_or(&false) {
                    continue;
                }
                if let Some(corr) = graph.get_correlation(start, other) {
                    if corr.abs() >= self.min_correlation {
                        cluster.push((*other).clone());
                        visited.insert(other, true);
                    }
                }
            }

            if cluster.len() < 2 {
                continue;
            }

            // Find trigger: asset with largest |return|
            let trigger = cluster
                .iter()
                .max_by(|a, b| {
                    returns.get(*a).unwrap_or(&0.0).abs()
                        .partial_cmp(&returns.get(*b).unwrap_or(&0.0).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .cloned()
                .unwrap_or_default();

            let mean_shock =
                cluster.iter().map(|a| returns.get(a).unwrap_or(&0.0).abs()).sum::<f64>()
                    / cluster.len() as f64;

            // Mean correlation within cluster
            let edges = graph.edges();
            let cluster_corrs: Vec<f64> = edges
                .iter()
                .filter(|e| {
                    cluster.contains(&e.asset_a) && cluster.contains(&e.asset_b)
                })
                .map(|e| e.correlation.abs())
                .collect();
            let mean_corr = if cluster_corrs.is_empty() {
                self.min_correlation
            } else {
                cluster_corrs.iter().sum::<f64>() / cluster_corrs.len() as f64
            };

            events.push(ContagionEvent {
                affected_assets: cluster,
                trigger,
                mean_shock,
                contagion_score: mean_shock * mean_corr,
            });
        }

        events
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn asset_list() -> Vec<String> {
        vec!["BTC".to_owned(), "ETH".to_owned(), "SOL".to_owned()]
    }

    fn feed_prices(graph: &mut CorrelationGraph, prices: &[&[f64]]) {
        for row in prices {
            graph.update(row).unwrap();
        }
    }

    #[test]
    fn test_graph_needs_at_least_two_assets() {
        assert!(CorrelationGraph::new(vec!["BTC".to_owned()], 0.5).is_err());
    }

    #[test]
    fn test_graph_wrong_price_count_errors() {
        let mut g = CorrelationGraph::new(asset_list(), 0.0).unwrap();
        assert!(g.update(&[100.0, 200.0]).is_err()); // 2 prices for 3 assets
    }

    #[test]
    fn test_correlation_positive_after_aligned_moves() {
        let mut g = CorrelationGraph::new(asset_list(), 0.0).unwrap();
        // All prices move up together → high positive correlation
        let mut p = [100.0_f64, 200.0, 50.0];
        for _ in 0..50 {
            p[0] *= 1.001;
            p[1] *= 1.001;
            p[2] *= 1.001;
            g.update(&p).unwrap();
        }
        let corr = g.get_correlation("BTC", "ETH").unwrap();
        assert!(corr > 0.9, "expected high positive correlation, got {corr:.4}");
    }

    #[test]
    fn test_edges_filtered_by_min_correlation() {
        let mut g = CorrelationGraph::new(asset_list(), 0.9).unwrap();
        // Feed identical moves → correlation = 1.0
        let mut p = [100.0_f64, 200.0, 50.0];
        for _ in 0..50 {
            p[0] *= 1.001;
            p[1] *= 1.001;
            p[2] *= 1.001;
            g.update(&p).unwrap();
        }
        let edges = g.edges();
        assert!(!edges.is_empty());
        for e in &edges {
            assert!(e.correlation.abs() >= 0.9);
        }
    }

    #[test]
    fn test_centrality_leader_identified() {
        let assets = asset_list();
        let edges = vec![
            CorrelationEdge { asset_a: "BTC".to_owned(), asset_b: "ETH".to_owned(), correlation: 0.95, n: 50 },
            CorrelationEdge { asset_a: "BTC".to_owned(), asset_b: "SOL".to_owned(), correlation: 0.90, n: 50 },
            CorrelationEdge { asset_a: "ETH".to_owned(), asset_b: "SOL".to_owned(), correlation: 0.70, n: 50 },
        ];
        let centrality = CentralityScores::compute(&assets, &edges);
        // BTC has highest total weighted correlation
        assert_eq!(centrality.leader, "BTC");
    }

    #[test]
    fn test_regime_detector_no_change_stable_correlations() {
        let mut det = RegimeChangeDetector::new(0.3, 10).unwrap();
        let edges = vec![
            CorrelationEdge { asset_a: "A".to_owned(), asset_b: "B".to_owned(), correlation: 0.8, n: 50 },
        ];
        let mut last = None;
        for _ in 0..20 {
            last = det.check(&edges);
        }
        let result = last.unwrap();
        // Stable input → deviation → 0 → no regime change
        assert!(!result.regime_change);
    }

    #[test]
    fn test_regime_detector_invalid_params() {
        assert!(RegimeChangeDetector::new(-0.1, 10).is_err());
        assert!(RegimeChangeDetector::new(0.1, 1).is_err());
    }

    #[test]
    fn test_contagion_detected_in_correlated_shock() {
        let mut g = CorrelationGraph::new(asset_list(), 0.0).unwrap();
        // Make BTC and ETH highly correlated
        let mut p = [100.0_f64, 200.0, 50.0];
        for _ in 0..50 {
            p[0] *= 1.001;
            p[1] *= 1.001;
            p[2] *= 1.001;
            g.update(&p).unwrap();
        }

        let det = ContagionDetector::new(0.05, 0.5).unwrap();
        let mut returns = HashMap::new();
        returns.insert("BTC".to_owned(), -0.10); // large shock
        returns.insert("ETH".to_owned(), -0.09); // large shock
        returns.insert("SOL".to_owned(), -0.01); // small, not shocked

        let events = det.check(&returns, &g);
        // BTC and ETH should form a contagion cluster
        assert!(!events.is_empty(), "expected contagion event");
        assert!(events[0].affected_assets.contains(&"BTC".to_owned()));
        assert!(events[0].affected_assets.contains(&"ETH".to_owned()));
    }

    #[test]
    fn test_contagion_no_event_without_correlation() {
        let g = CorrelationGraph::new(asset_list(), 0.0).unwrap();
        // No data fed → no correlations → no contagion
        let det = ContagionDetector::new(0.01, 0.5).unwrap();
        let mut returns = HashMap::new();
        returns.insert("BTC".to_owned(), -0.10);
        returns.insert("ETH".to_owned(), -0.10);
        let events = det.check(&returns, &g);
        assert!(events.is_empty());
    }
}
