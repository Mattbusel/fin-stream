//! Simulated market with agent-based price formation.
//!
//! Provides [`MarketSimulator`] which runs heterogeneous trading agents
//! (market makers, trend followers, mean reverters, noise traders, informed)
//! through a continuous double auction and records the resulting price path.

use std::f64::consts::PI;

// ---------------------------------------------------------------------------
// Agent types
// ---------------------------------------------------------------------------

/// Classification of a simulated trading agent's strategy.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentType {
    /// Provides liquidity by quoting both sides of the spread.
    MarketMaker,
    /// Buys after recent up-moves, sells after down-moves.
    TrendFollower,
    /// Bets on price reverting toward a long-run fundamental value.
    MeanReverter,
    /// Submits random orders unrelated to fundamentals.
    NoiseTrader,
    /// Trades on private information about the true fundamental value.
    Informed,
}

/// A single simulated agent participating in the market.
#[derive(Debug, Clone)]
pub struct SimAgent {
    /// Unique agent identifier.
    pub id: usize,
    /// Trading strategy type.
    pub agent_type: AgentType,
    /// Available cash capital.
    pub capital: f64,
    /// Current inventory (positive = long, negative = short).
    pub inventory: f64,
    /// Risk aversion coefficient: higher → smaller order sizes.
    pub risk_aversion: f64,
}

