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
    /// Returns `true` for errors that are not recoverable without a code or
    /// configuration change.
    ///
    /// Fatal errors indicate a programming mistake or invalid configuration:
    /// misconfigured parameters, unknown exchanges, exhausted reconnect budgets,
    /// or corrupted book state that cannot be repaired by retrying.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            StreamError::ConfigError { .. }
                | StreamError::LorentzConfigError { .. }
                | StreamError::UnknownExchange(_)
                | StreamError::ReconnectExhausted { .. }
                | StreamError::UnknownFeed { .. }
                | StreamError::BookReconstructionFailed { .. }
        )
    }

    /// Source subsystem for this error: `"ws"`, `"book"`, `"ring"`,
    /// `"aggregator"`, `"session"`, `"lorentz"`, or `"other"`.
    ///
    /// Useful for routing errors to subsystem-specific metrics or alert
    /// channels without pattern-matching every variant.
    pub fn source_module(&self) -> &'static str {
        match self {
            StreamError::ConnectionFailed { .. }
            | StreamError::Disconnected { .. }
            | StreamError::ReconnectExhausted { .. }
            | StreamError::WebSocket(_) => "ws",
            StreamError::BookCrossed { .. }
            | StreamError::BookReconstructionFailed { .. }
            | StreamError::SequenceGap { .. } => "book",
            StreamError::RingBufferFull { .. } | StreamError::RingBufferEmpty => "ring",
            StreamError::AggregationError { .. } | StreamError::NormalizationError { .. } => {
                "aggregator"
            }
            StreamError::StaleFeed { .. } | StreamError::UnknownFeed { .. } => "session",
            StreamError::LorentzConfigError { .. } => "lorentz",
            _ => "other",
        }
    }

    /// Returns `true` if this error can potentially be resolved by retrying —
    /// the inverse of [`is_fatal`](Self::is_fatal).
    /// Returns `true` for errors that can be recovered from without a full reconnect.
    pub fn is_recoverable(&self) -> bool {
        !self.is_fatal()
    }

    /// Returns `true` for errors that originate from a network connection.
    ///
    /// Network errors include WebSocket protocol failures, connection drops, and
    /// reconnect exhaustion. They are distinct from data errors (bad book state,
    /// bad tick data) or config errors.
    pub fn is_network_error(&self) -> bool {
        matches!(
            self,
            StreamError::ConnectionFailed { .. }
                | StreamError::Disconnected { .. }
                | StreamError::ReconnectExhausted { .. }
                | StreamError::WebSocket(_)
                | StreamError::Io(_)
        )
    }

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

    /// Returns the enum variant name as a static string.
    ///
    /// Useful for structured logging and metrics without allocating a dynamic
    /// string from the `Display` impl.
    pub fn variant_name(&self) -> &'static str {
        match self {
            StreamError::ConnectionFailed { .. } => "ConnectionFailed",
            StreamError::Disconnected { .. } => "Disconnected",
            StreamError::ReconnectExhausted { .. } => "ReconnectExhausted",
            StreamError::ParseError { .. } => "ParseError",
            StreamError::StaleFeed { .. } => "StaleFeed",
            StreamError::UnknownFeed { .. } => "UnknownFeed",
            StreamError::ConfigError { .. } => "ConfigError",
            StreamError::BookReconstructionFailed { .. } => "BookReconstructionFailed",
            StreamError::BookCrossed { .. } => "BookCrossed",
            StreamError::SequenceGap { .. } => "SequenceGap",
            StreamError::Backpressure { .. } => "Backpressure",
            StreamError::UnknownExchange(_) => "UnknownExchange",
            StreamError::Io(_) => "Io",
            StreamError::WebSocket(_) => "WebSocket",
            StreamError::RingBufferFull { .. } => "RingBufferFull",
            StreamError::RingBufferEmpty => "RingBufferEmpty",
            StreamError::AggregationError { .. } => "AggregationError",
            StreamError::NormalizationError { .. } => "NormalizationError",
            StreamError::InvalidTick { .. } => "InvalidTick",
            StreamError::LorentzConfigError { .. } => "LorentzConfigError",
            StreamError::FinPrimitives(_) => "FinPrimitives",
        }
    }

    /// Returns `true` if this is a configuration error.
    pub fn is_config_error(&self) -> bool {
        matches!(self, StreamError::ConfigError { .. })
    }

    /// Returns `true` if this error relates to a feed (stale or unknown).
    pub fn is_feed_error(&self) -> bool {
        matches!(
            self,
            StreamError::StaleFeed { .. } | StreamError::UnknownFeed { .. }
        )
    }

    /// Returns `true` if this is a tick parse error.
    pub fn is_parse_error(&self) -> bool {
        matches!(self, StreamError::ParseError { .. })
    }

    /// Returns `true` if this is a book reconstruction failure.
    pub fn is_book_reconstruction_error(&self) -> bool {
        matches!(self, StreamError::BookReconstructionFailed { .. })
    }

    /// Returns `true` if this error is related to computation or physics transforms.
    ///
    /// Covers `LorentzConfigError` (invalid beta value).
    pub fn is_computation_error(&self) -> bool {
        matches!(self, StreamError::LorentzConfigError { .. })
    }

    /// Returns `true` if this error is related to the ring buffer (full or empty).
    pub fn is_ring_error(&self) -> bool {
        matches!(
            self,
            StreamError::RingBufferFull { .. } | StreamError::RingBufferEmpty
        )
    }

    /// Human-readable category string for this error.
    ///
    /// Returns one of `"connection"`, `"data"`, `"pipeline"`, `"book"`,
    /// `"config"`, or `"other"`. Useful for metrics labels and structured
    /// logging without pattern-matching every variant.
    pub fn category(&self) -> &'static str {
        if self.is_connection_error() {
            "connection"
        } else if self.is_data_error() {
            "data"
        } else if self.is_pipeline_error() {
            "pipeline"
        } else if self.is_book_error() {
            "book"
        } else if matches!(self, StreamError::ConfigError { .. }) {
            "config"
        } else {
            "other"
        }
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

    /// The primary symbol, URL, or feed identifier associated with this error,
    /// if any.
    ///
    /// Returns `None` for errors that are not tied to a specific identifier
    /// (e.g. ring buffer errors, pipeline errors without a symbol field).
    /// Useful for routing errors to per-symbol dashboards or alert channels.
    pub fn affected_symbol(&self) -> Option<&str> {
        match self {
            StreamError::ConnectionFailed { url, .. }
            | StreamError::Disconnected { url }
            | StreamError::ReconnectExhausted { url, .. } => Some(url.as_str()),
            StreamError::BookCrossed { symbol, .. }
            | StreamError::BookReconstructionFailed { symbol, .. }
            | StreamError::SequenceGap { symbol, .. } => Some(symbol.as_str()),
            StreamError::StaleFeed { feed_id, .. }
            | StreamError::UnknownFeed { feed_id } => Some(feed_id.as_str()),
            _ => None,
        }
    }

    /// Returns `true` if this is a sequence gap error (missed delta).
    pub fn is_sequence_gap(&self) -> bool {
        matches!(self, StreamError::SequenceGap { .. })
    }

    /// Returns `true` for errors that indicate a data integrity violation:
    /// a crossed order book or a sequence gap in the delta stream.
    ///
    /// These errors mean the feed's internal state may be corrupted and
    /// typically require a book reset or reconnect.
    pub fn is_data_integrity_error(&self) -> bool {
        matches!(
            self,
            StreamError::BookCrossed { .. } | StreamError::SequenceGap { .. }
        )
    }

    /// Returns `true` if this error is associated with a specific symbol or feed ID.
    ///
    /// Covers book errors and feed-health errors that carry an identifier.
    pub fn is_symbol_error(&self) -> bool {
        matches!(
            self,
            StreamError::BookCrossed { .. }
                | StreamError::BookReconstructionFailed { .. }
                | StreamError::SequenceGap { .. }
                | StreamError::StaleFeed { .. }
                | StreamError::UnknownFeed { .. }
        )
    }

    /// Returns a numeric error code for this variant.
    ///
    /// Codes are grouped by category:
    /// - 1xxx: connection errors
    /// - 2xxx: feed/parse errors
    /// - 3xxx: configuration errors
    /// - 4xxx: book errors
    /// - 5xxx: backpressure/buffer errors
    /// - 6xxx: data errors
    /// - 7xxx: computation errors
    pub fn to_error_code(&self) -> u32 {
        match self {
            StreamError::ConnectionFailed { .. } => 1001,
            StreamError::Disconnected { .. } => 1002,
            StreamError::ReconnectExhausted { .. } => 1003,
            StreamError::Io(_) => 1004,
            StreamError::WebSocket(_) => 1005,
            StreamError::ParseError { .. } => 2001,
            StreamError::UnknownExchange(_) => 2002,
            StreamError::StaleFeed { .. } => 2003,
            StreamError::UnknownFeed { .. } => 2004,
            StreamError::ConfigError { .. } => 3001,
            StreamError::BookReconstructionFailed { .. } => 4001,
            StreamError::BookCrossed { .. } => 4002,
            StreamError::SequenceGap { .. } => 4003,
            StreamError::Backpressure { .. } => 5001,
            StreamError::RingBufferFull { .. } => 5002,
            StreamError::RingBufferEmpty => 5003,
            StreamError::AggregationError { .. } => 6001,
            StreamError::NormalizationError { .. } => 6002,
            StreamError::InvalidTick { .. } => 6003,
            StreamError::FinPrimitives(_) => 6004,
            StreamError::LorentzConfigError { .. } => 7001,
        }
    }

    /// Returns `true` if this error contains a URL field.
    pub fn has_url(&self) -> bool {
        matches!(self, StreamError::ConnectionFailed { .. })
    }

    /// Returns the high-level category code (thousands digit of `to_error_code`).
    ///
    /// - 1: connection, 2: feed/parse, 3: config, 4: book, 5: buffer, 6: data, 7: computation
    pub fn error_category_code(&self) -> u32 {
        self.to_error_code() / 1000
    }

    /// Returns `true` for ring-buffer and backpressure errors (category 5xxx).
    pub fn is_buffer_error(&self) -> bool {
        matches!(
            self,
            StreamError::Backpressure { .. }
                | StreamError::RingBufferFull { .. }
                | StreamError::RingBufferEmpty
        )
    }

    /// Returns `true` if this is a normalization or aggregation error.
    pub fn is_normalization_error(&self) -> bool {
        matches!(self, StreamError::NormalizationError { .. })
    }

    /// Returns `true` if this is an aggregation error.
    pub fn is_agg_error(&self) -> bool {
        matches!(self, StreamError::AggregationError { .. })
    }

    /// Returns `true` if this is a sequence gap error.
    pub fn is_sequence_error(&self) -> bool {
        matches!(self, StreamError::SequenceGap { .. })
    }

    /// Numeric severity level: 1 = informational, 2 = warning, 3 = error, 4 = critical.
    ///
    /// - 4 (critical): `ReconnectExhausted`
    /// - 3 (error): connection, book, and lorentz config errors
    /// - 2 (warning): stale feed, sequence gap, backpressure, buffer full/empty
    /// - 1 (informational): parse, normalization, aggregation, unknown feed, invalid tick
    pub fn severity_level(&self) -> u8 {
        match self {
            StreamError::ReconnectExhausted { .. } => 4,
            StreamError::ConnectionFailed { .. }
            | StreamError::Disconnected { .. }
            | StreamError::BookReconstructionFailed { .. }
            | StreamError::BookCrossed { .. }
            | StreamError::LorentzConfigError { .. } => 3,
            StreamError::StaleFeed { .. }
            | StreamError::SequenceGap { .. }
            | StreamError::Backpressure { .. }
            | StreamError::RingBufferFull { .. }
            | StreamError::RingBufferEmpty => 2,
            _ => 1,
        }
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

    // ── StreamError::category ─────────────────────────────────────────────────

    #[test]
    fn test_category_connection_errors() {
        assert_eq!(
            StreamError::ConnectionFailed { url: "u".into(), reason: "r".into() }.category(),
            "connection"
        );
        assert_eq!(
            StreamError::Disconnected { url: "u".into() }.category(),
            "connection"
        );
    }

    #[test]
    fn test_category_data_errors() {
        assert_eq!(
            StreamError::ParseError { exchange: "B".into(), reason: "r".into() }.category(),
            "data"
        );
        assert_eq!(
            StreamError::InvalidTick { reason: "neg price".into() }.category(),
            "data"
        );
    }

    #[test]
    fn test_category_pipeline_errors() {
        assert_eq!(StreamError::RingBufferFull { capacity: 8 }.category(), "pipeline");
        assert_eq!(StreamError::RingBufferEmpty.category(), "pipeline");
    }

    #[test]
    fn test_category_config_errors() {
        assert_eq!(
            StreamError::ConfigError { reason: "bad param".into() }.category(),
            "config"
        );
    }

    // ── StreamError::is_fatal ─────────────────────────────────────────────────

    #[test]
    fn test_is_fatal_config_error() {
        assert!(StreamError::ConfigError { reason: "bad".into() }.is_fatal());
    }

    #[test]
    fn test_is_fatal_unknown_exchange() {
        assert!(StreamError::UnknownExchange("Kraken".into()).is_fatal());
    }

    #[test]
    fn test_is_fatal_reconnect_exhausted() {
        assert!(StreamError::ReconnectExhausted { url: "wss://x".into(), attempts: 5 }.is_fatal());
    }

    #[test]
    fn test_is_fatal_lorentz_config() {
        assert!(StreamError::LorentzConfigError { reason: "beta>=1".into() }.is_fatal());
    }

    #[test]
    fn test_is_fatal_parse_error_is_not_fatal() {
        assert!(!StreamError::ParseError {
            exchange: "Binance".into(),
            reason: "bad json".into()
        }.is_fatal());
    }

    #[test]
    fn test_is_fatal_ring_buffer_full_is_not_fatal() {
        assert!(!StreamError::RingBufferFull { capacity: 8 }.is_fatal());
    }

    // ── StreamError::is_recoverable ───────────────────────────────────────────

    #[test]
    fn test_is_recoverable_inverse_of_is_fatal() {
        let fatal = StreamError::ConfigError { reason: "bad".into() };
        let transient = StreamError::RingBufferFull { capacity: 8 };
        assert!(fatal.is_fatal() && !fatal.is_recoverable());
        assert!(!transient.is_fatal() && transient.is_recoverable());
    }

    #[test]
    fn test_is_recoverable_connection_failed() {
        assert!(StreamError::ConnectionFailed {
            url: "wss://x".into(),
            reason: "timeout".into()
        }.is_recoverable());
    }

    // ── StreamError::source_module ────────────────────────────────────────────

    #[test]
    fn test_source_module_ws_variants() {
        assert_eq!(StreamError::WebSocket("err".into()).source_module(), "ws");
        assert_eq!(
            StreamError::ConnectionFailed { url: "u".into(), reason: "r".into() }.source_module(),
            "ws"
        );
        assert_eq!(
            StreamError::Disconnected { url: "u".into() }.source_module(),
            "ws"
        );
    }

    #[test]
    fn test_source_module_book_variants() {
        assert_eq!(
            StreamError::BookCrossed {
                symbol: "BTC-USD".into(),
                bid: Decimal::from(100u32),
                ask: Decimal::from(99u32),
            }
            .source_module(),
            "book"
        );
        assert_eq!(
            StreamError::SequenceGap { symbol: "X".into(), expected: 1, got: 3 }.source_module(),
            "book"
        );
    }

    #[test]
    fn test_source_module_ring_variants() {
        assert_eq!(StreamError::RingBufferFull { capacity: 8 }.source_module(), "ring");
        assert_eq!(StreamError::RingBufferEmpty.source_module(), "ring");
    }

    #[test]
    fn test_source_module_aggregator_variants() {
        assert_eq!(
            StreamError::AggregationError { reason: "bad".into() }.source_module(),
            "aggregator"
        );
        assert_eq!(
            StreamError::NormalizationError { reason: "bad".into() }.source_module(),
            "aggregator"
        );
    }

    #[test]
    fn test_source_module_session_variants() {
        assert_eq!(
            StreamError::StaleFeed { feed_id: "X".into(), elapsed_ms: 1, threshold_ms: 1 }
                .source_module(),
            "session"
        );
        assert_eq!(
            StreamError::UnknownFeed { feed_id: "X".into() }.source_module(),
            "session"
        );
    }

    #[test]
    fn test_source_module_lorentz_variant() {
        assert_eq!(
            StreamError::LorentzConfigError { reason: "bad".into() }.source_module(),
            "lorentz"
        );
    }

    #[test]
    fn test_source_module_other_fallback() {
        assert_eq!(StreamError::UnknownExchange("X".into()).source_module(), "other");
        assert_eq!(
            StreamError::ConfigError { reason: "bad".into() }.source_module(),
            "other"
        );
    }

    #[test]
    fn test_is_network_error_ws_variants() {
        assert!(StreamError::WebSocket("dropped".into()).is_network_error());
        assert!(StreamError::ConnectionFailed {
            url: "wss://x".into(),
            reason: "timeout".into()
        }
        .is_network_error());
        assert!(StreamError::Disconnected { url: "wss://x".into() }.is_network_error());
    }

    #[test]
    fn test_is_network_error_false_for_config() {
        assert!(!StreamError::ConfigError { reason: "bad".into() }.is_network_error());
        assert!(!StreamError::RingBufferFull { capacity: 8 }.is_network_error());
        assert!(!StreamError::BookCrossed {
            symbol: "BTC".into(),
            bid: Decimal::from(100u32),
            ask: Decimal::from(99u32),
        }
        .is_network_error());
    }

    // ── StreamError::variant_name ─────────────────────────────────────────────

    #[test]
    fn test_variant_name_connection_failed() {
        assert_eq!(
            StreamError::ConnectionFailed { url: "u".into(), reason: "r".into() }.variant_name(),
            "ConnectionFailed"
        );
    }

    #[test]
    fn test_variant_name_ring_buffer_empty() {
        assert_eq!(StreamError::RingBufferEmpty.variant_name(), "RingBufferEmpty");
    }

    #[test]
    fn test_variant_name_parse_error() {
        assert_eq!(
            StreamError::ParseError { exchange: "B".into(), reason: "bad".into() }.variant_name(),
            "ParseError"
        );
    }

    #[test]
    fn test_variant_name_book_crossed() {
        assert_eq!(
            StreamError::BookCrossed {
                symbol: "X".into(),
                bid: Decimal::from(1u32),
                ask: Decimal::from(1u32),
            }
            .variant_name(),
            "BookCrossed"
        );
    }

    #[test]
    fn test_variant_name_lorentz_config_error() {
        assert_eq!(
            StreamError::LorentzConfigError { reason: "bad".into() }.variant_name(),
            "LorentzConfigError"
        );
    }

    // ── StreamError::is_config_error ──────────────────────────────────────────

    #[test]
    fn test_is_config_error_true_for_config_error() {
        let e = StreamError::ConfigError { reason: "bad config".into() };
        assert!(e.is_config_error());
    }

    #[test]
    fn test_is_config_error_false_for_connection_failed() {
        let e = StreamError::ConnectionFailed { url: "ws://x".into(), reason: "timeout".into() };
        assert!(!e.is_config_error());
    }

    #[test]
    fn test_is_config_error_false_for_ring_buffer_empty() {
        assert!(!StreamError::RingBufferEmpty.is_config_error());
    }

    // ── StreamError::is_feed_error ────────────────────────────────────────────

    #[test]
    fn test_is_feed_error_true_for_stale_feed() {
        let e = StreamError::StaleFeed {
            feed_id: "btc".into(),
            elapsed_ms: 5_000,
            threshold_ms: 1_000,
        };
        assert!(e.is_feed_error());
    }

    #[test]
    fn test_is_feed_error_true_for_unknown_feed() {
        let e = StreamError::UnknownFeed { feed_id: "xyz".into() };
        assert!(e.is_feed_error());
    }

    #[test]
    fn test_is_feed_error_false_for_ring_buffer_empty() {
        assert!(!StreamError::RingBufferEmpty.is_feed_error());
    }

    // ── StreamError::is_parse_error ───────────────────────────────────────────

    #[test]
    fn test_is_parse_error_true_for_parse_error() {
        let e = StreamError::ParseError {
            exchange: "binance".into(),
            reason: "unexpected field".into(),
        };
        assert!(e.is_parse_error());
    }

    #[test]
    fn test_is_parse_error_false_for_connection_failed() {
        let e = StreamError::ConnectionFailed { url: "ws://x".into(), reason: "timeout".into() };
        assert!(!e.is_parse_error());
    }

    #[test]
    fn test_is_parse_error_false_for_stale_feed() {
        let e = StreamError::StaleFeed { feed_id: "x".into(), elapsed_ms: 1, threshold_ms: 1 };
        assert!(!e.is_parse_error());
    }

    // ── StreamError::is_book_reconstruction_error ─────────────────────────────

    #[test]
    fn test_is_book_reconstruction_error_true() {
        let e = StreamError::BookReconstructionFailed {
            symbol: "BTC-USD".into(),
            reason: "checksum mismatch".into(),
        };
        assert!(e.is_book_reconstruction_error());
    }

    #[test]
    fn test_is_book_reconstruction_error_false_for_book_crossed() {
        let e = StreamError::BookCrossed {
            symbol: "BTC-USD".into(),
            bid: rust_decimal_macros::dec!(101),
            ask: rust_decimal_macros::dec!(99),
        };
        assert!(!e.is_book_reconstruction_error());
    }

    #[test]
    fn test_is_book_reconstruction_error_false_for_ring_buffer_empty() {
        assert!(!StreamError::RingBufferEmpty.is_book_reconstruction_error());
    }

    // --- StreamError::affected_symbol ---

    #[test]
    fn test_affected_symbol_returns_url_for_connection_failed() {
        let e = StreamError::ConnectionFailed {
            url: "wss://feed.io".into(),
            reason: "refused".into(),
        };
        assert_eq!(e.affected_symbol(), Some("wss://feed.io"));
    }

    #[test]
    fn test_affected_symbol_returns_symbol_for_book_crossed() {
        let e = StreamError::BookCrossed {
            symbol: "ETH-USD".into(),
            bid: rust_decimal_macros::dec!(101),
            ask: rust_decimal_macros::dec!(99),
        };
        assert_eq!(e.affected_symbol(), Some("ETH-USD"));
    }

    #[test]
    fn test_affected_symbol_returns_feed_id_for_stale_feed() {
        let e = StreamError::StaleFeed {
            feed_id: "BTC-USD".into(),
            elapsed_ms: 5_000,
            threshold_ms: 2_000,
        };
        assert_eq!(e.affected_symbol(), Some("BTC-USD"));
    }

    #[test]
    fn test_affected_symbol_none_for_ring_buffer_errors() {
        assert!(StreamError::RingBufferEmpty.affected_symbol().is_none());
        assert!(StreamError::RingBufferFull { capacity: 8 }.affected_symbol().is_none());
    }

    // --- StreamError::is_sequence_gap ---

    #[test]
    fn test_is_sequence_gap_true_for_sequence_gap() {
        let e = StreamError::SequenceGap {
            symbol: "BTC-USD".into(),
            expected: 10,
            got: 15,
        };
        assert!(e.is_sequence_gap());
    }

    #[test]
    fn test_is_sequence_gap_false_for_book_crossed() {
        let e = StreamError::BookCrossed {
            symbol: "BTC-USD".into(),
            bid: rust_decimal_macros::dec!(101),
            ask: rust_decimal_macros::dec!(99),
        };
        assert!(!e.is_sequence_gap());
    }

    #[test]
    fn test_is_sequence_gap_false_for_ring_buffer_empty() {
        assert!(!StreamError::RingBufferEmpty.is_sequence_gap());
    }

    // ── StreamError::is_symbol_error ─────────────────────────────────────────

    #[test]
    fn test_is_symbol_error_true_for_book_crossed() {
        let e = StreamError::BookCrossed {
            symbol: "X".into(),
            bid: rust_decimal_macros::dec!(100),
            ask: rust_decimal_macros::dec!(99),
        };
        assert!(e.is_symbol_error());
    }

    #[test]
    fn test_is_symbol_error_true_for_stale_feed() {
        let e = StreamError::StaleFeed {
            feed_id: "feed1".into(),
            elapsed_ms: 10_000,
            threshold_ms: 5_000,
        };
        assert!(e.is_symbol_error());
    }

    #[test]
    fn test_is_symbol_error_false_for_io_error() {
        assert!(!StreamError::Io("disk error".into()).is_symbol_error());
    }

    #[test]
    fn test_is_symbol_error_false_for_config_error() {
        assert!(!StreamError::ConfigError { reason: "bad".into() }.is_symbol_error());
    }

    // ── StreamError::to_error_code ────────────────────────────────────────────

    #[test]
    fn test_to_error_code_connection_failed_is_1001() {
        let e = StreamError::ConnectionFailed {
            url: "wss://example.com".into(),
            reason: "timeout".into(),
        };
        assert_eq!(e.to_error_code(), 1001);
    }

    #[test]
    fn test_to_error_code_book_crossed_is_4002() {
        let e = StreamError::BookCrossed {
            symbol: "X".into(),
            bid: rust_decimal_macros::dec!(100),
            ask: rust_decimal_macros::dec!(99),
        };
        assert_eq!(e.to_error_code(), 4002);
    }

    #[test]
    fn test_to_error_code_ring_buffer_empty_is_5003() {
        assert_eq!(StreamError::RingBufferEmpty.to_error_code(), 5003);
    }

    #[test]
    fn test_to_error_code_lorentz_is_7001() {
        let e = StreamError::LorentzConfigError { reason: "bad beta".into() };
        assert_eq!(e.to_error_code(), 7001);
    }

    // --- StreamError::is_data_integrity_error ---
    #[test]
    fn test_is_data_integrity_error_true_for_book_crossed() {
        let e = StreamError::BookCrossed {
            symbol: "AAPL".into(),
            bid: rust_decimal_macros::dec!(101),
            ask: rust_decimal_macros::dec!(100),
        };
        assert!(e.is_data_integrity_error());
    }

    #[test]
    fn test_is_data_integrity_error_true_for_sequence_gap() {
        let e = StreamError::SequenceGap {
            symbol: "BTCUSDT".into(),
            expected: 42,
            got: 50,
        };
        assert!(e.is_data_integrity_error());
    }

    #[test]
    fn test_is_data_integrity_error_false_for_connection_failed() {
        let e = StreamError::ConnectionFailed {
            url: "wss://example.com".into(),
            reason: "timeout".into(),
        };
        assert!(!e.is_data_integrity_error());
    }

    #[test]
    fn test_is_data_integrity_error_false_for_stale_feed() {
        let e = StreamError::StaleFeed {
            feed_id: "kraken".into(),
            elapsed_ms: 9_000,
            threshold_ms: 5_000,
        };
        assert!(!e.is_data_integrity_error());
    }

    // --- StreamError::is_connection_error ---
    #[test]
    fn test_is_connection_error_true_for_connection_failed() {
        let e = StreamError::ConnectionFailed { url: "ws://x".into(), reason: "refused".into() };
        assert!(e.is_connection_error());
    }

    #[test]
    fn test_is_connection_error_true_for_disconnected() {
        let e = StreamError::Disconnected { url: "ws://x".into() };
        assert!(e.is_connection_error());
    }

    #[test]
    fn test_is_connection_error_true_for_reconnect_exhausted() {
        let e = StreamError::ReconnectExhausted { url: "ws://x".into(), attempts: 5 };
        assert!(e.is_connection_error());
    }

    #[test]
    fn test_is_connection_error_false_for_book_crossed() {
        let e = StreamError::BookCrossed {
            symbol: "AAPL".into(),
            bid: rust_decimal_macros::dec!(101),
            ask: rust_decimal_macros::dec!(100),
        };
        assert!(!e.is_connection_error());
    }

    // --- StreamError::is_computation_error / is_ring_error ---
    #[test]
    fn test_is_computation_error_true_for_lorentz() {
        let e = StreamError::LorentzConfigError { reason: "beta >= 1".into() };
        assert!(e.is_computation_error());
    }

    #[test]
    fn test_is_computation_error_false_for_connection() {
        let e = StreamError::ConnectionFailed { url: "ws://x".into(), reason: "refused".into() };
        assert!(!e.is_computation_error());
    }

    #[test]
    fn test_is_ring_error_true_for_ring_buffer_full() {
        let e = StreamError::RingBufferFull { capacity: 10 };
        assert!(e.is_ring_error());
    }

    #[test]
    fn test_is_ring_error_true_for_ring_buffer_empty() {
        assert!(StreamError::RingBufferEmpty.is_ring_error());
    }

    #[test]
    fn test_is_ring_error_false_for_other_errors() {
        let e = StreamError::ConnectionFailed { url: "ws://x".into(), reason: "timeout".into() };
        assert!(!e.is_ring_error());
    }

    // ── StreamError::has_url / error_category_code ──────────────────────────

    #[test]
    fn test_has_url_true_for_connection_failed() {
        let e = StreamError::ConnectionFailed { url: "wss://x".into(), reason: "timeout".into() };
        assert!(e.has_url());
    }

    #[test]
    fn test_has_url_false_for_other_errors() {
        assert!(!StreamError::RingBufferEmpty.has_url());
        assert!(!StreamError::ConfigError { reason: "bad".into() }.has_url());
    }

    #[test]
    fn test_error_category_code_connection_is_1() {
        let e = StreamError::ConnectionFailed { url: "wss://x".into(), reason: "x".into() };
        assert_eq!(e.error_category_code(), 1);
    }

    #[test]
    fn test_error_category_code_book_is_4() {
        let e = StreamError::BookCrossed {
            symbol: "X".into(),
            bid: rust_decimal_macros::dec!(100),
            ask: rust_decimal_macros::dec!(99),
        };
        assert_eq!(e.error_category_code(), 4);
    }

    #[test]
    fn test_error_category_code_lorentz_is_7() {
        let e = StreamError::LorentzConfigError { reason: "bad beta".into() };
        assert_eq!(e.error_category_code(), 7);
    }

    // ── StreamError::is_buffer_error ────────────────────────────────────────

    #[test]
    fn test_is_buffer_error_true_for_ring_buffer_empty() {
        assert!(StreamError::RingBufferEmpty.is_buffer_error());
    }

    #[test]
    fn test_is_buffer_error_true_for_backpressure() {
        assert!(StreamError::Backpressure { channel: "x".into(), depth: 8, capacity: 8 }.is_buffer_error());
    }

    #[test]
    fn test_is_buffer_error_true_for_ring_buffer_full() {
        assert!(StreamError::RingBufferFull { capacity: 8 }.is_buffer_error());
    }

    #[test]
    fn test_is_buffer_error_false_for_connection_failed() {
        let e = StreamError::ConnectionFailed { url: "wss://x".into(), reason: "x".into() };
        assert!(!e.is_buffer_error());
    }

    // ── StreamError::is_normalization_error / is_agg_error ──────────────────

    #[test]
    fn test_is_normalization_error_true() {
        let e = StreamError::NormalizationError { reason: "empty window".into() };
        assert!(e.is_normalization_error());
    }

    #[test]
    fn test_is_normalization_error_false_for_parse_error() {
        let e = StreamError::ParseError { exchange: "binance".into(), reason: "bad json".into() };
        assert!(!e.is_normalization_error());
    }

    #[test]
    fn test_is_agg_error_true() {
        let e = StreamError::AggregationError { reason: "mismatched symbol".into() };
        assert!(e.is_agg_error());
    }

    #[test]
    fn test_is_agg_error_false_for_other() {
        assert!(!StreamError::RingBufferEmpty.is_agg_error());
    }

    // ── StreamError::is_book_error / is_sequence_error ───────────────────────

    #[test]
    fn test_is_book_error_true_for_reconstruction() {
        let e = StreamError::BookReconstructionFailed { symbol: "BTC-USD".into(), reason: "missing snapshot".into() };
        assert!(e.is_book_error());
    }

    #[test]
    fn test_is_book_error_true_for_sequence_gap() {
        let e = StreamError::SequenceGap { symbol: "BTC-USD".into(), expected: 5, got: 7 };
        assert!(e.is_book_error());
    }

    #[test]
    fn test_is_book_error_false_for_parse_error() {
        let e = StreamError::ParseError { exchange: "binance".into(), reason: "bad json".into() };
        assert!(!e.is_book_error());
    }

    #[test]
    fn test_is_sequence_error_true() {
        let e = StreamError::SequenceGap { symbol: "ETH-USD".into(), expected: 1, got: 3 };
        assert!(e.is_sequence_error());
    }

    #[test]
    fn test_is_sequence_error_false_for_book_crossed() {
        let e = StreamError::BookCrossed { symbol: "BTC-USD".into(), bid: rust_decimal_macros::dec!(101), ask: rust_decimal_macros::dec!(100) };
        assert!(!e.is_sequence_error());
        assert!(e.is_book_error());
    }

    // ── StreamError::severity_level ───────────────────────────────────────────

    #[test]
    fn test_severity_level_4_for_reconnect_exhausted() {
        let e = StreamError::ReconnectExhausted { url: "wss://x".into(), attempts: 5 };
        assert_eq!(e.severity_level(), 4);
    }

    #[test]
    fn test_severity_level_3_for_connection_failed() {
        let e = StreamError::ConnectionFailed { url: "wss://x".into(), reason: "refused".into() };
        assert_eq!(e.severity_level(), 3);
    }

    #[test]
    fn test_severity_level_2_for_stale_feed() {
        let e = StreamError::StaleFeed { feed_id: "BTC".into(), elapsed_ms: 10_000, threshold_ms: 5_000 };
        assert_eq!(e.severity_level(), 2);
    }

    #[test]
    fn test_severity_level_1_for_parse_error() {
        let e = StreamError::ParseError { exchange: "binance".into(), reason: "bad".into() };
        assert_eq!(e.severity_level(), 1);
    }

    #[test]
    fn test_severity_level_2_for_ring_buffer_empty() {
        assert_eq!(StreamError::RingBufferEmpty.severity_level(), 2);
    }
}
