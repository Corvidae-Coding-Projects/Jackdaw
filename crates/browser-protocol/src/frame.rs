use serde::{Deserialize, Serialize};

use crate::{DocumentId, FrameId, TabId, ViewId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub tab_id: TabId,
    pub frame_id: FrameId,
    pub document_id: DocumentId,
    pub view_id: ViewId,
    pub width: u32,
    pub height: u32,
    pub pixel_scale: f32,
    pub pixels: FramePixels,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum FramePixels {
    Owned {
        format: PixelFormat,
        stride: u32,
        bytes: Vec<u8>,
    },
    LocalSurface {
        descriptor: String,
        format: PixelFormat,
        stride: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    Rgba8Premultiplied,
    Bgra8Premultiplied,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameInput {
    Pointer {
        phase: PointerPhase,
        x: f32,
        y: f32,
        button: Option<PointerButton>,
    },
    Key {
        phase: KeyPhase,
        key: String,
        text: Option<String>,
    },
    Scroll {
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerPhase {
    Move,
    Down,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Primary,
    Auxiliary,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyPhase {
    Down,
    Up,
}
