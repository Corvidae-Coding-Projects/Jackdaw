//! Persistent browser service state and protocol routing.
//!
//! Transport processes own a `BrowserService`; frontends exchange only owned
//! `browser-protocol` values.

mod broker;
mod cancellation;
mod capabilities;
mod document_worker;
mod errors;
mod events;
mod interactions;
mod navigation;
mod observation_commands;
mod observations;
mod operations;
mod profiles;
mod router;
mod sandbox;
mod screenshots;
mod service;
mod tabs;
mod transfers;
mod waits;
mod worker_protocol;
mod workers;

pub use cancellation::NavigationCancellation;
pub use document_worker::{probe_document_worker_process, run_document_worker_stdio};
pub use observations::ViewRetentionPolicy;
pub use service::BrowserService;
