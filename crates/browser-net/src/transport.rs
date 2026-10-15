use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

#[cfg(feature = "tls")]
use std::sync::Arc;

use url::Url;

#[cfg(feature = "tls")]
use rustls::{ClientConnection, StreamOwned, pki_types::ServerName};

use crate::{BrowserNetworkConfig, NetworkEffect, NetworkError, NetworkPhase, RequestControl};

/// One connected browser transport. TLS will add a second private variant once its provider gate
/// clears; request framing and response parsing remain independent of the concrete stream.
pub(crate) enum ConnectionStream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl ConnectionStream {
    pub(crate) fn connect(
        url: &Url,
        config: &BrowserNetworkConfig,
        control: &RequestControl,
    ) -> Result<Self, NetworkError> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(NetworkError::CredentialsInUrl);
        }
        match url.scheme() {
            "http" => {}
            "https" => {
                #[cfg(not(feature = "tls"))]
                return Err(NetworkError::UnsupportedScheme("https".to_owned()));
                #[cfg(feature = "tls")]
                if config.tls.is_none() {
                    return Err(NetworkError::UnsupportedScheme("https".to_owned()));
                }
            }
            scheme => return Err(NetworkError::UnsupportedScheme(scheme.to_owned())),
        }

        let host = url.host_str().ok_or(NetworkError::MissingHost)?;
        let port = url
            .port_or_known_default()
            .ok_or(NetworkError::MissingHost)?;
        check_control(control, NetworkPhase::Resolve, NetworkEffect::None)?;

        // `ToSocketAddrs` is the synchronous resolver boundary for this transport slice. Its
        // returned address set is finite; connection attempts share one request deadline.
        let addresses = (host, port).to_socket_addrs().map_err(|source| {
            NetworkError::operation(NetworkPhase::Resolve, NetworkEffect::None, source)
        })?;
        let mut attempted = false;
        let mut final_error = None;
        for address in addresses {
            attempted = true;
            check_control(control, NetworkPhase::Connect, NetworkEffect::None)?;
            let remaining = control.remaining().ok_or(NetworkError::Cancelled {
                phase: NetworkPhase::Connect,
                effect: NetworkEffect::None,
            })?;
            let timeout = remaining.min(config.connect_timeout);
            match TcpStream::connect_timeout(&address, timeout) {
                Ok(stream) => {
                    let io_timeout = bounded_timeout(config.io_timeout, control).ok_or(
                        NetworkError::Cancelled {
                            phase: NetworkPhase::Connect,
                            effect: NetworkEffect::None,
                        },
                    )?;
                    set_socket_timeouts(
                        &stream,
                        config.cancellation_poll_interval.min(io_timeout),
                        NetworkPhase::Connect,
                        NetworkEffect::None,
                    )?;
                    if url.scheme() == "http" {
                        return Ok(Self::Plain(stream));
                    }
                    #[cfg(feature = "tls")]
                    {
                        let mut stream = stream;
                        let tls = config
                            .tls
                            .as_ref()
                            .ok_or_else(|| NetworkError::UnsupportedScheme("https".to_owned()))?;
                        let server_name = ServerName::try_from(host.to_owned())
                            .map_err(|_| NetworkError::InvalidServerIdentity(host.to_owned()))?;
                        let connection = ClientConnection::new(Arc::clone(tls), server_name)
                            .map_err(|error| NetworkError::TlsInitialization(error.to_string()))?;
                        let poll_timeout = config.cancellation_poll_interval.min(io_timeout);
                        set_socket_timeouts(
                            &stream,
                            poll_timeout,
                            NetworkPhase::TlsHandshake,
                            NetworkEffect::None,
                        )?;
                        let mut connection = connection;
                        while connection.is_handshaking() {
                            check_control(
                                control,
                                NetworkPhase::TlsHandshake,
                                NetworkEffect::None,
                            )?;
                            match connection.complete_io(&mut stream) {
                                Ok(_) => {}
                                Err(source)
                                    if matches!(
                                        source.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                    ) => {}
                                Err(source) => {
                                    return Err(NetworkError::TlsHandshake(source.to_string()));
                                }
                            }
                        }
                        let io_timeout = bounded_timeout(config.io_timeout, control).ok_or(
                            NetworkError::Cancelled {
                                phase: NetworkPhase::TlsHandshake,
                                effect: NetworkEffect::None,
                            },
                        )?;
                        set_socket_timeouts(
                            &stream,
                            config.cancellation_poll_interval.min(io_timeout),
                            NetworkPhase::TlsHandshake,
                            NetworkEffect::None,
                        )?;
                        return Ok(Self::Tls(Box::new(StreamOwned::new(connection, stream))));
                    }
                    #[cfg(not(feature = "tls"))]
                    unreachable!("HTTPS is rejected before connecting without development TLS");
                }
                Err(source) => final_error = Some(source),
            }
        }
        let source = final_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                if attempted {
                    "all resolved addresses failed"
                } else {
                    "host resolved to no addresses"
                },
            )
        });
        Err(NetworkError::operation(
            NetworkPhase::Connect,
            NetworkEffect::None,
            source,
        ))
    }
}

fn bounded_timeout(configured: Duration, control: &RequestControl) -> Option<Duration> {
    control
        .remaining()
        .map(|remaining| remaining.min(configured))
}

fn set_socket_timeouts(
    stream: &TcpStream,
    timeout: Duration,
    phase: NetworkPhase,
    effect: NetworkEffect,
) -> Result<(), NetworkError> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|source| NetworkError::operation(phase, effect, source))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|source| NetworkError::operation(phase, effect, source))
}

fn check_control(
    control: &RequestControl,
    phase: NetworkPhase,
    effect: NetworkEffect,
) -> Result<(), NetworkError> {
    if control.is_cancelled() || control.remaining().is_none() {
        return Err(NetworkError::Cancelled { phase, effect });
    }
    Ok(())
}

impl Read for ConnectionStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for ConnectionStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.flush(),
        }
    }
}
