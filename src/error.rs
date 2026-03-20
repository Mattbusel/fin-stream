//! Typed error hierarchy for fin-stream.
//!
//! All fallible operations in the pipeline return `StreamError`. Variants are
//! grouped by subsystem: connection/WebSocket, tick parsing, order book,
//! backpressure, and the streaming pipeline internals (ring buffer, aggregation,
//! normalization, transforms).

use rust_decimal::Decimal;

/// Unified error type for all fin-stream pipeline operations.
///
/// Each variant carries enough context to reconstruct the failure site without
/// inspecting internal state. The `Display` impl is machine-parseable: field
/// values never contain the literal substring used as a delimiter.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// WebSocket connection failed.
    #[error("WebSocket connection failed to '{url}': {reason}")]
    ConnectionFailed {
        /// The WebSocket URL that could not be reached.
        url: String,
        /// Human-readable description of the failure.
        reason: String,
    },

    /// WebSocket disconnected unexpectedly.
    #[error("WebSocket disconnected from '{url}'")]
    Disconnected {
        /// The WebSocket URL that was disconnected.
        url: String,
    },

    /// Reconnection attempts exhausted.
    #[error("Reconnection exhausted after {attempts} attempts to '{url}'")]
    ReconnectExhausted {
        /// The target URL for reconnection.
        url: String,
        /// Total number of reconnect attempts made.
        attempts: u32,
    },

    /// Tick deserialization failed.
    #[error("Tick parse error from {exchange}: {reason}")]
    ParseError {
        /// Name of the exchange that sent the unparseable tick.
        exchange: String,
        /// Description of the parse failure.
        reason: String,
    },

    /// Feed is stale -- no data received within staleness threshold.
    #[error(
        "Feed '{feed_id}' is stale: last tick was {elapsed_ms}ms ago (threshold: {threshold_ms}ms)"
    )]
    StaleFeed {
        /// Identifier of the stale feed.
        feed_id: String,
        /// Milliseconds since the last tick was received.
        elapsed_ms: u64,
        /// Configured staleness threshold in milliseconds.
        threshold_ms: u64,
    },

    /// Feed identifier is not registered with the health monitor.
    #[error("Unknown feed '{feed_id}': not registered with the health monitor")]
    UnknownFeed {
        /// Identifier of the feed that was not found.
        feed_id: String,
    },

    /// A configuration parameter is invalid.
    ///
    /// Returned by constructors when a parameter violates documented invariants
    /// (e.g. reconnect multiplier < 1.0, zero channel capacity). Distinct from
    /// runtime errors such as [`ConnectionFailed`](Self::ConnectionFailed).
    #[error("Invalid configuration: {reason}")]
    ConfigError {
        /// Description of the configuration violation.
        reason: String,
    },

    /// Order book reconstruction failed.
    #[error("Order book reconstruction failed for '{symbol}': {reason}")]
    BookReconstructionFailed {
        /// Symbol whose order book could not be reconstructed.
        symbol: String,
        /// Description of the reconstruction failure.
        reason: String,
    },

    /// Order book is crossed (bid >= ask).
    #[error("Order book crossed for '{symbol}': best bid {bid} >= best ask {ask}")]
    BookCrossed {
        /// Symbol with the crossed book.
        symbol: String,
        /// Best bid price.
        bid: Decimal,
        /// Best ask price.
        ask: Decimal,
    },

    /// Order book sequence gap detected — one or more deltas were skipped.
    #[error("Sequence gap for '{symbol}': expected {expected}, got {got}")]
    SequenceGap {
        /// Symbol whose delta stream has a gap.
        symbol: String,
        /// The sequence number that was expected.
        expected: u64,
        /// The sequence number that was actually received.
        got: u64,
    },

    /// Backpressure: the downstream channel is full.
    #[error("Backpressure on channel '{channel}': {depth}/{capacity} slots used")]
    Backpressure {
        /// Name or URL of the backpressured channel.
        channel: String,
        /// Current number of items queued.
        depth: usize,
        /// Maximum capacity of the channel.
        capacity: usize,
    },

    /// Invalid exchange format.
    #[error("Unknown exchange format: '{0}'")]
    UnknownExchange(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(String),

    /// WebSocket protocol error.
    #[error("WebSocket error: {0}")]
    WebSocket(String),

    // ── Pipeline-internal errors ─────────────────────────────────────────────
    /// SPSC ring buffer is full; the producer must back off or drop the item.
    ///
    /// This variant is returned by [`crate::ring::SpscRing::push`] when the
    /// buffer has no free slots. It never panics.
    #[error("SPSC ring buffer is full (capacity: {capacity})")]
    RingBufferFull {
        /// Configured usable capacity of the ring buffer (N - 1 slots).
        capacity: usize,
    },

    /// SPSC ring buffer is empty; no item is available for the consumer.
    ///
    /// This variant is returned by [`crate::ring::SpscRing::pop`] when there
    /// are no pending items. Callers should retry or park the consumer thread.
    #[error("SPSC ring buffer is empty")]
    RingBufferEmpty,

    /// An error occurred during OHLCV bar aggregation.
    ///
    /// Wraps structural errors such as receiving a tick for the wrong symbol or
    /// a timeframe with a zero-duration period.
    #[error("OHLCV aggregation error: {reason}")]
    AggregationError {
        /// Description of the aggregation failure.
        reason: String,
    },

    /// An error occurred during coordinate normalization.
    ///
    /// Typically indicates that the normalizer received a value outside the
    /// expected numeric range, or that the rolling window is not yet seeded.
    #[error("Normalization error: {reason}")]
    NormalizationError {
        /// Description of the normalization failure.
        reason: String,
    },

    /// A tick failed structural validation before entering the pipeline.
    ///
    /// Examples: negative price, zero quantity, timestamp in the past beyond
    /// the configured tolerance.
    #[error("Invalid tick: {reason}")]
    InvalidTick {
        /// Description of the validation failure.
        reason: String,
    },

    /// The Lorentz transform configuration is invalid.
    ///
    /// The relativistic velocity parameter beta (v/c) must satisfy 0 <= beta < 1.
    /// A beta of exactly 1 (or above) would produce a division by zero in the
    /// Lorentz factor gamma = 1 / sqrt(1 - beta^2).
    #[error("Lorentz config error: {reason}")]
    LorentzConfigError {
        /// Description of the configuration error.
        reason: String,
    },

    /// An error propagated from the `fin-primitives` crate.
    ///
    /// Allows `?` to be used on `fin_primitives` operations inside `fin-stream`
    /// pipelines without an explicit `.map_err()`.
    #[error("fin-primitives error: {0}")]
    FinPrimitives(String),
}

