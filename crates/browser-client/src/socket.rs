use std::{
    error::Error,
    fmt,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use browser_protocol::{CommandReply, CommandRequest};

pub const DEFAULT_MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Protocol(serde_json::Error),
    EmptyReply,
    ReplyTooLarge { limit: usize },
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "browser transport failed: {error}"),
            Self::Protocol(error) => {
                write!(formatter, "browser protocol reply was invalid: {error}")
            }
            Self::EmptyReply => formatter.write_str("browserd closed without a reply"),
            Self::ReplyTooLarge { limit } => {
                write!(formatter, "browserd reply exceeded the {limit}-byte limit")
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::EmptyReply | Self::ReplyTooLarge { .. } => None,
        }
    }
}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error)
    }
}

#[derive(Debug, Clone)]
pub struct BrowserClient {
    socket: PathBuf,
    max_reply_bytes: usize,
}

impl BrowserClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            max_reply_bytes: DEFAULT_MAX_REPLY_BYTES,
        }
    }

    pub fn with_max_reply_bytes(mut self, max_reply_bytes: usize) -> Self {
        self.max_reply_bytes = max_reply_bytes.max(1);
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn send(&self, request: &CommandRequest) -> Result<CommandReply, ClientError> {
        let stream = UnixStream::connect(&self.socket)?;
        let mut writer = BufWriter::new(stream.try_clone()?);
        serde_json::to_writer(&mut writer, request)?;
        writer.write_all(b"\n")?;
        writer.flush()?;

        let mut response = Vec::new();
        let read_limit = self.max_reply_bytes.saturating_add(1);
        let mut reader = BufReader::new(stream).take(read_limit as u64);
        reader.read_until(b'\n', &mut response)?;
        if response.is_empty() {
            return Err(ClientError::EmptyReply);
        }
        while response
            .last()
            .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
        {
            response.pop();
        }
        if response.len() > self.max_reply_bytes {
            return Err(ClientError::ReplyTooLarge {
                limit: self.max_reply_bytes,
            });
        }
        Ok(serde_json::from_slice(&response)?)
    }
}
