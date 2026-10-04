//! Counters emitted through the [`metrics`](https://docs.rs/metrics) facade.
//!
//! Enabled by the `metrics` cargo feature. fin-stream only *emits*; you choose where
//! the numbers go by installing a recorder once at startup, for example
//! `metrics-exporter-prometheus` for a `/metrics` endpoint, or an OpenTelemetry or
//! StatsD exporter. With the feature off, or with no recorder installed, every call
//! here compiles to nothing or a no-op.
//!
//! | metric | type | labels | meaning |
//! |---|---|---|---|
//! | `fin_stream_ws_connects_total` | counter | `url` | WebSocket handshakes that succeeded |
//! | `fin_stream_ws_connect_failures_total` | counter | `url` | connection attempts that failed before the handshake completed |
//! | `fin_stream_ws_disconnects_total` | counter | `url` | established connections that ended (server close, error, dead connection) |
//! | `fin_stream_ws_dead_connections_total` | counter | `url` | connections dropped because nothing arrived for two ping intervals |
//! | `fin_stream_ws_messages_total` | counter | `url` | text and binary frames received |
//! | `fin_stream_ws_bytes_total` | counter | `url` | payload bytes received |
//! | `fin_stream_ohlcv_bars_total` | counter | `symbol` | bars completed by [`crate::ohlcv::OhlcvAggregator`] |
//! | `fin_stream_ohlcv_late_ticks_total` | counter | `symbol` | ticks dropped because their bar had already closed |
//!
//! Handles are created when a [`crate::ws::WsManager::run`] loop or an
//! [`crate::ohlcv::OhlcvAggregator`] starts, so install the recorder before that.

/// Counter handles for one WebSocket connection loop.
#[derive(Clone)]
pub(crate) struct WsCounters {
    #[cfg(feature = "metrics")]
    inner: [metrics::Counter; 6],
}

impl WsCounters {
    /// Register (or look up) the counters for `url`.
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    pub(crate) fn new(url: &str) -> Self {
        #[cfg(feature = "metrics")]
        {
            let u = url.to_owned();
            Self {
                inner: [
                    metrics::counter!("fin_stream_ws_connects_total", "url" => u.clone()),
                    metrics::counter!("fin_stream_ws_connect_failures_total", "url" => u.clone()),
                    metrics::counter!("fin_stream_ws_disconnects_total", "url" => u.clone()),
                    metrics::counter!("fin_stream_ws_dead_connections_total", "url" => u.clone()),
                    metrics::counter!("fin_stream_ws_messages_total", "url" => u.clone()),
                    metrics::counter!("fin_stream_ws_bytes_total", "url" => u),
                ],
            }
        }
        #[cfg(not(feature = "metrics"))]
        Self {}
    }

    #[inline]
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    fn add(&self, idx: usize, n: u64) {
        #[cfg(feature = "metrics")]
        self.inner[idx].increment(n);
    }

    pub(crate) fn connected(&self) {
        self.add(0, 1);
    }
    pub(crate) fn connect_failed(&self) {
        self.add(1, 1);
    }
    pub(crate) fn disconnected(&self) {
        self.add(2, 1);
    }
    pub(crate) fn dead_connection(&self) {
        self.add(3, 1);
    }
    #[inline]
    pub(crate) fn message(&self, bytes: usize) {
        self.add(4, 1);
        self.add(5, bytes as u64);
    }
}

/// Counter handles for one bar aggregator.
#[derive(Clone)]
pub(crate) struct BarCounters {
    #[cfg(feature = "metrics")]
    inner: [metrics::Counter; 2],
}

impl std::fmt::Debug for BarCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BarCounters")
    }
}

impl BarCounters {
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    pub(crate) fn new(symbol: &str) -> Self {
        #[cfg(feature = "metrics")]
        {
            let s = symbol.to_owned();
            Self {
                inner: [
                    metrics::counter!("fin_stream_ohlcv_bars_total", "symbol" => s.clone()),
                    metrics::counter!("fin_stream_ohlcv_late_ticks_total", "symbol" => s),
                ],
            }
        }
        #[cfg(not(feature = "metrics"))]
        Self {}
    }

    #[inline]
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    pub(crate) fn bars(&self, n: u64) {
        #[cfg(feature = "metrics")]
        self.inner[0].increment(n);
    }

    #[inline]
    pub(crate) fn late_tick(&self) {
        #[cfg(feature = "metrics")]
        self.inner[1].increment(1);
    }
}
