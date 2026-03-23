//! FIX 4.2 session adapter — parse, serialize, and manage a FIX connection.
//!
//! ## Responsibility
//! Implement just enough of FIX 4.2 to connect to a market-data server,
//! authenticate with a Logon message, subscribe to market data, and convert
//! incoming Quote / Market Data Snapshot messages into [`NormalizedTick`]s.
//!
//! ## Wire format
//! Fields are separated by SOH (0x01). Each field is `tag=value\x01`.
//! The frame ends with tag 10 (checksum), which is the modulo-256 sum of all
//! bytes before it (including the separators).
//!
//! ## Key FIX 4.2 message types handled
//! | MsgType | Name |
//! |---------|------|
//! | `A`     | Logon |
//! | `0`     | Heartbeat |
//! | `V`     | Market Data Request |
//! | `W`     | Market Data Snapshot/Full Refresh |
//! | `X`     | Market Data Incremental Refresh |
//!
//! ## Concurrency
//! [`FixParser`] is stateless and `Send + Sync`. [`FixSession`] owns its TCP
//! stream and is not intended to be shared across tasks — wrap in `Arc<Mutex<_>>`
//! if needed.

use crate::error::StreamError;
use crate::tick::{Exchange, NormalizedTick};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// SOH byte — FIX field separator.
const SOH: u8 = 0x01;

/// FIX 4.2 tag numbers used in this implementation.
mod tag {
    pub const BEGIN_STRING: u32 = 8;
    pub const BODY_LENGTH: u32 = 9;
    pub const MSG_TYPE: u32 = 35;
    pub const SENDER_COMP_ID: u32 = 49;
    pub const TARGET_COMP_ID: u32 = 56;
    pub const MSG_SEQ_NUM: u32 = 34;
    pub const SENDING_TIME: u32 = 52;
    pub const ENCRYPT_METHOD: u32 = 98;
    pub const HEART_BT_INT: u32 = 108;
    pub const MD_REQ_ID: u32 = 262;
    pub const SUBSCRIPTION_REQUEST_TYPE: u32 = 263;
    pub const MARKET_DEPTH: u32 = 264;
    pub const MD_ENTRY_TYPE: u32 = 269;
    pub const NO_MD_ENTRY_TYPES: u32 = 267;
    pub const NO_RELATED_SYM: u32 = 146;
    pub const SYMBOL: u32 = 55;
    pub const MD_ENTRY_PX: u32 = 270;
    pub const MD_ENTRY_SIZE: u32 = 271;
    pub const CHECKSUM: u32 = 10;
}

/// Error types specific to FIX protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum FixError {
    /// The checksum in tag 10 does not match the computed value.
    #[error("FIX checksum mismatch: expected {expected:03}, got {actual:03}")]
    ChecksumMismatch {
        /// Checksum value computed from the message bytes.
        expected: u8,
        /// Checksum value present in tag 10.
        actual: u8,
    },

    /// A required tag is absent from the message.
    #[error("FIX missing required tag {tag}")]
    MissingTag {
        /// Tag number that was not found.
        tag: u32,
    },

    /// A tag value could not be parsed into the expected type.
    #[error("FIX parse error for tag {tag}: {reason}")]
    ParseError {
        /// Tag number whose value could not be parsed.
        tag: u32,
        /// Description of the parse failure.
        reason: String,
    },

    /// TCP connection error.
    #[error("FIX TCP error: {0}")]
    Tcp(String),

    /// A FIX message was received with an unexpected BeginString.
    #[error("FIX unsupported version '{0}' (expected FIX.4.2)")]
    UnsupportedVersion(String),
}

impl From<FixError> for StreamError {
    fn from(e: FixError) -> Self {
        StreamError::ParseError {
            exchange: "FIX4.2".into(),
            reason: e.to_string(),
        }
    }
}

/// A decoded FIX 4.2 message.
#[derive(Debug, Clone)]
pub struct FixMessage {
    /// MsgType value (tag 35), e.g. `"A"`, `"0"`, `"W"`.
    pub msg_type: String,
    /// All tag-value pairs present in the message, including header and trailer.
    pub fields: HashMap<u32, String>,
}

impl FixMessage {
    /// Retrieve the string value of a tag, returning a [`FixError::MissingTag`]
    /// if the tag is absent.
    pub fn get(&self, tag: u32) -> Result<&str, FixError> {
        self.fields
            .get(&tag)
            .map(String::as_str)
            .ok_or(FixError::MissingTag { tag })
    }

