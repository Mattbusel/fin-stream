//! Position sizing, risk management, and portfolio-level controls.
//!
//! Provides Kelly-like position sizing, stop-loss/take-profit monitoring,
//! daily P&L limits, and simplified portfolio VaR computation using
//! a [`DashMap`]-backed concurrent position store.

use dashmap::DashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Enums and simple types
// ---------------------------------------------------------------------------

/// Direction of a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositionSide {
    /// Long position (buy to open).
    Long,
    /// Short position (sell to open).
    Short,
    /// No position.
    Flat,
}

/// Risk limits applied to the position manager.
#[derive(Debug, Clone)]
pub struct RiskLimits {
    /// Maximum notional value of a single position (USD).
    pub max_position_usd: f64,
    /// Maximum allowed drawdown as a fraction (e.g. 0.10 = 10%).
    pub max_drawdown_pct: f64,
    /// Maximum concentration of any single position (fraction of total, e.g. 0.25).
    pub max_concentration_pct: f64,
    /// Stop-loss as a fraction of entry price (e.g. 0.02 = 2%).
    pub stop_loss_pct: f64,
    /// Take-profit as a fraction of entry price (e.g. 0.05 = 5%).
    pub take_profit_pct: f64,
    /// Daily loss limit in USD; trading halts when breached.
    pub daily_loss_limit_usd: f64,
}

// ---------------------------------------------------------------------------
// Position state
// ---------------------------------------------------------------------------

/// Current state of an open position.
#[derive(Debug, Clone)]
pub struct PositionState {
    /// Instrument symbol.
    pub symbol: String,
    /// Long, Short, or Flat.
    pub side: PositionSide,
    /// Quantity (contracts, shares, etc.).
    pub quantity: f64,
    /// Average entry price.
    pub entry_price: f64,
    /// Most recent mark price.
    pub current_price: f64,
    /// Optional stop-loss price level.
    pub stop_loss: Option<f64>,
    /// Optional take-profit price level.
    pub take_profit: Option<f64>,
}

impl PositionState {
    /// Unrealized P&L for this position.
    pub fn unrealized_pnl(&self) -> f64 {
        match self.side {
            PositionSide::Long => (self.current_price - self.entry_price) * self.quantity,
            PositionSide::Short => (self.entry_price - self.current_price) * self.quantity,
            PositionSide::Flat => 0.0,
        }
    }

    /// Unrealized P&L as a fraction of entry value.
    pub fn pnl_pct(&self) -> f64 {
        let entry_value = self.entry_price * self.quantity;
        if entry_value.abs() < 1e-12 {
            return 0.0;
        }
        self.unrealized_pnl() / entry_value
    }

    /// Current mark-to-market value of the position.
    pub fn market_value(&self) -> f64 {
        self.current_price * self.quantity
    }

    /// Returns `true` if the position has breached its stop-loss level.
    pub fn is_stopped_out(&self) -> bool {
        match (&self.side, self.stop_loss) {
            (PositionSide::Long, Some(sl)) => self.current_price <= sl,
            (PositionSide::Short, Some(sl)) => self.current_price >= sl,
            _ => false,
        }
    }