impl SimAgent {
    /// Generate an optional order for this agent given the current market state.
    ///
    /// Returns `None` when the agent decides not to trade this step.
    pub fn generate_order(
        &self,
        mid_price: f64,
        volatility: f64,
        step: u64,
        seed: u64,
    ) -> Option<SimOrder> {
        // Deterministic pseudo-random number from step, agent id, and seed.
        let s0 = seed ^ step.wrapping_mul(6_364_136_223_846_793_005) ^ (self.id as u64).wrapping_mul(2_862_933_555_777_941_757);
        let r = lcg_uniform(s0);
        let s1 = s0 ^ 0xDEAD_BEEF_CAFE_1234_u64;
        let r2 = lcg_uniform(s1);
        let s2 = s1 ^ 0x0123_4567_89AB_CDEF_u64;
        let r3 = lcg_uniform(s2);

        // Quantity: risk-aversion-scaled, minimum 1 unit.
        let base_qty = (self.capital * 0.01 / mid_price.max(1e-6) / self.risk_aversion).max(1.0);
        let qty = base_qty * (0.5 + r3);

        match self.agent_type {
            AgentType::NoiseTrader => {
                // Random direction, market order.
                Some(SimOrder {
                    agent_id: self.id,
                    is_buy: r > 0.5,
                    price: mid_price,
                    quantity: qty,
                    order_type: SimOrderType::Market,
                })
            }
            AgentType::MarketMaker => {
                // Quote both sides; here we emit whichever side restores inventory balance.
                let is_buy = self.inventory < 0.0 || r > 0.6;
                let spread = volatility * 2.0 * mid_price;
                let offset = if is_buy { -spread * 0.5 } else { spread * 0.5 };
                Some(SimOrder {
                    agent_id: self.id,
                    is_buy,
                    price: (mid_price + offset).max(1e-6),
                    quantity: qty * 0.5,
                    order_type: SimOrderType::Limit,
                })
            }
            AgentType::TrendFollower => {
                // Only trade after sufficient history — use step as a proxy.
                if step < 5 {
                    return None;
                }
                // Momentum signal: buy if seed hash indicates upward drift.
                let signal = if seed % 2 == 0 { 1.0_f64 } else { -1.0_f64 };
                if signal * (r - 0.5) < 0.1 {
                    return None;
                }
                let is_buy = signal > 0.0;
                Some(SimOrder {
                    agent_id: self.id,
                    is_buy,
                    price: mid_price,
                    quantity: qty,
                    order_type: SimOrderType::Market,
                })
            }
            AgentType::MeanReverter => {
                // No order if price is near fundamental (represented by seed / large number).
                let fundamental = (seed as f64 % 1_000.0) + 100.0;
                let deviation = (mid_price - fundamental) / fundamental;
                if deviation.abs() < 0.01 {
                    return None;
                }
                let is_buy = deviation < 0.0; // buy when below fundamental
                let limit_price = if is_buy {
                    mid_price * (1.0 + 0.002)
                } else {
                    mid_price * (1.0 - 0.002)
                };
                Some(SimOrder {
                    agent_id: self.id,
                    is_buy,
                    price: limit_price.max(1e-6),
                    quantity: qty * deviation.abs().min(1.0),
                    order_type: SimOrderType::Limit,
                })
            }
            AgentType::Informed => {
                // Informed trader knows the true fundamental.
                let fundamental = (seed as f64 % 1_000.0) + 100.0;
                let is_buy = fundamental > mid_price;
                if (fundamental - mid_price).abs() < mid_price * 0.005 {
                    return None;
                }
                Some(SimOrder {
                    agent_id: self.id,
                    is_buy,
                    price: mid_price,
                    quantity: qty * 2.0,
                    order_type: SimOrderType::Market,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Order types
// ---------------------------------------------------------------------------

/// Order type used in the simulated market.
#[derive(Debug, Clone, PartialEq)]
pub enum SimOrderType {
    /// Execute immediately at best available price.
    Market,
    /// Rest at the specified limit price.
    Limit,
    /// Cancel an existing resting order (not used in this simplified model).
    Cancel,
}

/// An order submitted by a simulated agent.
#[derive(Debug, Clone)]
pub struct SimOrder {
    /// ID of the submitting agent.
    pub agent_id: usize,
    /// True for a buy order.
    pub is_buy: bool,
    /// Limit price (used for `Limit` orders; for `Market` orders this is the mid-price).
    pub price: f64,
    /// Order quantity.
    pub quantity: f64,
    /// Order type.
    pub order_type: SimOrderType,
}

// ---------------------------------------------------------------------------
// Price formation
// ---------------------------------------------------------------------------

/// Fundamental value process driving the simulated market.
#[derive(Debug, Clone)]
pub struct PriceFormation {
    /// Current fundamental value (random walk).
    pub fundamental_value: f64,
    /// Variance of the per-step noise shock.
    pub noise_variance: f64,
    /// Drift per step.
    pub drift: f64,
}

impl PriceFormation {
    /// Apply a random-walk shock to the fundamental value.
    pub fn update_fundamental(&mut self, shock: f64) {
        self.fundamental_value = (self.fundamental_value + self.drift + shock).max(1e-6);
    }

    /// Current mid-price (equal to fundamental in this simplified model).
    pub fn mid_price(&self) -> f64 {
        self.fundamental_value
    }
}

// ---------------------------------------------------------------------------
// Simulation step / result
// ---------------------------------------------------------------------------

/// Output of a single simulation step.
#[derive(Debug, Clone)]
pub struct SimStep {
    /// List of matched trades: (price, quantity).
    pub trades: Vec<(f64, f64)>,
    /// Closing price for this step.
    pub new_price: f64,
    /// Best bid at end of step.
    pub bid: f64,
    /// Best ask at end of step.
    pub ask: f64,
    /// Total traded volume this step.
    pub volume: f64,
}

/// Aggregate simulation output over all steps.
#[derive(Debug, Clone)]
pub struct SimResult {
    /// Sequence of closing prices.
    pub price_path: Vec<f64>,
    /// Log-return series.
    pub return_series: Vec<f64>,
    /// Annualised realised volatility.
    pub volatility: f64,
    /// Lag-1 autocorrelation of returns.
    pub autocorrelation: f64,
    /// Excess kurtosis of returns.
    pub kurtosis: f64,
}

// ---------------------------------------------------------------------------
// MarketSimulator
// ---------------------------------------------------------------------------

/// Agent-based market simulator with a continuous double auction.
pub struct MarketSimulator {
    /// All agents participating in the market.
    pub agents: Vec<SimAgent>,
    /// Fundamental value process.
    pub formation: PriceFormation,
    /// Current simulation step counter.
    pub step: u64,
    /// History of closing prices (one per step).
    pub price_history: Vec<f64>,
    /// History of traded volumes (one per step).
    pub volume_history: Vec<f64>,
    /// History of bid-ask spreads (one per step).
    pub spread_history: Vec<f64>,
}

impl MarketSimulator {
    /// Create a new simulator with `num_agents` agents and the given initial price.
    pub fn new(num_agents: u32, initial_price: f64, seed: u64) -> Self {
        let agent_types = [
            AgentType::MarketMaker,
            AgentType::TrendFollower,
            AgentType::MeanReverter,
            AgentType::NoiseTrader,
            AgentType::Informed,
        ];

        let agents: Vec<SimAgent> = (0..num_agents as usize)
            .map(|i| {
                let r = lcg_uniform(seed ^ (i as u64).wrapping_mul(0xBEEF_CAFE));
                SimAgent {
                    id: i,
                    agent_type: agent_types[i % agent_types.len()].clone(),
                    capital: 100_000.0 * (0.5 + r),
                    inventory: 0.0,
                    risk_aversion: 1.0 + r * 2.0,
                }
            })
            .collect();

        Self {
            agents,
            formation: PriceFormation {
                fundamental_value: initial_price,
                noise_variance: initial_price * 0.001,
                drift: 0.0,
            },
            step: 0,
            price_history: vec![initial_price],
            volume_history: vec![],
            spread_history: vec![],
        }
    }

    /// Advance the simulation by one step.
    pub fn step(&mut self, seed: u64) -> SimStep {
        let step_seed = seed ^ self.step.wrapping_mul(6_364_136_223_846_793_005);
        let vol = self.compute_volatility(20).max(0.001);
        let mid = self.formation.mid_price();

        // Collect orders from all agents.
        let mut bids: Vec<SimOrder> = Vec::new();
        let mut asks: Vec<SimOrder> = Vec::new();

        for agent in &self.agents {
            if let Some(order) = agent.generate_order(mid, vol, self.step, step_seed ^ (agent.id as u64)) {
                if order.is_buy {
                    bids.push(order);
                } else {
                    asks.push(order);
                }
            }
        }

        // Match orders.
        let trades = Self::match_orders(&mut bids, &mut asks);

        // Update fundamental with a noise shock.
        let shock_u = lcg_uniform(step_seed ^ 0xF00D);
        let shock_u2 = lcg_uniform(step_seed ^ 0xBEEF);
        let shock_u = shock_u.max(1e-12);
        let shock = (-2.0 * shock_u.ln()).sqrt() * (2.0 * PI * shock_u2).cos()
            * self.formation.noise_variance.sqrt();
        self.formation.update_fundamental(shock);

        // New price.
        let prev = *self.price_history.last().unwrap_or(&mid);
        let new_price = Self::update_price(&trades, prev, self.formation.fundamental_value);

        // Compute bid/ask.
        let spread_half = vol * new_price * 0.5;
        let bid = new_price - spread_half;
        let ask = new_price + spread_half;
        let volume: f64 = trades.iter().map(|(_, q)| q).sum();

        self.price_history.push(new_price);
        self.volume_history.push(volume);
        self.spread_history.push(ask - bid);
        self.step += 1;

        SimStep { trades, new_price, bid, ask, volume }
    }

    /// Run the simulator for `steps` steps.
    pub fn run(&mut self, steps: u32, seed: u64) -> SimResult {
        for i in 0..steps {
            self.step(seed ^ (i as u64).wrapping_mul(0x1234_5678_9ABC_DEF0));
        }

        let prices = &self.price_history;
        let returns: Vec<f64> = prices
            .windows(2)
            .map(|w| (w[1] / w[0].max(1e-12)).ln())
            .collect();

        let volatility = std_dev(&returns) * (252.0_f64).sqrt();
        let autocorrelation = lag1_autocorrelation(&returns);
        let kurtosis = excess_kurtosis(&returns);

        SimResult {
            price_path: prices.clone(),
            return_series: returns,
            volatility,
            autocorrelation,
            kurtosis,
        }
    }

    /// Match bids against asks using price-time priority.
    ///
    /// Returns a list of `(price, quantity)` trades.
    pub fn match_orders(
        bids: &mut Vec<SimOrder>,
        asks: &mut Vec<SimOrder>,
    ) -> Vec<(f64, f64)> {
        // Sort bids descending by price, asks ascending.
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        let mut trades = Vec::new();
        let mut bid_idx = 0;
        let mut ask_idx = 0;
        let mut bid_qty_rem: Option<f64> = None;
        let mut ask_qty_rem: Option<f64> = None;

        while bid_idx < bids.len() && ask_idx < asks.len() {
            let bid = &bids[bid_idx];
            let ask = &asks[ask_idx];

            if bid.price < ask.price {
                break; // No more crossing orders.
            }

            let bq = bid_qty_rem.unwrap_or(bid.quantity);
            let aq = ask_qty_rem.unwrap_or(ask.quantity);
            let trade_price = (bid.price + ask.price) * 0.5;
            let trade_qty = bq.min(aq);

            trades.push((trade_price, trade_qty));

            if bq > aq {
                bid_qty_rem = Some(bq - aq);
                ask_qty_rem = None;
                ask_idx += 1;
            } else if aq > bq {
                ask_qty_rem = Some(aq - bq);
                bid_qty_rem = None;
                bid_idx += 1;
            } else {
                bid_qty_rem = None;
                ask_qty_rem = None;
                bid_idx += 1;
                ask_idx += 1;
            }
        }

        trades
    }

    /// Determine the new closing price from executed trades and the fundamental value.
    pub fn update_price(trades: &[(f64, f64)], prev_price: f64, fundamental: f64) -> f64 {
        if trades.is_empty() {
            // Mean-revert 1 % toward fundamental.
            return prev_price * 0.99 + fundamental * 0.01;
        }
        // Volume-weighted average trade price.
        let total_vol: f64 = trades.iter().map(|(_, q)| q).sum();
        let vwap = if total_vol < 1e-12 {
            prev_price
        } else {
            trades.iter().map(|(p, q)| p * q).sum::<f64>() / total_vol
        };
        // Blend with previous price and fundamental.
        vwap * 0.7 + prev_price * 0.2 + fundamental * 0.1
    }

    /// Rolling realised volatility (annualised) over `window` steps.
    pub fn compute_volatility(&self, window: usize) -> f64 {
        let prices = &self.price_history;
        if prices.len() < 2 {
            return 0.0;
        }
        let start = prices.len().saturating_sub(window + 1);
        let slice = &prices[start..];
        let returns: Vec<f64> = slice
            .windows(2)
            .map(|w| (w[1] / w[0].max(1e-12)).ln())
            .collect();
        std_dev(&returns) * (252.0_f64).sqrt()
    }
}

// ---------------------------------------------------------------------------
// Statistical helpers (module-private)
// ---------------------------------------------------------------------------

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<f64>() / v.len() as f64
}

fn std_dev(v: &[f64]) -> f64 {
    let n = v.len();
    if n < 2 { return 0.0; }
    let m = mean(v);
    let var = v.iter().map(|&x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64;
    var.sqrt()
}

fn lag1_autocorrelation(v: &[f64]) -> f64 {
    let n = v.len();
    if n < 3 { return 0.0; }
    let m = mean(v);
    let cov: f64 = v[..n - 1].iter().zip(v[1..].iter()).map(|(&a, &b)| (a - m) * (b - m)).sum();
    let var: f64 = v.iter().map(|&x| (x - m).powi(2)).sum();
    if var < 1e-14 { 0.0 } else { cov / var }
}

fn excess_kurtosis(v: &[f64]) -> f64 {
    let n = v.len();
    if n < 4 { return 0.0; }
    let m = mean(v);
    let m2 = v.iter().map(|&x| (x - m).powi(2)).sum::<f64>() / n as f64;
    if m2 < 1e-14 { return 0.0; }
    let m4 = v.iter().map(|&x| (x - m).powi(4)).sum::<f64>() / n as f64;
    m4 / m2.powi(2) - 3.0
}

/// Linear congruential generator — returns a value in [0, 1).
fn lcg_uniform(seed: u64) -> f64 {
    let s = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    (s >> 11) as f64 / (1u64 << 53) as f64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulator_runs() {
        let mut sim = MarketSimulator::new(10, 100.0, 42);
        let result = sim.run(50, 42);
        assert_eq!(result.price_path.len(), 51); // initial + 50 steps
        assert!(result.volatility >= 0.0);
    }

    #[test]
    fn test_match_orders_crossing() {
        let mut bids = vec![
            SimOrder { agent_id: 0, is_buy: true, price: 101.0, quantity: 10.0, order_type: SimOrderType::Limit },
        ];
        let mut asks = vec![
            SimOrder { agent_id: 1, is_buy: false, price: 100.0, quantity: 10.0, order_type: SimOrderType::Limit },
        ];
        let trades = MarketSimulator::match_orders(&mut bids, &mut asks);
        assert_eq!(trades.len(), 1);
        assert!((trades[0].1 - 10.0).abs() < 1e-9);
    }

    #[test]
    fn test_match_orders_non_crossing() {
        let mut bids = vec![
            SimOrder { agent_id: 0, is_buy: true, price: 99.0, quantity: 5.0, order_type: SimOrderType::Limit },
        ];
        let mut asks = vec![
            SimOrder { agent_id: 1, is_buy: false, price: 101.0, quantity: 5.0, order_type: SimOrderType::Limit },
        ];
        let trades = MarketSimulator::match_orders(&mut bids, &mut asks);
        assert!(trades.is_empty());
    }

    #[test]
    fn test_update_price_no_trades() {
        let p = MarketSimulator::update_price(&[], 100.0, 110.0);
        assert!((p - (100.0 * 0.99 + 110.0 * 0.01)).abs() < 1e-9);
    }

    #[test]
    fn test_compute_volatility() {
        let mut sim = MarketSimulator::new(5, 100.0, 1);
        sim.run(30, 1);
        let vol = sim.compute_volatility(20);
        assert!(vol >= 0.0);
    }
}
