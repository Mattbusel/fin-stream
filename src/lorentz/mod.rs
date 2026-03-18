//! Lorentz (special-relativistic) transforms applied to financial time series.
//!
//! ## Mathematical basis
//!
//! In special relativity, the Lorentz transformation relates the spacetime
//! coordinates `(t, x)` measured in one inertial frame `S` to the coordinates
//! `(t', x')` measured in a frame `S'` that moves with velocity `v` along the
//! x-axis:
//!
//! ```text
//!     t' = gamma * (t  - beta * x / c)
//!     x' = gamma * (x  - beta * c * t)
//!
//!     where  beta  = v / c          (dimensionless velocity, 0 <= beta < 1)
//!            gamma = 1 / sqrt(1 - beta^2)   (Lorentz factor, >= 1)
//! ```
//!
//! ## Application to market data
//!
//! Market microstructure theory frequently treats price discovery as a
//! diffusion process in two dimensions:
//!
//! - **Time axis `t`**: wall-clock time (seconds, or normalized to [0, 1]).
//! - **Price axis `x`**: log-price or normalized price.
//!
//! The Lorentz frame boost is applied as a **feature engineering** step:
//! it stretches or compresses the price-time plane along hyperbolas, which
//! are invariant under Lorentz boosts. The motivation is that certain
//! microstructure signals (momentum, mean-reversion) that appear curved in
//! the lab frame can appear as straight lines in a boosted frame.
//!
//! For a financial series, we set:
//! - `c = 1` (price changes are bounded by some maximum instantaneous return).
//! - `beta = drift_rate` (the normalized drift velocity of the price process).
//!
//! The transformed coordinates `(t', x')` are then fed as features into a
//! downstream model.
//!
//! ## Guarantees
//!
//! - Non-panicking: construction validates `beta` and returns a typed error.
//! - Deterministic: the same `(t, x, beta)` always produces the same output.
//! - `beta = 0` is the identity transform (gamma = 1, no distortion).

use crate::error::StreamError;

/// A spacetime event expressed as a (time, position) coordinate pair.
///
/// In the financial interpretation:
/// - `t` is elapsed time since the series start, normalized to a convenient
///   scale (e.g., seconds, or fraction of the session).
/// - `x` is normalized log-price or normalized price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpacetimePoint {
    /// Time coordinate.
    pub t: f64,
    /// Spatial (price) coordinate.
    pub x: f64,
}

impl SpacetimePoint {
    /// Construct a new spacetime point.
    pub fn new(t: f64, x: f64) -> Self {
        Self { t, x }
    }
}

/// Lorentz frame-boost transform for financial time series.
///
/// Applies the special-relativistic Lorentz transformation with velocity
/// parameter `beta = v/c` to `(t, x)` coordinate pairs. The speed of light
/// is normalized to `c = 1`.
///
/// # Construction
///
/// ```rust
/// use fin_stream::lorentz::LorentzTransform;
///
/// let lt = LorentzTransform::new(0.5).unwrap(); // beta = 0.5
/// ```
///
/// # Identity
///
/// `beta = 0` is the identity: `t' = t`, `x' = x`.
///
/// # Singularity avoidance
///
/// `beta >= 1` would require dividing by zero in the Lorentz factor and is
/// rejected at construction time with `LorentzConfigError`.
#[derive(Debug, Clone, Copy)]
pub struct LorentzTransform {
    /// Normalized velocity (v/c). Satisfies `0.0 <= beta < 1.0`.
    beta: f64,
    /// Precomputed Lorentz factor: `1 / sqrt(1 - beta^2)`.
    gamma: f64,
}

impl LorentzTransform {
    /// Create a new Lorentz transform with the given velocity parameter.
    ///
    /// `beta` must satisfy `0.0 <= beta < 1.0`.
    ///
    /// # Errors
    ///
    /// Returns `LorentzConfigError` if `beta` is negative, `NaN`, or `>= 1.0`.
    pub fn new(beta: f64) -> Result<Self, StreamError> {
        if beta.is_nan() || beta < 0.0 || beta >= 1.0 {
            return Err(StreamError::LorentzConfigError {
                reason: format!(
                    "beta must be in [0.0, 1.0) but got {beta}; \
                     beta >= 1 produces a division by zero in the Lorentz factor"
                ),
            });
        }
        let gamma = 1.0 / (1.0 - beta * beta).sqrt();
        Ok(Self { beta, gamma })
    }

    /// The configured velocity parameter `beta = v/c`.
    pub fn beta(&self) -> f64 {
        self.beta
    }