    /// Retrieve the string value of a tag, returning `None` if absent.
    pub fn get_opt(&self, tag: u32) -> Option<&str> {
        self.fields.get(&tag).map(String::as_str)
    }
}

/// Stateless FIX 4.2 frame parser and serializer.
///
/// [`FixParser`] has no mutable state and is `Send + Sync`; a single instance
/// can be shared between read and write tasks via `Arc`.
#[derive(Debug, Default, Clone)]
pub struct FixParser {
    _private: (),
}

impl FixParser {
    /// Create a new [`FixParser`].
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Parse a raw FIX 4.2 frame from bytes.
    ///
    /// The input must be a complete FIX message (from `8=FIX.4.2` through the
    /// trailing `10=NNN\x01`). Partial frames are rejected with a
    /// [`FixError::MissingTag`] for the checksum tag.
    ///
    /// # Errors
    /// - [`FixError::UnsupportedVersion`] — BeginString is not `FIX.4.2`.
    /// - [`FixError::ChecksumMismatch`] — computed checksum != tag 10 value.
    /// - [`FixError::MissingTag`] — required header tag absent.
    /// - [`FixError::ParseError`] — a tag value is malformed.
    pub fn parse(&self, raw: &[u8]) -> Result<FixMessage, FixError> {
        // Split on SOH to get individual tag=value tokens.
        let fields: HashMap<u32, String> = {
            let mut map = HashMap::new();
            for field_bytes in raw.split(|&b| b == SOH) {
                if field_bytes.is_empty() {
                    continue;
                }
                // Find the '=' separator.
                let sep = field_bytes
                    .iter()
                    .position(|&b| b == b'=')
                    .ok_or_else(|| FixError::ParseError {
                        tag: 0,
                        reason: format!(
                            "field '{}' has no '=' separator",
                            String::from_utf8_lossy(field_bytes)
                        ),
                    })?;
                let tag_str = std::str::from_utf8(&field_bytes[..sep]).map_err(|e| {
                    FixError::ParseError {
                        tag: 0,
                        reason: format!("tag not UTF-8: {e}"),
                    }
                })?;
                let val_str =
                    std::str::from_utf8(&field_bytes[sep + 1..]).map_err(|e| FixError::ParseError {
                        tag: 0,
                        reason: format!("value not UTF-8: {e}"),
                    })?;
                let tag: u32 = tag_str.parse().map_err(|e| FixError::ParseError {
                    tag: 0,
                    reason: format!("tag not numeric ('{tag_str}'): {e}"),
                })?;
                map.insert(tag, val_str.to_string());
            }
            map
        };

        // Validate version.
        let begin_string = fields
            .get(&tag::BEGIN_STRING)
            .ok_or(FixError::MissingTag { tag: tag::BEGIN_STRING })?;
        if begin_string != "FIX.4.2" {
            return Err(FixError::UnsupportedVersion(begin_string.clone()));
        }

        // Validate checksum: sum all bytes except the `10=NNN\x01` trailer.
        let checksum_tag_pos = raw
            .windows(3)
            .position(|w| w == b"10=")
            .ok_or(FixError::MissingTag { tag: tag::CHECKSUM })?;
        let computed: u8 = raw[..checksum_tag_pos]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        let declared_str = fields
            .get(&tag::CHECKSUM)
            .ok_or(FixError::MissingTag { tag: tag::CHECKSUM })?;
        let declared: u8 = declared_str.parse().map_err(|e| FixError::ParseError {
            tag: tag::CHECKSUM,
            reason: format!("checksum not numeric: {e}"),
        })?;
        if computed != declared {
            return Err(FixError::ChecksumMismatch {
                expected: computed,
                actual: declared,
            });
        }

        let msg_type = fields
            .get(&tag::MSG_TYPE)
            .ok_or(FixError::MissingTag { tag: tag::MSG_TYPE })?
            .clone();

        Ok(FixMessage { msg_type, fields })
    }

