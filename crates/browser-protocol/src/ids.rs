use std::{error::Error, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// Wire protocol version. Version negotiation is explicit rather than inferred
/// from the shape of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    pub const CURRENT: Self = Self(1);

    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        if value == Self::CURRENT.get() {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format_args!(
                "unsupported protocol version {value}; expected {}",
                Self::CURRENT.get()
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId {
    kind: &'static str,
    reason: &'static str,
}

impl InvalidId {
    const fn new(kind: &'static str, reason: &'static str) -> Self {
        Self { kind, reason }
    }
}

impl fmt::Display for InvalidId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {}: {}", self.kind, self.reason)
    }
}

impl Error for InvalidId {}

fn validate_prefixed(value: &str, prefix: &str, kind: &'static str) -> Result<(), InvalidId> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(InvalidId::new(kind, "wrong prefix"));
    };
    if suffix.is_empty() {
        return Err(InvalidId::new(kind, "missing identifier body"));
    }
    if suffix.len() > 128 {
        return Err(InvalidId::new(kind, "identifier body is too long"));
    }
    if !suffix
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(InvalidId::new(
            kind,
            "identifier body contains an unsupported character",
        ));
    }
    Ok(())
}

macro_rules! opaque_id {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, InvalidId> {
                let value = value.into();
                validate_prefixed(&value, $prefix, $kind)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(RequestId, "r_", "request ID");
opaque_id!(ProfileId, "p_", "profile ID");
opaque_id!(TabId, "t_", "tab ID");
opaque_id!(FrameId, "f_", "frame ID");
opaque_id!(DocumentId, "doc_", "document ID");
opaque_id!(ViewId, "v_", "view ID");
opaque_id!(OperationId, "op_", "operation ID");
opaque_id!(DownloadId, "d_", "download ID");
opaque_id!(ServiceInstanceId, "svc_", "service instance ID");

/// A generation-checked action reference. The view portion makes stale-view
/// failures visible at the protocol boundary instead of retargeting a new node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ElementRef(String);

impl ElementRef {
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidId> {
        let value = value.into();
        let Some((view, element)) = value.split_once(':') else {
            return Err(InvalidId::new(
                "element reference",
                "missing view separator",
            ));
        };
        validate_prefixed(view, "v_", "element reference view")?;
        validate_prefixed(element, "e_", "element reference node")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn view_id(&self) -> ViewId {
        let view = self
            .0
            .split_once(':')
            .expect("validated element reference")
            .0;
        ViewId::parse(view).expect("validated element reference view")
    }
}

impl fmt::Display for ElementRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ElementRef {
    type Err = InvalidId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ElementRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}
