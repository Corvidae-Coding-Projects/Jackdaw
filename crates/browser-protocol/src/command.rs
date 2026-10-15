use serde::{Deserialize, Serialize};

use crate::{
    DownloadId, ElementRef, EventCursor, FrameId, FrameInput, ObservationCursor, ProfileId,
    ProtocolVersion, RequestId, TabId, ViewId,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub profile_id: ProfileId,
    pub tab_id: Option<TabId>,
    pub command: Command,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "parameters", rename_all = "snake_case")]
pub enum Command {
    Status,
    Shutdown,
    Version,
    Capabilities,
    Profile(ProfileCommand),
    Tab(TabCommand),
    Navigate(NavigationCommand),
    Observe(ObserveCommand),
    Read(ReadCommand),
    Find(FindCommand),
    Screenshot(ScreenshotCommand),
    FrameInput(FrameInputCommand),
    Interact(InteractionCommand),
    Wait(WaitCommand),
    Upload(UploadCommand),
    Download(DownloadCommand),
    Operation(OperationCommand),
    Events(EventCommand),
    Diagnostic(DiagnosticCommand),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ProfileCommand {
    Create { profile: ProfileId },
    List,
    Open { profile: ProfileId },
    Close { profile: ProfileId },
    Remove { profile: ProfileId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TabCommand {
    List,
    New { url: Option<String> },
    Close { tab: TabId },
    Activate { tab: TabId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum NavigationCommand {
    Open { url: String },
    Back,
    Forward,
    Reload,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveCommand {
    pub mode: ObservationMode,
    pub frame_id: Option<FrameId>,
    pub cursor: Option<ObservationCursor>,
    pub max_items: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadCommand {
    pub frame_id: Option<FrameId>,
    pub cursor: Option<ObservationCursor>,
    pub max_items: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindCommand {
    pub query: String,
    pub frame_id: Option<FrameId>,
    pub max_items: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotCommand {
    pub output_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameInputCommand {
    pub view_id: ViewId,
    pub input: FrameInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationMode {
    Text,
    Controls,
    Links,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum InteractionCommand {
    Click {
        target: ElementRef,
    },
    ClickAt {
        x: i32,
        y: i32,
    },
    Focus {
        target: ElementRef,
    },
    Fill {
        target: ElementRef,
        value: String,
    },
    Type {
        target: ElementRef,
        text: String,
    },
    Key {
        target: Option<ElementRef>,
        key: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<String>,
    },
    Scroll {
        target: Option<ElementRef>,
        delta_x: i32,
        delta_y: i32,
    },
    Select {
        target: ElementRef,
        values: Vec<String>,
    },
    Check {
        target: ElementRef,
        checked: bool,
    },
    Submit {
        target: ElementRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum WaitCommand {
    Idle,
    Url {
        url: String,
    },
    Text {
        value: String,
    },
    RoleName {
        role: String,
        name: String,
    },
    TargetState {
        target: ElementRef,
        state: TargetState,
    },
    Download {
        download_id: DownloadId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    Attached,
    Detached,
    Enabled,
    Disabled,
    Focused,
    Checked,
    Unchecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadCommand {
    pub target: ElementRef,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DownloadCommand {
    Start {
        url: String,
    },
    List,
    Wait {
        download_id: DownloadId,
        output_path: String,
    },
    Cancel {
        download_id: DownloadId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OperationCommand {
    Get { request_id: RequestId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCommand {
    pub cursor: Option<EventCursor>,
    pub max_events: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DiagnosticCommand {
    RenderFrame,
    RenderFrameViewport { width: u32, height: u32 },
    Evaluate { source: String },
    Console,
    Network,
}