impl From<fin_primitives::error::FinError> for StreamError {
    fn from(e: fin_primitives::error::FinError) -> Self {
        StreamError::FinPrimitives(e.to_string())
    }
}

impl StreamError {
    /// Returns `true` for errors that originate in the order book subsystem.
    ///
    /// Book errors indicate structural problems with the market data feed
    /// (sequence gaps, crossed books, failed reconstruction) that typically
    /// require a book reset or feed reconnection to resolve.
    pub fn is_book_error(&self) -> bool {
        matches!(
            self,
            StreamError::SequenceGap { .. }
                | StreamError::BookCrossed { .. }
                | StreamError::BookReconstructionFailed { .. }
        )
    }

    /// Returns `true` for errors that arise inside the processing pipeline.
    ///
    /// Pipeline errors indicate internal structural failures — ring buffer full
    /// or empty, aggregation shape mismatch, normalization range violations, or
    /// Lorentz configuration errors. They do not indicate bad input data or
    /// network problems.
    pub fn is_pipeline_error(&self) -> bool {
        matches!(
            self,
            StreamError::RingBufferFull { .. }
                | StreamError::RingBufferEmpty
                | StreamError::AggregationError { .. }
                | StreamError::NormalizationError { .. }
                | StreamError::LorentzConfigError { .. }
        )
    }

    /// Returns `true` for errors that indicate a connection-layer failure.
    ///
    /// Connection errors arise from network problems: initial connection refused,
    /// unexpected disconnection, or reconnection attempts exhausted. Unlike data
    /// errors, these may resolve by reconnecting or waiting.
    pub fn is_connection_error(&self) -> bool {
        matches!(
            self,
            StreamError::ConnectionFailed { .. }
                | StreamError::Disconnected { .. }
                | StreamError::ReconnectExhausted { .. }
        )
    }

    /// Returns `true` for errors that indicate bad data from the exchange.
    ///
    /// Data errors arise from malformed or logically inconsistent market data:
    /// parse failures, invalid tick values, crossed order books, or sequence
    /// gaps. They do not imply a network problem — reconnecting won't help.
    pub fn is_data_error(&self) -> bool {
        matches!(
            self,
            StreamError::ParseError { .. }
                | StreamError::InvalidTick { .. }
                | StreamError::BookCrossed { .. }
                | StreamError::SequenceGap { .. }
                | StreamError::BookReconstructionFailed { .. }
        )
    }

