use std::{fmt, io};

use browser_store::StoreError;

/// Whether an unsuccessful operation is known to have had no remote effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkEffect {
    None,
    Unknown,
}

/// Stable phase in which a network operation stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPhase {
    Resolve,
    Connect,
    TlsHandshake,
    SendRequest,
    AwaitResponse,
    ReadResponse,
}

/// Typed failures from browser transport and redirect policy.
#[derive(Debug)]
pub enum NetworkError {
    InvalidConfiguration(&'static str),
    InvalidTrustAnchor(String),
    TlsInitialization(String),
    InvalidServerIdentity(String),
    TlsHandshake(String),
    InvalidUrl(url::ParseError),
    UnsupportedScheme(String),
    MissingHost,
    CredentialsInUrl,
    UnsupportedMethod(String),
    Cancelled {
        phase: NetworkPhase,
        effect: NetworkEffect,
    },
    Operation {
        phase: NetworkPhase,
        effect: NetworkEffect,
        source: io::Error,
    },
    Io(io::Error),
    MalformedResponse(&'static str),
    InvalidStatus(String),
    InvalidContentLength(String),
    UnsupportedTransferEncoding(String),
    ResponseTooLarge {
        limit: usize,
    },
    RedirectMissingLocation {
        status: u16,
    },
    TooManyRedirects {
        limit: usize,
    },
    Store(StoreError),
}

impl NetworkError {
    pub(crate) fn operation(phase: NetworkPhase, effect: NetworkEffect, source: io::Error) -> Self {
        Self::Operation {
            phase,
            effect,
            source,
        }
    }

    /// Conservative remote-effect provenance for protocol error mapping.
    pub fn effect(&self) -> NetworkEffect {
        match self {
            Self::InvalidConfiguration(_)
            | Self::InvalidTrustAnchor(_)
            | Self::TlsInitialization(_)
            | Self::InvalidServerIdentity(_)
            | Self::TlsHandshake(_)
            | Self::InvalidUrl(_)
            | Self::UnsupportedScheme(_)
            | Self::MissingHost
            | Self::CredentialsInUrl
            | Self::UnsupportedMethod(_) => NetworkEffect::None,
            Self::Cancelled { effect, .. } | Self::Operation { effect, .. } => *effect,
            Self::Io(_)
            | Self::MalformedResponse(_)
            | Self::InvalidStatus(_)
            | Self::InvalidContentLength(_)
            | Self::UnsupportedTransferEncoding(_)
            | Self::ResponseTooLarge { .. }
            | Self::RedirectMissingLocation { .. }
            | Self::TooManyRedirects { .. }
            | Self::Store(_) => NetworkEffect::Unknown,
        }
    }
}

impl fmt::Display for NetworkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(reason) => {
                write!(formatter, "invalid browser network configuration: {reason}")
            }
            Self::InvalidTrustAnchor(reason) => {
                write!(formatter, "invalid TLS trust anchor: {reason}")
            }
            Self::TlsInitialization(reason) => {
                write!(formatter, "TLS client configuration failed: {reason}")
            }
            Self::InvalidServerIdentity(host) => {
                write!(formatter, "invalid TLS server identity {host:?}")
            }
            Self::TlsHandshake(reason) => {
                write!(formatter, "TLS handshake failed: {reason}")
            }
            Self::InvalidUrl(error) => write!(formatter, "invalid URL: {error}"),
            Self::UnsupportedScheme(scheme) => write!(
                formatter,
                "unsupported URL scheme {scheme:?}; HTTPS is unavailable until the audited Rust-only TLS gate is complete"
            ),
            Self::MissingHost => formatter.write_str("network URL has no host"),
            Self::CredentialsInUrl => {
                formatter.write_str("credentials in network URLs are not supported")
            }
            Self::UnsupportedMethod(method) => {
                write!(formatter, "unsupported browser form method {method:?}")
            }
            Self::Cancelled { phase, effect } => {
                write!(
                    formatter,
                    "network operation was cancelled during {phase:?} with remote effect {effect:?}"
                )
            }
            Self::Operation {
                phase,
                effect,
                source,
            } => write!(
                formatter,
                "network operation failed during {phase:?} with remote effect {effect:?}: {source}"
            ),
            Self::Io(error) => write!(formatter, "network I/O failed: {error}"),
            Self::MalformedResponse(reason) => {
                write!(formatter, "malformed HTTP response: {reason}")
            }
            Self::InvalidStatus(value) => write!(formatter, "invalid HTTP status {value:?}"),
            Self::InvalidContentLength(value) => {
                write!(formatter, "invalid Content-Length {value:?}")
            }
            Self::UnsupportedTransferEncoding(value) => {
                write!(formatter, "unsupported Transfer-Encoding {value:?}")
            }
            Self::ResponseTooLarge { limit } => {
                write!(formatter, "HTTP response exceeded the {limit}-byte limit")
            }
            Self::RedirectMissingLocation { status } => {
                write!(
                    formatter,
                    "HTTP redirect status {status} did not include Location"
                )
            }
            Self::TooManyRedirects { limit } => {
                write!(formatter, "HTTP redirect limit of {limit} was exceeded")
            }
            Self::Store(error) => write!(formatter, "profile cookie persistence failed: {error}"),
        }
    }
}

impl std::error::Error for NetworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::Operation { source, .. } => Some(source),
            Self::Io(error) => Some(error),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<url::ParseError> for NetworkError {
    fn from(error: url::ParseError) -> Self {
        Self::InvalidUrl(error)
    }
}

impl From<io::Error> for NetworkError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for NetworkError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
