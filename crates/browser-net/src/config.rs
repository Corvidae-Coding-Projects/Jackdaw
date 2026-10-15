use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::NetworkError;

#[cfg(feature = "tls")]
use rustls::ClientConfig;

const DEFAULT_MAX_REDIRECTS: usize = 10;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Immutable limits and deadlines shared by every client in one browser service.
#[derive(Clone, Debug)]
pub struct BrowserNetworkConfig {
    pub max_redirects: usize,
    pub max_response_bytes: usize,
    pub io_timeout: Duration,
    pub connect_timeout: Duration,
    pub cancellation_poll_interval: Duration,
    #[cfg(feature = "tls")]
    pub(crate) tls: Option<Arc<ClientConfig>>,
}

impl BrowserNetworkConfig {
    /// Construct the fail-closed production transport configuration.
    ///
    /// TLS provider and trust-root state will be added to this immutable value once the
    /// separately audited provider gate is integrated.
    pub fn production() -> Result<Self, NetworkError> {
        Self::builder().build()
    }

    pub fn builder() -> BrowserNetworkConfigBuilder {
        BrowserNetworkConfigBuilder::default()
    }

    pub(crate) fn validate(&self) -> Result<(), NetworkError> {
        if self.max_response_bytes == 0 {
            return Err(NetworkError::InvalidConfiguration(
                "maximum response size must be greater than zero",
            ));
        }
        if self.io_timeout.is_zero() {
            return Err(NetworkError::InvalidConfiguration(
                "I/O timeout must be greater than zero",
            ));
        }
        if self.connect_timeout.is_zero() {
            return Err(NetworkError::InvalidConfiguration(
                "connect timeout must be greater than zero",
            ));
        }
        if self.cancellation_poll_interval.is_zero() {
            return Err(NetworkError::InvalidConfiguration(
                "cancellation poll interval must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl Default for BrowserNetworkConfig {
    fn default() -> Self {
        Self {
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            io_timeout: DEFAULT_IO_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            cancellation_poll_interval: DEFAULT_CANCELLATION_POLL_INTERVAL,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

/// Validated builder for browser-owned network limits.
#[derive(Clone, Debug, Default)]
pub struct BrowserNetworkConfigBuilder {
    config: BrowserNetworkConfig,
    #[cfg(feature = "tls")]
    additional_root_ders: Vec<Vec<u8>>,
}

impl BrowserNetworkConfigBuilder {
    pub fn max_redirects(mut self, max_redirects: usize) -> Self {
        self.config.max_redirects = max_redirects;
        self
    }

    pub fn max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.config.max_response_bytes = max_response_bytes;
        self
    }

    pub fn io_timeout(mut self, io_timeout: Duration) -> Self {
        self.config.io_timeout = io_timeout;
        self
    }

    pub fn connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.config.connect_timeout = connect_timeout;
        self
    }

    pub fn cancellation_poll_interval(mut self, interval: Duration) -> Self {
        self.config.cancellation_poll_interval = interval;
        self
    }

    #[cfg(feature = "tls")]
    pub fn add_root_der(mut self, bytes: Vec<u8>) -> Result<Self, NetworkError> {
        crate::tls::validate_root_der(&bytes)?;
        self.additional_root_ders.push(bytes);
        Ok(self)
    }

    pub fn build(self) -> Result<BrowserNetworkConfig, NetworkError> {
        #[cfg(feature = "tls")]
        {
            let mut config = self.config;
            config.validate()?;
            config.tls = Some(crate::tls::client_config(&self.additional_root_ders)?);
            Ok(config)
        }
        #[cfg(not(feature = "tls"))]
        {
            self.config.validate()?;
            Ok(self.config)
        }
    }
}

/// Deadline and cancellation state for one network operation.
#[derive(Clone, Debug)]
pub struct RequestControl {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl RequestControl {
    pub fn for_timeout(timeout: Duration) -> Result<Self, NetworkError> {
        if timeout.is_zero() {
            return Err(NetworkError::InvalidConfiguration(
                "request timeout must be greater than zero",
            ));
        }
        let deadline =
            Instant::now()
                .checked_add(timeout)
                .ok_or(NetworkError::InvalidConfiguration(
                    "request deadline is outside the supported range",
                ))?;
        Ok(Self {
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }
}
