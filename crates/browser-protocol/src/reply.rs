use serde::{Deserialize, Serialize};

use crate::{
    CapabilityManifest, DocumentId, DownloadId, EventBatch, Frame, Observation, OperationId,
    ProfileId, ProtocolVersion, RequestId, TabId,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReply {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub success: bool,
    pub target: Option<ReplyTarget>,
    pub receipt: Option<OperationReceipt>,
    pub result: Option<CommandResult>,
    pub error: Option<ProtocolError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyTarget {
    pub profile_id: ProfileId,
    pub tab_id: Option<TabId>,
    pub document_id: Option<DocumentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationReceipt {
    pub id: OperationId,
    pub status: OperationStatus,
    pub effect: EffectState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Accepted,
    Running,
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    None,
    Applied,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CommandResult {
    Empty,
    Status(ServiceStatus),
    Version(VersionResult),
    Capabilities(CapabilityManifest),
    Profiles(Vec<ProfileId>),
    Tabs(Vec<TabSummary>),
    Navigation(NavigationResult),
    Observation(Observation),
    Frame(Frame),
    Screenshot(ScreenshotResult),
    Scroll(ScrollResult),
    Download(DownloadResult),
    Downloads(Vec<DownloadResult>),
    Operation(OperationResult),
    Events(EventBatch),
    Console(Vec<ConsoleRecord>),
    Network(Vec<NetworkRecord>),
    Json(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkRecord {
    pub sequence: u64,
    pub request_id: String,
    pub requested_url: String,
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleRecord {
    pub sequence: u64,
    pub level: ConsoleLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleLevel {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationResult {
    pub request_id: RequestId,
    pub receipt: OperationReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionResult {
    pub product_version: String,
    pub protocol_version: ProtocolVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub running: bool,
    pub service_instance_id: crate::ServiceInstanceId,
    pub profiles_open: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSummary {
    pub tab_id: TabId,
    pub url: String,
    pub title: String,
    pub loading: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavigationResult {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub document_id: DocumentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollResult {
    pub offset_x: i64,
    pub offset_y: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub format: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadResult {
    pub download_id: DownloadId,
    pub status: crate::TransferStatus,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub effect: EffectState,
    pub details: Vec<ErrorDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedProtocol,
    ProfileNotFound,
    TabNotFound,
    FrameNotFound,
    AmbiguousTarget,
    StaleTarget,
    ViewExpired,
    TargetNotInteractable,
    Timeout,
    WorkerCrashed,
    NetworkError,
    PolicyDenied,
    TransferFailed,
    RequestIdConflict,
    OperationExpired,
    EventGap,
    CapabilityUnsupported,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorDetail {
    pub field: Option<String>,
    pub reason: String,
}

impl ProtocolError {
    /// Process status for the protocol-v1 CLI contract. Structured errors remain authoritative.
    pub const fn exit_status(&self) -> u8 {
        match self.code {
            ErrorCode::InvalidRequest | ErrorCode::UnsupportedProtocol => 2,
            ErrorCode::Timeout => 3,
            ErrorCode::PolicyDenied | ErrorCode::CapabilityUnsupported => 4,
            ErrorCode::WorkerCrashed | ErrorCode::RequestIdConflict | ErrorCode::Internal => 5,
            ErrorCode::ProfileNotFound
            | ErrorCode::TabNotFound
            | ErrorCode::FrameNotFound
            | ErrorCode::AmbiguousTarget
            | ErrorCode::StaleTarget
            | ErrorCode::ViewExpired
            | ErrorCode::TargetNotInteractable
            | ErrorCode::NetworkError
            | ErrorCode::TransferFailed
            | ErrorCode::OperationExpired
            | ErrorCode::EventGap => 1,
        }
    }
}
