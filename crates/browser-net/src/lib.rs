//! Browser-oriented HTTP(S) transport, TLS validation, and redirect policy.
//!
//! The default build uses the pinned Rust-only TLS provider and public-root set, validates server
//! identity, and shares one bounded HTTP/1.1 exchange across plain and encrypted connections.

mod client;
mod config;
mod cookies;
mod error;
mod origin;
mod request;
#[cfg(feature = "tls")]
mod tls;
mod transport;

pub use client::BrowserClient;
pub use config::{BrowserNetworkConfig, BrowserNetworkConfigBuilder, RequestControl};
pub use error::{NetworkEffect, NetworkError, NetworkPhase};
pub use origin::{OpaqueOriginId, Origin};
pub use request::{BrowserRequest, BrowserResponse, RedirectHop, RequestMethod};