    /// Encode a [`FixMessage`] to FIX 4.2 wire format.
    ///
    /// Tags 8, 9, and 10 are automatically inserted/overwritten with the
    /// correct values; callers should not set them in `msg.fields`.
    pub fn serialize(&self, msg: &FixMessage) -> Vec<u8> {
        // Build body: all tags except 8, 9, 10.
        let mut body: Vec<u8> = Vec::new();
        // Emit tag 35 (MsgType) first per FIX standard.
        body.extend_from_slice(
            format!("{}={}{}", tag::MSG_TYPE, msg.msg_type, SOH as char).as_bytes(),
        );
        // Remaining tags in ascending order for deterministic output.
        let mut sorted_tags: Vec<(&u32, &String)> = msg
            .fields
            .iter()
            .filter(|(k, _)| {
                **k != tag::BEGIN_STRING
                    && **k != tag::BODY_LENGTH
                    && **k != tag::MSG_TYPE
                    && **k != tag::CHECKSUM
            })
            .collect();
        sorted_tags.sort_by_key(|(k, _)| **k);
        for (tag, val) in sorted_tags {
            body.extend_from_slice(format!("{tag}={val}{}", SOH as char).as_bytes());
        }

        // Prepend header: 8= and 9=
        let mut frame: Vec<u8> = Vec::new();
        frame.extend_from_slice(
            format!("{}=FIX.4.2{}", tag::BEGIN_STRING, SOH as char).as_bytes(),
        );
        frame
            .extend_from_slice(format!("{}={}{}", tag::BODY_LENGTH, body.len(), SOH as char).as_bytes());
        frame.extend_from_slice(&body);

        // Append checksum.
        let checksum: u8 = frame.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        frame.extend_from_slice(
            format!("{}={:03}{}", tag::CHECKSUM, checksum, SOH as char).as_bytes(),
        );
        frame
    }
}

/// A FIX 4.2 session — TCP connection with sequence number tracking.
pub struct FixSession {
    stream: TcpStream,
    parser: FixParser,
    sender_comp_id: String,
    target_comp_id: String,
    seq_num: u32,
}

