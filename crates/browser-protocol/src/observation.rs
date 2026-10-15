use serde::{Deserialize, Serialize};

use crate::{DocumentId, ElementRef, FrameId, ViewId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub view_id: ViewId,
    pub document_id: DocumentId,
    pub frame_id: FrameId,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub ready_state: DocumentReadyState,
    pub items: Vec<ObservedItem>,
    pub next_cursor: Option<ObservationCursor>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentReadyState {
    Loading,
    Interactive,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservedItem {
    Text(ObservedText),
    Control(ObservedControl),
    Link(ObservedLink),
    Frame(ObservedFrame),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedText {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedControl {
    pub target: ElementRef,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub states: ControlStates,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlStates {
    pub disabled: bool,
    pub focused: bool,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub required: bool,
    pub invalid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedLink {
    pub target: ElementRef,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedFrame {
    pub frame_id: FrameId,
    pub name: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCursor {
    pub view_id: ViewId,
    pub offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetRef {
    pub frame_id: FrameId,
    pub document_id: DocumentId,
    pub element: ElementRef,
}
