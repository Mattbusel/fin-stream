//! gRPC streaming endpoint — expose the tick stream over gRPC via `tonic`.
//!
//! ## Feature flag
//!
//! This module is gated behind the `grpc` Cargo feature. Add to `Cargo.toml`:
//!
//! ```toml
//! fin-stream = { version = "*", features = ["grpc"] }
//! ```
//!
//! ## Proto definition
//!
//! See `proto/tick_stream.proto`. The service exposes a single bidirectional
//! streaming RPC `SubscribeTicks` that emits [`Tick`] messages as fast as the
//! pipeline produces them.
//!
//! ## Architecture
//!
//! ```text
//! NormalizedTick (pipeline)
//!        │
//!        ▼
//! broadcast::Sender<NormalizedTick>    ← TickStreamServer holds this
//!        │
//!        ▼
//! SubscribeTicks RPC handler           ← subscribes a new receiver per call
//!        │
//!        ▼
//! gRPC client (tonic stream)
//! ```

#[cfg(feature = "grpc")]
pub use grpc_impl::*;

#[cfg(feature = "grpc")]
mod grpc_impl {
    use crate::tick::{Exchange, NormalizedTick};
    use std::pin::Pin;
    use std::str::FromStr;
    use tokio::sync::broadcast;
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::{Stream, StreamExt};
    use tonic::{Request, Response, Status};

    // Include the generated tonic code.
    pub mod proto {
        tonic::include_proto!("fin_stream");
    }

    use proto::tick_stream_service_server::{TickStreamService, TickStreamServiceServer};
    use proto::{SubscribeTicksRequest, Tick};

    /// Convert a [`NormalizedTick`] to the generated proto [`Tick`] message.
    fn to_proto_tick(t: NormalizedTick) -> Tick {
        Tick {
            exchange: t.exchange.to_string(),
            symbol: t.symbol,
            price: t.price.to_string(),
            quantity: t.quantity.to_string(),
            side: t.side.map(|s| s.to_string()).unwrap_or_default(),
            trade_id: t.trade_id.unwrap_or_default(),
            exchange_ts_ms: t.exchange_ts_ms.unwrap_or(0),
            received_at_ms: t.received_at_ms,
        }
    }

    /// gRPC server that broadcasts ticks to all connected clients.
    ///
    /// Construct with [`TickStreamServer::new`], then feed ticks via
    /// [`TickStreamServer::publish`]. Pass [`TickStreamServer::into_service`]
    /// to a `tonic::transport::Server`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # #[cfg(feature = "grpc")]
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// use fin_stream::grpc::TickStreamServer;
    /// use tonic::transport::Server;
    ///
    /// let server = TickStreamServer::new(1024);
    /// let svc = server.clone_service();
    /// tokio::spawn(async move {
    ///     Server::builder()
    ///         .add_service(svc)
    ///         .serve("0.0.0.0:50051".parse().unwrap())
    ///         .await
    ///         .unwrap();
    /// });
    /// // server.publish(tick).ok();
    /// # Ok(())
    /// # }
    /// ```
    #[derive(Clone)]
    pub struct TickStreamServer {
        tx: broadcast::Sender<NormalizedTick>,
    }

    impl TickStreamServer {
        /// Create a server with the given broadcast channel capacity.
        ///
        /// `capacity` is the number of ticks the broadcast channel can buffer
        /// per subscriber. Slow subscribers that fall behind will receive a
        /// [`broadcast::error::RecvError::Lagged`] error and miss ticks.
        pub fn new(capacity: usize) -> Self {
            let (tx, _) = broadcast::channel(capacity);
            Self { tx }
        }

        /// Publish a tick to all active subscribers.
        ///
        /// Returns the number of active subscribers that received the tick.
        /// Returns `0` (not an error) if no clients are connected.
        pub fn publish(&self, tick: NormalizedTick) -> usize {
            match self.tx.send(tick) {
                Ok(n) => n,
                Err(_) => 0, // No receivers — not an error.
            }
        }

        /// Number of currently active subscribers.
        pub fn subscriber_count(&self) -> usize {
            self.tx.receiver_count()
        }

        /// Build the tonic service and return it for use with
        /// `tonic::transport::Server::add_service`.
        pub fn clone_service(
            &self,
        ) -> TickStreamServiceServer<TickStreamServer> {
            TickStreamServiceServer::new(self.clone())
        }
    }

    type TickStream = Pin<Box<dyn Stream<Item = Result<Tick, Status>> + Send + 'static>>;

    #[tonic::async_trait]
    impl TickStreamService for TickStreamServer {
        type SubscribeTicksStream = TickStream;

        async fn subscribe_ticks(
            &self,
            request: Request<SubscribeTicksRequest>,
        ) -> Result<Response<Self::SubscribeTicksStream>, Status> {
            let filter = request.into_inner().filter.unwrap_or_default();
            let symbol_filter = if filter.symbol.is_empty() {
                None
            } else {
                Some(filter.symbol)
            };
            let exchange_filter: Option<Exchange> = if filter.exchange.is_empty() {
                None
            } else {
                Exchange::from_str(&filter.exchange).ok()
            };

            let rx = self.tx.subscribe();
            let stream = BroadcastStream::new(rx).filter_map(move |result| {
                match result {
                    Err(_lagged) => {
                        // Subscriber fell behind — skip lagged messages.
                        None
                    }
                    Ok(tick) => {
                        // Apply symbol filter.
                        if let Some(ref sym) = symbol_filter {
                            if &tick.symbol != sym {
                                return None;
                            }
                        }
                        // Apply exchange filter.
                        if let Some(ex) = exchange_filter {
                            if tick.exchange != ex {
                                return None;
                            }
                        }
                        Some(Ok(to_proto_tick(tick)))
                    }
                }
            });

            Ok(Response::new(Box::pin(stream)))
        }
    }
}

// When the grpc feature is disabled, expose a minimal stub so the module
// compiles and doc-tests don't fail.
#[cfg(not(feature = "grpc"))]
/// gRPC feature is not enabled.
///
/// Enable the `grpc` Cargo feature to use this module:
///
/// ```toml
/// fin-stream = { version = "*", features = ["grpc"] }
/// ```
pub struct GrpcDisabled;
