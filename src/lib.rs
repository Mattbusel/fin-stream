// SPDX-License-Identifier: MIT
#![deny(missing_docs)]
//! # fin-stream
//!
//! Lock-free streaming primitives for real-time financial market data.
//! Optimized for high-throughput, zero-allocation hot paths.

pub use fin_stream_core::*;

pub mod prelude {
    //! Workspace-wide prelude for common traits and types.
    pub use fin_stream_core::prelude::*;
}