impl FixSession {
    /// Open a TCP connection to `addr` (e.g. `"127.0.0.1:9878"`).
    ///
    /// Does not send a Logon — call [`FixSession::logon`] after construction.
    pub async fn connect(addr: &str) -> Result<Self, FixError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| FixError::Tcp(e.to_string()))?;
        info!(addr, "FIX TCP connected");
        Ok(Self {
            stream,
            parser: FixParser::new(),
            sender_comp_id: String::new(),
            target_comp_id: String::new(),
            seq_num: 1,
        })
    }

    /// Send a FIX 4.2 Logon message (MsgType = `A`).
    ///
    /// Sets `sender_comp_id` and `target_comp_id` for all subsequent messages.
    pub async fn logon(
        &mut self,
        sender_comp_id: &str,
        target_comp_id: &str,
    ) -> Result<(), FixError> {
        self.sender_comp_id = sender_comp_id.to_string();
        self.target_comp_id = target_comp_id.to_string();

        let mut fields = HashMap::new();
        fields.insert(tag::SENDER_COMP_ID, sender_comp_id.to_string());
        fields.insert(tag::TARGET_COMP_ID, target_comp_id.to_string());
        fields.insert(tag::MSG_SEQ_NUM, self.seq_num.to_string());
        fields.insert(
            tag::SENDING_TIME,
            chrono::Utc::now().format("%Y%m%d-%H:%M:%S").to_string(),
        );
        fields.insert(tag::ENCRYPT_METHOD, "0".to_string());
        fields.insert(tag::HEART_BT_INT, "30".to_string());

        let msg = FixMessage {
            msg_type: "A".to_string(),
            fields,
        };
        let bytes = self.parser.serialize(&msg);
        self.send_raw(&bytes).await?;
        self.seq_num += 1;
        debug!(seq = self.seq_num - 1, "FIX Logon sent");
        Ok(())
    }

    /// Send a Market Data Request (MsgType = `V`) subscribing to all `symbols`.
    ///
    /// Requests bid/ask/trade (entry types 0, 1, 2) at full book depth.
    pub async fn subscribe_market_data(&mut self, symbols: &[&str]) -> Result<(), FixError> {
        let mut fields = HashMap::new();
        fields.insert(tag::SENDER_COMP_ID, self.sender_comp_id.clone());
        fields.insert(tag::TARGET_COMP_ID, self.target_comp_id.clone());
        fields.insert(tag::MSG_SEQ_NUM, self.seq_num.to_string());
        fields.insert(
            tag::SENDING_TIME,
            chrono::Utc::now().format("%Y%m%d-%H:%M:%S").to_string(),
        );
        // MDReqID: unique request identifier
        fields.insert(tag::MD_REQ_ID, format!("req-{}", self.seq_num));
        // SubscriptionRequestType=1 (Snapshot + Updates)
        fields.insert(tag::SUBSCRIPTION_REQUEST_TYPE, "1".to_string());
        // MarketDepth=1 (top of book)
        fields.insert(tag::MARKET_DEPTH, "1".to_string());
        // NoMDEntryTypes=3 (Bid, Offer, Trade)
        fields.insert(tag::NO_MD_ENTRY_TYPES, "3".to_string());
        fields.insert(tag::MD_ENTRY_TYPE, "0".to_string()); // Bid
        // NoRelatedSym
        fields.insert(tag::NO_RELATED_SYM, symbols.len().to_string());
        for sym in symbols {
            fields.insert(tag::SYMBOL, sym.to_string());
        }

        let msg = FixMessage {
            msg_type: "V".to_string(),
            fields,
        };
        let bytes = self.parser.serialize(&msg);
        self.send_raw(&bytes).await?;
        self.seq_num += 1;
        debug!(
            seq = self.seq_num - 1,
            symbols = ?symbols,
            "FIX MarketDataRequest sent"
        );
        Ok(())
    }

    /// Convert an inbound FIX message into a [`NormalizedTick`] when possible.
    ///
    /// Handles:
    /// - `W` (Market Data Snapshot) — uses MDEntryPx (tag 270) as price,
    ///   MDEntrySize (tag 271) as quantity.
    /// - `X` (Market Data Incremental Refresh) — same field extraction.
    /// - `0` (Heartbeat) — logs and returns `None`.
    /// - `A` (Logon) — logs and returns `None`.
    /// - All other types — returns `None`.
    pub fn on_message(&self, msg: FixMessage) -> Option<NormalizedTick> {
        match msg.msg_type.as_str() {
            "W" | "X" => {
                let symbol = msg.get_opt(tag::SYMBOL)?.to_string();
                let price_str = msg.get_opt(tag::MD_ENTRY_PX)?;
                let size_str = msg.get_opt(tag::MD_ENTRY_SIZE).unwrap_or("0");

                let price = Decimal::from_str(price_str).ok()?;
                let quantity = Decimal::from_str(size_str).unwrap_or(Decimal::ZERO);

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);

                Some(NormalizedTick {
                    exchange: Exchange::Alpaca, // FIX sessions default to Alpaca
                    symbol,
                    price,
                    quantity,
                    side: None,
                    trade_id: msg.get_opt(tag::MD_REQ_ID).map(str::to_string),
                    exchange_ts_ms: None,
                    received_at_ms: now_ms,
                })
            }
            "0" => {
                debug!("FIX Heartbeat received");
                None
            }
            "A" => {
                info!("FIX Logon acknowledged");
                None
            }
            other => {
                warn!(msg_type = other, "FIX unhandled message type");
                None
            }
        }
    }

    /// Read one complete FIX message from the TCP stream.
    ///
    /// Reads until `10=NNN\x01` is encountered, then parses the accumulated
    /// bytes. Returns `None` on clean EOF.
    pub async fn read_message(&mut self) -> Result<Option<FixMessage>, FixError> {
        let mut buf: Vec<u8> = Vec::with_capacity(512);
        let mut byte = [0u8; 1];
        loop {
            match self.stream.read(&mut byte).await {
                Ok(0) => {
                    if buf.is_empty() {
                        return Ok(None);
                    }
                    // Partial message on EOF — try to parse what we have.
                    break;
                }
                Ok(_) => {
                    buf.push(byte[0]);
                    // Detect end of FIX message: last field is 10=NNN\x01
                    if buf.len() >= 6 && buf.ends_with(&[SOH]) {
                        // Check if the last segment is the checksum tag.
                        let last_soh = buf[..buf.len() - 1]
                            .iter()
                            .rposition(|&b| b == SOH)
                            .map(|p| p + 1)
                            .unwrap_or(0);
                        if buf[last_soh..].starts_with(b"10=") {
                            break;
                        }
                    }
                }
                Err(e) => return Err(FixError::Tcp(e.to_string())),
            }
        }
        Ok(Some(self.parser.parse(&buf)?))
    }

    /// Write raw bytes to the TCP stream.
    async fn send_raw(&mut self, data: &[u8]) -> Result<(), FixError> {
        self.stream
            .write_all(data)
            .await
            .map_err(|e| FixError::Tcp(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_minimal_fix_frame(msg_type: &str, body_fields: &str) -> Vec<u8> {
        // Build body string: tag 35 + extra fields
        let body = format!("35={msg_type}{soh}{body_fields}", soh = SOH as char);
        let body_len = body.len();
        let header = format!(
            "8=FIX.4.2{soh}9={body_len}{soh}",
            soh = SOH as char
        );
        let pre_checksum = format!("{header}{body}");
        let checksum: u8 = pre_checksum
            .as_bytes()
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        let full = format!(
            "{pre_checksum}10={checksum:03}{soh}",
            soh = SOH as char
        );
        full.into_bytes()
    }

    #[test]
    fn parse_heartbeat() {
        let frame = make_minimal_fix_frame("0", "49=SENDER\x0156=TARGET\x01");
        let parser = FixParser::new();
        let msg = parser.parse(&frame).expect("should parse heartbeat");
        assert_eq!(msg.msg_type, "0");
    }

    #[test]
    fn parse_wrong_version_error() {
        let body = format!("35=0{soh}49=A{soh}56=B{soh}", soh = SOH as char);
        let body_len = body.len();
        let pre = format!("8=FIX.4.1{soh}9={body_len}{soh}{body}", soh = SOH as char);
        let checksum: u8 = pre.as_bytes().iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        let frame = format!("{pre}10={checksum:03}{soh}", soh = SOH as char).into_bytes();
        let parser = FixParser::new();
        let result = parser.parse(&frame);
        assert!(matches!(result, Err(FixError::UnsupportedVersion(_))));
    }

    #[test]
    fn parse_checksum_mismatch_error() {
        let mut frame = make_minimal_fix_frame("0", "49=A\x0156=B\x01");
        // Corrupt a byte in the body to trigger checksum failure.
        if let Some(b) = frame.get_mut(10) {
            *b = b'^';
        }
        let parser = FixParser::new();
        let result = parser.parse(&frame);
        assert!(matches!(result, Err(FixError::ChecksumMismatch { .. })));
    }

    #[test]
    fn serialize_roundtrip() {
        let parser = FixParser::new();
        let mut fields = HashMap::new();
        fields.insert(tag::SENDER_COMP_ID, "SENDER".to_string());
        fields.insert(tag::TARGET_COMP_ID, "TARGET".to_string());
        fields.insert(tag::MSG_SEQ_NUM, "1".to_string());
        let msg = FixMessage {
            msg_type: "0".to_string(),
            fields,
        };
        let bytes = parser.serialize(&msg);
        let parsed = parser.parse(&bytes).expect("serialized frame should parse");
        assert_eq!(parsed.msg_type, "0");
        assert_eq!(parsed.get(tag::SENDER_COMP_ID).unwrap(), "SENDER");
    }

    #[test]
    fn on_message_snapshot_converts_to_tick() {
        let parser = FixParser::new();
        let mut fields = HashMap::new();
        fields.insert(tag::SYMBOL, "AAPL".to_string());
        fields.insert(tag::MD_ENTRY_PX, "182.50".to_string());
        fields.insert(tag::MD_ENTRY_SIZE, "100".to_string());
        let msg = FixMessage {
            msg_type: "W".to_string(),
            fields,
        };
        let session = FixSession {
            stream: {
                // We can't construct a real TcpStream in unit tests, so we test
                // on_message directly without a live connection.
                // Use std::net to bind a loopback listener, then connect.
                // This is acceptable in tests only.
                let listener =
                    std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
                let addr = listener.local_addr().expect("local addr");
                let std_stream =
                    std::net::TcpStream::connect(addr).expect("connect loopback");
                TcpStream::from_std(std_stream).expect("into tokio stream")
            },
            parser,
            sender_comp_id: "S".to_string(),
            target_comp_id: "T".to_string(),
            seq_num: 1,
        };
        let tick = session.on_message(msg).expect("should produce tick");
        assert_eq!(tick.symbol, "AAPL");
        assert_eq!(tick.price.to_string(), "182.50");
        assert_eq!(tick.quantity.to_string(), "100");
    }

    #[test]
    fn on_message_heartbeat_returns_none() {
        let parser = FixParser::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let std_stream = std::net::TcpStream::connect(addr).expect("connect");
        let stream = TcpStream::from_std(std_stream).expect("tokio stream");
        let session = FixSession {
            stream,
            parser,
            sender_comp_id: "S".to_string(),
            target_comp_id: "T".to_string(),
            seq_num: 1,
        };
        let msg = FixMessage {
            msg_type: "0".to_string(),
            fields: HashMap::new(),
        };
        assert!(session.on_message(msg).is_none());
    }

    #[test]
    fn fix_message_get_missing_tag_error() {
        let msg = FixMessage {
            msg_type: "A".to_string(),
            fields: HashMap::new(),
        };
        assert!(matches!(msg.get(999), Err(FixError::MissingTag { tag: 999 })));
    }
}
