//! Browser document-engine boundary.
//!
//! The engine owns each live Blitz document together with its thread-affine Boa realm.
//! Public callers interact through generation-checked, owned values rather than engine handles.

mod persistence;
mod runtime;
mod runtime_document;
mod runtime_fetch;
mod runtime_forms;
mod runtime_images;
mod runtime_interaction;
mod runtime_mutation;
mod runtime_navigation;
mod runtime_observation;
mod runtime_render;
mod runtime_script;

pub use persistence::{COMPONENT_SCHEMA_VERSION, ComponentRecord, round_trip_component_record};
pub use runtime::{
    DocumentRuntime, ElementIdentity, ElementTarget, PageConsoleLevel, PageConsoleRecord,
    PageNetworkRecord,
};
pub use runtime_fetch::{PageFetchDispatch, PageFetchRequest};
pub use runtime_forms::{FormField, FormSubmission};
pub use runtime_navigation::{DocumentHost, FormNavigationRequest, NavigationCommit};
pub use runtime_observation::{SemanticControl, SemanticLink};