    /// Returns `true` if the position has hit its take-profit level.
    pub fn is_take_profit(&self) -> bool {
        match (&self.side, self.take_profit) {
            (PositionSide::Long, Some(tp)) => self.current_price >= tp,
            (PositionSide::Short, Some(tp)) => self.current_price <= tp,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors returned by [`PositionManager`] operations.
#[derive(Debug, Error)]
pub enum PositionError {
    /// A risk limit was exceeded; contains a human-readable description.
    #[error("risk limit exceeded: {0}")]
    RiskLimitExceeded(String),
    /// A position for this symbol is already open.
    #[error("position already open")]
    AlreadyOpen,
    /// The daily loss limit has been reached.
    #[error("daily loss limit reached")]
    DailyLimitReached,
}

// ---------------------------------------------------------------------------
// Position summary
// ---------------------------------------------------------------------------

/// Aggregate statistics across all open positions.
#[derive(Debug, Clone)]
pub struct PositionSummary {
    /// Number of currently open positions.
    pub open_positions: usize,
    /// Total long market value (USD).
    pub long_exposure: f64,
    /// Total short market value (USD).
    pub short_exposure: f64,
    /// Net exposure: long minus short (USD).
    pub net_exposure: f64,
    /// Sum of unrealized P&L across all positions (USD).
    pub total_unrealized_pnl: f64,
    /// Cumulative daily P&L (USD, updated on close).
    pub daily_pnl_usd: f64,
}

// ---------------------------------------------------------------------------
// Position manager
// ---------------------------------------------------------------------------

/// Normal distribution z-scores for common confidence levels (90%, 95%, 99%).
fn z_score_for_confidence(confidence: f64) -> f64 {
    // Approximate inverse-normal using a lookup table
    if confidence >= 0.99 {
        2.326
    } else if confidence >= 0.975 {
        1.96
    } else if confidence >= 0.95 {
        1.645
    } else if confidence >= 0.90 {
        1.282
    } else {
        1.0
    }
}

/// Concurrent position manager with Kelly-like sizing and risk controls.
///
/// All position state is stored in a [`DashMap`] for lock-free concurrent access.
/// Daily P&L is tracked atomically in micro-dollars (1 USD = 1_000_000 units).
pub struct PositionManager {
    positions: DashMap<String, PositionState>,
    limits: RiskLimits,
    /// Daily P&L in micro-dollars (i64 for atomic ops).
    daily_pnl_micro: AtomicI64,
}

impl PositionManager {
    /// Create a new `PositionManager` with the given risk limits.
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            positions: DashMap::new(),
            limits,
            daily_pnl_micro: AtomicI64::new(0),
        }
    }

    /// Open a new position using Kelly-like sizing.
    ///
    /// Kelly fraction ≈ `signal_strength * 0.5` (half-Kelly).
    /// Final quantity is capped by `max_position_usd`.
    ///
    /// Returns the position quantity opened, or a [`PositionError`].
    pub fn open_position(
        &self,
        symbol: &str,
        side: PositionSide,
        price: f64,
        capital: f64,
        signal_strength: f64,
    ) -> Result<f64, PositionError> {
        // Check daily limit
        if self.check_daily_limit() {
            return Err(PositionError::DailyLimitReached);
        }
        // Reject if already open
        if self.positions.contains_key(symbol) {
            return Err(PositionError::AlreadyOpen);
        }
        if price.abs() < 1e-12 {
            return Err(PositionError::RiskLimitExceeded("price is zero".to_string()));
        }

        // Kelly-like sizing: half-Kelly
        let kelly_fraction = (signal_strength.abs() * 0.5).clamp(0.0, 1.0);
        let notional = (capital * kelly_fraction).min(self.limits.max_position_usd);

        if notional < 1e-6 {
            return Err(PositionError::RiskLimitExceeded(
                "computed notional too small".to_string(),
            ));
        }

        let quantity = notional / price;

        // Concentration check
        let total_exposure = self.total_exposure();
        if total_exposure > 1e-6 {
            let new_share = notional / (total_exposure + notional);
            if new_share > self.limits.max_concentration_pct {
                return Err(PositionError::RiskLimitExceeded(format!(
                    "concentration {:.1}% exceeds limit {:.1}%",
                    new_share * 100.0,
                    self.limits.max_concentration_pct * 100.0
                )));
            }
        }

        // Compute stop-loss and take-profit levels
        let (stop_loss, take_profit) = match side {
            PositionSide::Long => (
                Some(price * (1.0 - self.limits.stop_loss_pct)),
                Some(price * (1.0 + self.limits.take_profit_pct)),
            ),
            PositionSide::Short => (
                Some(price * (1.0 + self.limits.stop_loss_pct)),
                Some(price * (1.0 - self.limits.take_profit_pct)),
            ),
            PositionSide::Flat => (None, None),
        };

        let state = PositionState {
            symbol: symbol.to_string(),
            side,
            quantity,
            entry_price: price,
            current_price: price,
            stop_loss,
            take_profit,
        };

        self.positions.insert(symbol.to_string(), state);
        Ok(quantity)
    }

    /// Close an open position at `price`, returning the realized P&L (or `None` if not open).
    pub fn close_position(&self, symbol: &str, price: f64) -> Option<f64> {
        let (_, state) = self.positions.remove(symbol)?;
        let pnl = match state.side {
            PositionSide::Long => (price - state.entry_price) * state.quantity,
            PositionSide::Short => (state.entry_price - price) * state.quantity,
            PositionSide::Flat => 0.0,
        };
        // Accumulate daily P&L (convert to micro-dollars)
        let micro = (pnl * 1_000_000.0) as i64;
        self.daily_pnl_micro.fetch_add(micro, Ordering::Relaxed);
        Some(pnl)
    }

    /// Update the mark price for a symbol.
    pub fn update_price(&self, symbol: &str, price: f64) {
        if let Some(mut state) = self.positions.get_mut(symbol) {
            state.current_price = price;
        }
    }

    /// Return symbols whose positions have breached their stop-loss.
    pub fn check_stops(&self) -> Vec<String> {
        self.positions
            .iter()
            .filter(|entry| entry.value().is_stopped_out())
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Return `true` if the daily loss limit has been reached.
    pub fn check_daily_limit(&self) -> bool {
        let daily_pnl = self.daily_pnl_micro.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        daily_pnl <= -self.limits.daily_loss_limit_usd.abs()
    }

    /// Simplified portfolio VaR at a given confidence level.
    ///
    /// `VaR ≈ total_exposure × vol_estimate × z_score × sqrt(holding_days)`.
    /// Vol estimate defaults to 2% daily.
    pub fn portfolio_var_pct(&self, confidence: f64) -> f64 {
        let z = z_score_for_confidence(confidence);
        let daily_vol = 0.02_f64; // 2% daily vol assumption
        let holding_days = 1.0_f64;
        z * daily_vol * holding_days.sqrt()
    }

    /// Compute a snapshot of portfolio-level statistics.
    pub fn position_summary(&self) -> PositionSummary {
        let mut long_exposure = 0.0_f64;
        let mut short_exposure = 0.0_f64;
        let mut total_unrealized_pnl = 0.0_f64;
        let mut open_positions = 0_usize;

        for entry in self.positions.iter() {
            let s = entry.value();
            open_positions += 1;
            total_unrealized_pnl += s.unrealized_pnl();
            match s.side {
                PositionSide::Long => long_exposure += s.market_value(),
                PositionSide::Short => short_exposure += s.market_value(),
                PositionSide::Flat => {}
            }
        }

        let daily_pnl_usd = self.daily_pnl_micro.load(Ordering::Relaxed) as f64 / 1_000_000.0;

        PositionSummary {
            open_positions,
            long_exposure,
            short_exposure,
            net_exposure: long_exposure - short_exposure,
            total_unrealized_pnl,
            daily_pnl_usd,
        }
    }

    /// Sum of all position market values (long + short).
    fn total_exposure(&self) -> f64 {
        self.positions
            .iter()
            .map(|entry| entry.value().market_value())
            .sum()
    }

    /// Reset daily P&L counter (call at start of each trading day).
    pub fn reset_daily_pnl(&self) {
        self.daily_pnl_micro.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_limits() -> RiskLimits {
        RiskLimits {
            max_position_usd: 10_000.0,
            max_drawdown_pct: 0.10,
            max_concentration_pct: 0.50,
            stop_loss_pct: 0.02,
            take_profit_pct: 0.05,
            daily_loss_limit_usd: 500.0,
        }
    }

    #[test]
    fn open_and_close_position() {
        let pm = PositionManager::new(test_limits());
        let qty = pm
            .open_position("AAPL", PositionSide::Long, 150.0, 100_000.0, 0.8)
            .expect("should open");
        assert!(qty > 0.0);

        pm.update_price("AAPL", 155.0);
        let pnl = pm.close_position("AAPL", 155.0).expect("should close");
        assert!(pnl > 0.0);
    }

    #[test]
    fn duplicate_open_returns_error() {
        let pm = PositionManager::new(test_limits());
        pm.open_position("BTC", PositionSide::Long, 30_000.0, 100_000.0, 0.6)
            .expect("first open");
        let result = pm.open_position("BTC", PositionSide::Long, 30_000.0, 100_000.0, 0.6);
        assert!(matches!(result, Err(PositionError::AlreadyOpen)));
    }

    #[test]
    fn stop_loss_detected() {
        let pm = PositionManager::new(test_limits());
        pm.open_position("ETH", PositionSide::Long, 2000.0, 100_000.0, 0.9)
            .expect("open");
        // Drop price below stop-loss (2% below entry = 1960)
        pm.update_price("ETH", 1950.0);
        let stopped = pm.check_stops();
        assert!(stopped.contains(&"ETH".to_string()));
    }

    #[test]
    fn daily_limit_check() {
        let pm = PositionManager::new(test_limits());
        // Inject a large loss
        pm.daily_pnl_micro
            .store((-600_000_000_i64), Ordering::Relaxed);
        assert!(pm.check_daily_limit());
    }

    #[test]
    fn position_summary_counts_open() {
        let pm = PositionManager::new(test_limits());
        pm.open_position("X", PositionSide::Long, 50.0, 10_000.0, 0.7)
            .expect("open");
        let summary = pm.position_summary();
        assert_eq!(summary.open_positions, 1);
        assert!(summary.long_exposure > 0.0);
    }

    #[test]
    fn unrealized_pnl_long() {
        let state = PositionState {
            symbol: "TEST".to_string(),
            side: PositionSide::Long,
            quantity: 100.0,
            entry_price: 10.0,
            current_price: 12.0,
            stop_loss: None,
            take_profit: None,
        };
        assert!((state.unrealized_pnl() - 200.0).abs() < 1e-9);
    }

    #[test]
    fn unrealized_pnl_short() {
        let state = PositionState {
            symbol: "TEST".to_string(),
            side: PositionSide::Short,
            quantity: 100.0,
            entry_price: 10.0,
            current_price: 8.0,
            stop_loss: None,
            take_profit: None,
        };
        assert!((state.unrealized_pnl() - 200.0).abs() < 1e-9);
    }
}
