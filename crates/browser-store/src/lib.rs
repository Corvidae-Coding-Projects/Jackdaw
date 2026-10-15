//! Durable, versioned profile storage for browser-owned state.
//!
//! This crate owns persistence and migration boundaries. It does not perform network requests or
//! expose redb guards to callers.

mod cookies;
mod operations;
mod storage;

pub use cookies::{
    COOKIE_SCHEMA_VERSION, CookieRecord, ProfileCookieStore, ProfileStore, StoreError,
    TabSessionRecord,
};
pub use operations::{OperationPhase, OperationRecord};
