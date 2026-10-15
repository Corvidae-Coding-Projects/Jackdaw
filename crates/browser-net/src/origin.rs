use std::sync::atomic::{AtomicU64, Ordering};

use url::Url;

static NEXT_OPAQUE_ORIGIN: AtomicU64 = AtomicU64::new(1);

/// Browser security origin identity.
///
/// Tuple origins compare by normalized scheme, host, and effective port. Opaque
/// origins carry a unique identity and therefore never collapse to an empty or
/// permissive tuple.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Origin {
    Tuple {
        scheme: String,
        host: String,
        port: u16,
    },
    Opaque(OpaqueOriginId),
}

/// Process-local identity for an opaque origin.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OpaqueOriginId(u64);

impl Origin {
    /// Derive a tuple origin when the URL has both a host and a known effective
    /// port. Schemes without that authority are not silently treated as tuple
    /// origins.
    pub fn tuple(url: &Url) -> Option<Self> {
        let host = url.host_str()?.to_ascii_lowercase();
        let port = url.port_or_known_default()?;
        Some(Self::Tuple {
            scheme: url.scheme().to_ascii_lowercase(),
            host,
            port,
        })
    }

    /// Allocate an origin that is distinct from every tuple and other opaque
    /// origin in this process.
    pub fn new_opaque() -> Self {
        Self::Opaque(OpaqueOriginId(
            NEXT_OPAQUE_ORIGIN.fetch_add(1, Ordering::Relaxed),
        ))
    }

    pub fn scheme(&self) -> Option<&str> {
        match self {
            Self::Tuple { scheme, .. } => Some(scheme),
            Self::Opaque(_) => None,
        }
    }

    pub fn host(&self) -> Option<&str> {
        match self {
            Self::Tuple { host, .. } => Some(host),
            Self::Opaque(_) => None,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Self::Tuple { port, .. } => Some(*port),
            Self::Opaque(_) => None,
        }
    }

    pub fn is_same_origin(&self, other: &Self) -> bool {
        self == other
    }

    /// Serialize an origin for diagnostics or protocol policy. Opaque origins
    /// deliberately use the standard `null` sentinel without exposing their
    /// internal identity.
    pub fn serialize(&self) -> String {
        match self {
            Self::Tuple { scheme, host, port } => {
                let default_port =
                    matches!((scheme.as_str(), *port), ("http", 80) | ("https", 443));
                if default_port {
                    format!("{scheme}://{host}")
                } else {
                    format!("{scheme}://{host}:{port}")
                }
            }
            Self::Opaque(_) => "null".to_owned(),
        }
    }
}
