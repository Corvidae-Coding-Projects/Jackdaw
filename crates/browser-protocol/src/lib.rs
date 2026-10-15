//! Versioned, frontend-neutral wire contract for the Rust Agent Browser.
//!
//! This crate deliberately contains no browser-engine, persistence, or transport
//! implementation types. Its values are owned so they can cross process boundaries.

mod capabilities;
mod command;
mod event;
mod frame;
mod ids;
mod observation;
mod reply;

pub use capabilities::*;
pub use command::*;
pub use event::*;
pub use frame::*;
pub use ids::*;
pub use observation::*;
pub use reply::*;

/// The only protocol version accepted by this build.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1);