    /// Returns `true` for errors that are transient and worth retrying.
    ///
    /// Transient errors arise from external conditions (network drops, full
    /// buffers, stale feeds) that may resolve on their own. Permanent errors
    /// indicate misconfiguration or invalid data that will not self-heal.
    ///
    /// # Example
    ///
    /// ```rust
    /// use fin_stream::StreamError;
    ///
    /// let e = StreamError::WebSocket("connection reset".into());
    /// assert!(e.is_transient());
    ///
    /// let e = StreamError::ConfigError { reason: "multiplier < 1".into() };
    /// assert!(!e.is_transient());
    /// ```
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            StreamError::ConnectionFailed { .. }
                | StreamError::Disconnected { .. }
                | StreamError::ReconnectExhausted { .. }
                | StreamError::StaleFeed { .. }
                | StreamError::Backpressure { .. }
                | StreamError::RingBufferFull { .. }
                | StreamError::RingBufferEmpty
                | StreamError::Io(_)
                | StreamError::WebSocket(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_failed_display() {
        let e = StreamError::ConnectionFailed {
            url: "wss://example.com".into(),
            reason: "timeout".into(),
        };
        assert!(e.to_string().contains("example.com"));
        assert!(e.to_string().contains("timeout"));
    }

    #[test]
    fn test_disconnected_display() {
        let e = StreamError::Disconnected {
            url: "wss://feed.io".into(),
        };
        assert!(e.to_string().contains("feed.io"));
    }

    #[test]
    fn test_reconnect_exhausted_display() {
        let e = StreamError::ReconnectExhausted {
            url: "wss://x.io".into(),
            attempts: 5,
        };
        assert!(e.to_string().contains("5"));
    }

    #[test]
    fn test_parse_error_display() {
        let e = StreamError::ParseError {
            exchange: "Binance".into(),
            reason: "missing field".into(),
        };
        assert!(e.to_string().contains("Binance"));
    }

    #[test]
    fn test_stale_feed_display() {
        let e = StreamError::StaleFeed {
            feed_id: "BTC-USD".into(),
            elapsed_ms: 5000,
            threshold_ms: 2000,
        };
        assert!(e.to_string().contains("BTC-USD"));
        assert!(e.to_string().contains("5000"));
    }

    #[test]
    fn test_unknown_feed_display() {
        let e = StreamError::UnknownFeed {
            feed_id: "ghost-feed".into(),
        };
        assert!(e.to_string().contains("ghost-feed"));
        assert!(e.to_string().contains("not registered"));
    }

    #[test]
    fn test_config_error_display() {
        let e = StreamError::ConfigError {
            reason: "multiplier must be >= 1.0".into(),
        };
        assert!(e.to_string().contains("multiplier"));
    }

    #[test]
    fn test_book_reconstruction_failed_display() {
        let e = StreamError::BookReconstructionFailed {
            symbol: "ETH-USD".into(),
            reason: "gap in sequence".into(),
        };
        assert!(e.to_string().contains("ETH-USD"));
    }

    #[test]
    fn test_book_crossed_display() {
        let e = StreamError::BookCrossed {
            symbol: "BTC-USD".into(),
            bid: Decimal::from(50001u32),
            ask: Decimal::from(50000u32),
        };
        assert!(e.to_string().contains("crossed"));
        assert!(e.to_string().contains("BTC-USD"));
    }

    #[test]
    fn test_sequence_gap_display() {
        let e = StreamError::SequenceGap {
            symbol: "BTC-USD".into(),
            expected: 5,
            got: 7,
        };
        assert!(e.to_string().contains("5"));
        assert!(e.to_string().contains("7"));
    }

    #[test]
    fn test_backpressure_display() {
        let e = StreamError::Backpressure {
            channel: "ticks".into(),
            depth: 1000,
            capacity: 1000,
        };
        assert!(e.to_string().contains("1000"));
    }

    #[test]
    fn test_unknown_exchange_display() {
        let e = StreamError::UnknownExchange("Kraken".into());
        assert!(e.to_string().contains("Kraken"));
    }

    #[test]
    fn test_ring_buffer_full_display() {
        let e = StreamError::RingBufferFull { capacity: 1024 };
        assert!(e.to_string().contains("1024"));
        assert!(e.to_string().contains("full"));
    }

    #[test]
    fn test_ring_buffer_empty_display() {
        let e = StreamError::RingBufferEmpty;
        assert!(e.to_string().contains("empty"));
    }

    #[test]
    fn test_aggregation_error_display() {
        let e = StreamError::AggregationError {
            reason: "wrong symbol".into(),
        };
        assert!(e.to_string().contains("wrong symbol"));
    }

    #[test]
    fn test_normalization_error_display() {
        let e = StreamError::NormalizationError {
            reason: "window not seeded".into(),
        };
        assert!(e.to_string().contains("window not seeded"));
    }

    #[test]
    fn test_invalid_tick_display() {
        let e = StreamError::InvalidTick {
            reason: "negative price".into(),
        };
        assert!(e.to_string().contains("negative price"));
    }

    #[test]
    fn test_lorentz_config_error_display() {
        let e = StreamError::LorentzConfigError {
            reason: "beta >= 1".into(),
        };
        assert!(e.to_string().contains("beta >= 1"));
    }

    #[test]
    fn test_is_transient_websocket_is_transient() {
        assert!(StreamError::WebSocket("reset".into()).is_transient());
        assert!(StreamError::ConnectionFailed {
            url: "wss://x.io".into(),
            reason: "timeout".into()
        }
        .is_transient());
        assert!(StreamError::RingBufferFull { capacity: 8 }.is_transient());
        assert!(StreamError::RingBufferEmpty.is_transient());
    }

    #[test]
    fn test_is_data_error_parse_and_book_errors() {
        assert!(StreamError::ParseError {
            exchange: "Binance".into(),
            reason: "bad field".into()
        }
        .is_data_error());
        assert!(StreamError::InvalidTick {
            reason: "neg price".into()
        }
        .is_data_error());
        assert!(StreamError::BookCrossed {
            symbol: "BTC-USD".into(),
            bid: Decimal::from(1u32),
            ask: Decimal::from(1u32)
        }
        .is_data_error());
        assert!(StreamError::SequenceGap {
            symbol: "BTC-USD".into(),
            expected: 1,
            got: 3
        }
        .is_data_error());
    }

    #[test]
    fn test_is_data_error_connectivity_errors_are_not_data() {
        assert!(!StreamError::WebSocket("reset".into()).is_data_error());
        assert!(!StreamError::ConnectionFailed {
            url: "wss://x.io".into(),
            reason: "timeout".into()
        }
        .is_data_error());
        assert!(!StreamError::ConfigError {
            reason: "bad param".into()
        }
        .is_data_error());
    }

    #[test]
    fn test_is_transient_config_errors_are_not_transient() {
        assert!(!StreamError::ConfigError {
            reason: "bad param".into()
        }
        .is_transient());
        assert!(!StreamError::ParseError {
            exchange: "Binance".into(),
            reason: "bad field".into()
        }
        .is_transient());
        assert!(!StreamError::InvalidTick {
            reason: "neg price".into()
        }
        .is_transient());
        assert!(!StreamError::LorentzConfigError {
            reason: "beta>=1".into()
        }
        .is_transient());
    }

    // ── StreamError::is_connection_error ─────────────────────────────────────

    #[test]
    fn test_is_connection_error_connection_variants() {
        assert!(StreamError::ConnectionFailed {
            url: "wss://x.io".into(),
            reason: "refused".into()
        }
        .is_connection_error());
        assert!(StreamError::Disconnected {
            url: "wss://x.io".into()
        }
        .is_connection_error());
        assert!(StreamError::ReconnectExhausted {
            url: "wss://x.io".into(),
            attempts: 5
        }
        .is_connection_error());
    }

    #[test]
    fn test_is_connection_error_data_errors_are_not_connection() {
        assert!(!StreamError::ParseError {
            exchange: "Binance".into(),
            reason: "bad json".into()
        }
        .is_connection_error());
        assert!(!StreamError::ConfigError {
            reason: "bad config".into()
        }
        .is_connection_error());
        assert!(!StreamError::BookCrossed {
            symbol: "BTC-USD".into(),
            bid: rust_decimal_macros::dec!(100),
            ask: rust_decimal_macros::dec!(99),
        }
        .is_connection_error());
    }

    // ── StreamError::is_pipeline_error ───────────────────────────────────────

    #[test]
    fn test_is_pipeline_error_ring_and_aggregation_variants() {
        assert!(StreamError::RingBufferFull { capacity: 8 }.is_pipeline_error());
        assert!(StreamError::RingBufferEmpty.is_pipeline_error());
        assert!(StreamError::AggregationError {
            reason: "wrong symbol".into()
        }
        .is_pipeline_error());
        assert!(StreamError::NormalizationError {
            reason: "out of range".into()
        }
        .is_pipeline_error());
        assert!(StreamError::LorentzConfigError {
            reason: "beta>=1".into()
        }
        .is_pipeline_error());
    }

    #[test]
    fn test_is_pipeline_error_connection_errors_are_not_pipeline() {
        assert!(!StreamError::ConnectionFailed {
            url: "wss://x.io".into(),
            reason: "refused".into()
        }
        .is_pipeline_error());
        assert!(!StreamError::ParseError {
            exchange: "Binance".into(),
            reason: "bad json".into()
        }
        .is_pipeline_error());
    }
}
