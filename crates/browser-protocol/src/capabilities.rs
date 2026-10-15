use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ProtocolVersion;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub protocol_version: ProtocolVersion,
    pub product_version: String,
    pub engine_versions: BTreeMap<String, String>,
    pub commands: Vec<CommandCapability>,
    pub observation_modes: Vec<String>,
    pub frame_formats: Vec<String>,
    pub limitations: Vec<KnownLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandCapability {
    pub name: String,
    pub schema_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownLimitation {
    pub capability: String,
    pub reason: String,
}
