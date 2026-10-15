use serde::{Deserialize, Serialize};

use crate::{DocumentId, DownloadId, OperationId, ServiceInstanceId, TabId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCursor {
    pub service_instance_id: ServiceInstanceId,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBatch {
    pub events: Vec<BrowserEvent>,
    pub next_cursor: EventCursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserEvent {
    pub service_instance_id: ServiceInstanceId,
    pub sequence: u64,
    pub event: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    NavigationStarted {
        tab_id: TabId,
        operation_id: OperationId,
    },
    NavigationCommitted {
        tab_id: TabId,
        document_id: DocumentId,
        url: String,
    },
    LoadingChanged {
        tab_id: TabId,
        loading: bool,
    },
    DownloadChanged {
        download_id: DownloadId,
        status: TransferStatus,
        received_bytes: u64,
    },
    WorkerCrashed {
        tab_id: TabId,
    },
    Gap {
        first_available_sequence: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Proposed,
    InProgress,
    Completed,
    Cancelled,
    Interrupted,
    Failed,
}