    /// The precomputed Lorentz factor `gamma = 1 / sqrt(1 - beta^2)`.
    ///
    /// `gamma == 1.0` when `beta == 0` (identity). `gamma` increases
    /// monotonically towards infinity as `beta` approaches 1.
    pub fn gamma(&self) -> f64 {
        self.gamma
    }

    /// Apply the Lorentz boost to a spacetime point.
    ///
    /// Computes:
    /// ```text
    ///     t' = gamma * (t - beta * x)
    ///     x' = gamma * (x - beta * t)
    /// ```
    ///
    /// # Complexity: O(1), no heap allocation.
    pub fn transform(&self, p: SpacetimePoint) -> SpacetimePoint {
        let t_prime = self.gamma * (p.t - self.beta * p.x);
        let x_prime = self.gamma * (p.x - self.beta * p.t);
        SpacetimePoint { t: t_prime, x: x_prime }
    }

    /// Apply the inverse Lorentz boost (boost in the opposite direction).
    ///
    /// Computes:
    /// ```text
    ///     t' = gamma * (t + beta * x)
    ///     x' = gamma * (x + beta * t)
    /// ```
    ///
    /// For a point `p`, `inverse_transform(transform(p)) == p` up to floating-
    /// point rounding.
    ///
    /// # Complexity: O(1), no heap allocation.
    pub fn inverse_transform(&self, p: SpacetimePoint) -> SpacetimePoint {
        let t_orig = self.gamma * (p.t + self.beta * p.x);
        let x_orig = self.gamma * (p.x + self.beta * p.t);
        SpacetimePoint { t: t_orig, x: x_orig }
    }

    /// Apply the boost to a batch of points.
    ///
    /// Returns a new `Vec` of transformed points. The input slice is not
    /// modified.
    ///
    /// # Complexity: O(n), one heap allocation of size `n`.
    pub fn transform_batch(&self, points: &[SpacetimePoint]) -> Vec<SpacetimePoint> {
        points.iter().map(|&p| self.transform(p)).collect()
    }

    /// Time-dilation: the transformed time coordinate for a point at rest
    /// (`x = 0`) is `t' = gamma * t`.
    ///
    /// This is a convenience method that surfaces the time-dilation formula
    /// for the common case where only the time axis is of interest.
    ///
    /// # Complexity: O(1)
    pub fn dilate_time(&self, t: f64) -> f64 {
        self.gamma * t
    }

    /// Length-contraction: the transformed spatial coordinate for an event at
    /// the time origin (`t = 0`) is `x' = gamma * x`.
    ///
    /// This is a convenience method that surfaces the length-contraction
    /// formula for the common case where only the price axis is of interest.
    ///
    /// # Complexity: O(1)
    pub fn contract_length(&self, x: f64) -> f64 {
        self.gamma * x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-10;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < EPS
    }

    fn point_approx_eq(a: SpacetimePoint, b: SpacetimePoint) -> bool {
        approx_eq(a.t, b.t) && approx_eq(a.x, b.x)
    }

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn test_new_valid_beta() {
        let lt = LorentzTransform::new(0.5).unwrap();
        assert!((lt.beta() - 0.5).abs() < EPS);
    }

    #[test]
    fn test_new_beta_zero() {
        let lt = LorentzTransform::new(0.0).unwrap();
        assert_eq!(lt.beta(), 0.0);
        assert!((lt.gamma() - 1.0).abs() < EPS);
    }

