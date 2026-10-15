use serde::{Deserialize, Serialize};

/// Private daemon-to-document-worker messages.
///
/// This protocol is intentionally not part of `browser-protocol`: it is owned by
/// the daemon and may change with the exact release binary. Every value is owned
/// so no Boa or Blitz handle can cross the process boundary.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DocumentWorkerCommand {
    Initialize {
        html: String,
    },
    ObserveDocumentText,
    ObserveControls,
    ObserveLinks,
    Metadata,
    HitTest {
        x: i32,
        y: i32,
    },
    LinkUrl {
        target: DocumentWorkerTarget,
    },
    ValidateTarget {
        target: DocumentWorkerTarget,
    },
    FocusedControl,
    RunNextTimer,
    NavigationTarget {
        navigation: DocumentWorkerNavigation,
    },
    NavigationSnapshot,
    CommitNavigation {
        navigation: DocumentWorkerNavigation,
        requested_url: String,
        response: DocumentWorkerHttpResponse,
    },
    RestoreHistory {
        history: Vec<String>,
        cursor: usize,
        response: DocumentWorkerHttpResponse,
    },
    Click {
        target: DocumentWorkerTarget,
    },
    Key {
        target: DocumentWorkerTarget,
        key: String,
        modifiers: Vec<String>,
    },
    Select {
        target: DocumentWorkerTarget,
        values: Vec<String>,
    },
    SelectFiles {
        target: DocumentWorkerTarget,
        files: Vec<DocumentWorkerSelectedFile>,
    },
    ConsoleRecords,
    NetworkRecords,
    SubmissionAfterClick {
        target: DocumentWorkerTarget,
    },
    ResolveSubmissionTarget {
        target: DocumentWorkerTarget,
    },
    Submit {
        form: DocumentWorkerTarget,
        submitter: Option<DocumentWorkerTarget>,
    },
    PrepareFormSubmission {
        submission: DocumentWorkerFormSubmission,
    },
    CommitFormNavigation {
        requested_url: String,
        response: DocumentWorkerHttpResponse,
    },
    TakePageFetchRequests,
    PreparePageFetch {
        request: DocumentWorkerPageFetchRequest,
    },
    CompletePageFetch {
        request: DocumentWorkerPageFetchRequest,
        response: DocumentWorkerHttpResponse,
    },
    RejectPageFetchResponse {
        request: DocumentWorkerPageFetchRequest,
        response: DocumentWorkerHttpResponse,
        message: String,
    },
    RejectPageFetch {
        request: DocumentWorkerPageFetchRequest,
        message: String,
    },
    BrokerReply {
        request_id: u64,
        result: DocumentWorkerBrokerResult,
    },
    Focus {
        target: DocumentWorkerTarget,
    },
    Fill {
        target: DocumentWorkerTarget,
        value: String,
    },
    TypeText {
        target: DocumentWorkerTarget,
        text: String,
    },
    Check {
        target: DocumentWorkerTarget,
        checked: bool,
    },
    EvaluateScript {
        source: String,
    },
    ScrollBy {
        delta_x: i32,
        delta_y: i32,
    },
    RenderDocument {
        width: u16,
        height: u16,
    },
    ProbeIsolation {
        file_path: String,
        socket_path: String,
    },
    Ping,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DocumentWorkerReply {
    Ready(DocumentWorkerMetadata),
    Pong(DocumentWorkerMetadata),
    DocumentText {
        text: String,
    },
    Controls {
        controls: Vec<DocumentWorkerControl>,
    },
    Links {
        links: Vec<DocumentWorkerLink>,
    },
    Metadata(DocumentWorkerMetadata),
    Target {
        target: Option<DocumentWorkerTarget>,
    },
    OptionalString {
        value: Option<String>,
    },
    Validated,
    Advanced {
        advanced: bool,
    },
    NavigationTarget {
        url: Option<String>,
    },
    NavigationSnapshot {
        snapshot: Option<DocumentWorkerNavigationSnapshot>,
    },
    NavigationCommitted {
        commit: Option<DocumentWorkerNavigationCommit>,
    },
    Boolean {
        value: bool,
    },
    SelectedValues {
        values: Vec<String>,
    },
    ConsoleRecords {
        records: Vec<DocumentWorkerConsoleRecord>,
    },
    NetworkRecords {
        records: Vec<DocumentWorkerNetworkRecord>,
    },
    FormSubmission {
        submission: Option<DocumentWorkerFormSubmission>,
    },
    SubmissionTarget {
        form: DocumentWorkerTarget,
        submitter: Option<DocumentWorkerTarget>,
    },
    FormNavigation {
        request: DocumentWorkerFormNavigationRequest,
    },
    PageFetchRequests {
        requests: Vec<DocumentWorkerPageFetchRequest>,
    },
    PageFetchDispatch {
        dispatch: Option<DocumentWorkerPageFetchDispatch>,
    },
    BrokerRequest(DocumentWorkerBrokerRequest),
    Mutated,
    Scrolled {
        x: i64,
        y: i64,
    },
    RenderedDocument {
        width: u32,
        height: u32,
        rgba_premultiplied: Vec<u8>,
    },
    Isolation {
        file_access_denied: bool,
        socket_access_denied: bool,
    },
    Shutdown,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerMetadata {
    pub generation: u64,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerTarget {
    pub document_generation: u64,
    pub element_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerControl {
    pub target: DocumentWorkerTarget,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub disabled: bool,
    pub focused: bool,
    pub checked: Option<bool>,
    pub required: bool,
    pub invalid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerLink {
    pub target: DocumentWorkerTarget,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DocumentWorkerNavigation {
    Open,
    Back,
    Forward,
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerHttpResponse {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub redirect_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerNavigationCommit {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub redirect_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerNavigationSnapshot {
    pub history: Vec<String>,
    pub cursor: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerSelectedFile {
    pub file_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerFormField {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerFormFile {
    pub name: String,
    pub file_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerFormSubmission {
    pub method: String,
    pub action: String,
    pub enctype: String,
    pub fields: Vec<DocumentWorkerFormField>,
    pub files: Vec<DocumentWorkerFormFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerFormNavigationRequest {
    pub requested_url: String,
    pub request: DocumentWorkerHttpRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerPageFetchRequest {
    pub document_generation: u64,
    pub request_id: String,
    pub requested_url: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerPageFetchDispatch {
    pub request: DocumentWorkerHttpRequest,
    pub document_origin: String,
    pub cross_origin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerConsoleRecord {
    pub sequence: u64,
    pub level: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DocumentWorkerNetworkRecord {
    pub sequence: u64,
    pub request_id: String,
    pub requested_url: String,
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DocumentWorkerBrokerRequest {
    FetchSubresource {
        request_id: u64,
        requested_url: String,
    },
    LoadLocalStorage {
        request_id: u64,
        origin: String,
    },
    ReplaceLocalStorage {
        request_id: u64,
        origin: String,
        entries: Vec<(String, String)>,
    },
    DocumentCookie {
        request_id: u64,
        document_url: String,
    },
    StoreDocumentCookie {
        request_id: u64,
        document_url: String,
        assignment: String,
    },
}

impl DocumentWorkerBrokerRequest {
    pub(crate) fn request_id(&self) -> u64 {
        match self {
            Self::FetchSubresource { request_id, .. }
            | Self::LoadLocalStorage { request_id, .. }
            | Self::ReplaceLocalStorage { request_id, .. }
            | Self::DocumentCookie { request_id, .. }
            | Self::StoreDocumentCookie { request_id, .. } => *request_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum DocumentWorkerBrokerResult {
    HttpResponse { response: DocumentWorkerHttpResponse },
    Storage { entries: Vec<(String, String)> },
    Cookie { value: String },
    Applied,
    Error { message: String },
}