    #[test]
    fn test_new_beta_one_returns_error() {
        let err = LorentzTransform::new(1.0).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    #[test]
    fn test_new_beta_above_one_returns_error() {
        let err = LorentzTransform::new(1.5).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    #[test]
    fn test_new_beta_negative_returns_error() {
        let err = LorentzTransform::new(-0.1).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    #[test]
    fn test_new_beta_nan_returns_error() {
        let err = LorentzTransform::new(f64::NAN).unwrap_err();
        assert!(matches!(err, StreamError::LorentzConfigError { .. }));
    }

    // ── beta = 0 is identity ─────────────────────────────────────────────────

    #[test]
    fn test_beta_zero_is_identity_transform() {
        let lt = LorentzTransform::new(0.0).unwrap();
        let p = SpacetimePoint::new(3.0, 4.0);
        let q = lt.transform(p);
        assert!(point_approx_eq(p, q), "beta=0 must be identity, got {q:?}");
    }

    // ── Time dilation ─────────────────────────────────────────────────────────

    /// For a point at x=0, t' = gamma * t (pure time dilation).
    #[test]
    fn test_time_dilation_at_x_zero() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p = SpacetimePoint::new(5.0, 0.0);
        let q = lt.transform(p);
        let expected_t = lt.gamma() * 5.0;
        assert!(approx_eq(q.t, expected_t));
    }

    #[test]
    fn test_dilate_time_helper() {
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!(approx_eq(lt.dilate_time(1.0), lt.gamma()));
    }

    // ── Length contraction ────────────────────────────────────────────────────

    /// For a point at t=0, x' = gamma * x (pure length contraction).
    #[test]
    fn test_length_contraction_at_t_zero() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p = SpacetimePoint::new(0.0, 5.0);
        let q = lt.transform(p);
        let expected_x = lt.gamma() * 5.0;
        assert!(approx_eq(q.x, expected_x));
    }

    #[test]
    fn test_contract_length_helper() {
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!(approx_eq(lt.contract_length(1.0), lt.gamma()));
    }

    // ── Known beta values ─────────────────────────────────────────────────────

    /// For beta = 0.6, gamma = 1.25 (classic textbook value).
    #[test]
    fn test_known_beta_0_6_gamma_is_1_25() {
        let lt = LorentzTransform::new(0.6).unwrap();
        assert!((lt.gamma() - 1.25).abs() < 1e-9, "gamma should be 1.25, got {}", lt.gamma());
    }

    /// For beta = 0.8, gamma = 5/3 approximately 1.6667.
    #[test]
    fn test_known_beta_0_8_gamma() {
        let lt = LorentzTransform::new(0.8).unwrap();
        let expected_gamma = 5.0 / 3.0;
        assert!((lt.gamma() - expected_gamma).abs() < 1e-9);
    }

    /// For beta = 0.5, gamma = 2/sqrt(3) approximately 1.1547.
    #[test]
    fn test_known_beta_0_5_gamma() {
        let lt = LorentzTransform::new(0.5).unwrap();
        let expected_gamma = 2.0 / 3.0f64.sqrt();
        assert!((lt.gamma() - expected_gamma).abs() < 1e-9);
    }

    /// Transform a known point with beta=0.6 and verify the result manually.
    ///
    /// Point (t=1, x=0): t' = 1.25 * (1 - 0.6*0) = 1.25; x' = 1.25*(0 - 0.6) = -0.75
    #[test]
    fn test_transform_known_point_beta_0_6() {
        let lt = LorentzTransform::new(0.6).unwrap();
        let p = SpacetimePoint::new(1.0, 0.0);
        let q = lt.transform(p);
        assert!((q.t - 1.25).abs() < 1e-9);
        assert!((q.x - (-0.75)).abs() < 1e-9);
    }

    // ── Inverse round-trip ────────────────────────────────────────────────────

    #[test]
    fn test_inverse_transform_roundtrip() {
        let lt = LorentzTransform::new(0.7).unwrap();
        let p = SpacetimePoint::new(3.0, 1.5);
        let q = lt.transform(p);
        let r = lt.inverse_transform(q);
        assert!(point_approx_eq(r, p), "round-trip failed: expected {p:?}, got {r:?}");
    }

    // ── Batch transform ───────────────────────────────────────────────────────

    #[test]
    fn test_transform_batch_length_preserved() {
        let lt = LorentzTransform::new(0.3).unwrap();
        let pts = vec![
            SpacetimePoint::new(0.0, 1.0),
            SpacetimePoint::new(1.0, 2.0),
            SpacetimePoint::new(2.0, 3.0),
        ];
        let out = lt.transform_batch(&pts);
        assert_eq!(out.len(), pts.len());
    }

    #[test]
    fn test_transform_batch_matches_individual() {
        let lt = LorentzTransform::new(0.4).unwrap();
        let pts = vec![
            SpacetimePoint::new(1.0, 0.5),
            SpacetimePoint::new(2.0, 1.5),
        ];
        let batch = lt.transform_batch(&pts);
        for (i, &p) in pts.iter().enumerate() {
            let individual = lt.transform(p);
            assert!(
                point_approx_eq(batch[i], individual),
                "batch[{i}] differs from individual transform"
            );
        }
    }

    // ── SpacetimePoint ────────────────────────────────────────────────────────

    #[test]
    fn test_spacetime_point_fields() {
        let p = SpacetimePoint::new(1.5, 2.5);
        assert_eq!(p.t, 1.5);
        assert_eq!(p.x, 2.5);
    }

    #[test]
    fn test_spacetime_point_equality() {
        let p = SpacetimePoint::new(1.0, 2.0);
        let q = SpacetimePoint::new(1.0, 2.0);
        assert_eq!(p, q);
    }
}
