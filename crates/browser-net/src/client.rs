use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use browser_store::{OperationRecord, ProfileCookieStore, TabSessionRecord};

use crate::{
    BrowserNetworkConfig, BrowserRequest, BrowserResponse, NetworkEffect, NetworkError,
    NetworkPhase, RedirectHop, RequestControl, cookies::CookieJar, transport::ConnectionStream,
};

/// A bounded browser document client.
#[derive(Clone)]
pub struct BrowserClient {
    network_config: BrowserNetworkConfig,
    cookie_jar: Arc<Mutex<CookieJar>>,
    profile_store: Option<Arc<Mutex<ProfileCookieStore>>>,
}

impl Default for BrowserClient {
    fn default() -> Self {
        Self::from_parts(BrowserNetworkConfig::default(), CookieJar::default(), None)
    }
}

impl BrowserClient {
    fn from_parts(
        network_config: BrowserNetworkConfig,
        cookie_jar: CookieJar,
        profile_store: Option<Arc<Mutex<ProfileCookieStore>>>,
    ) -> Self {
        Self {
            network_config,
            cookie_jar: Arc::new(Mutex::new(cookie_jar)),
            profile_store,
        }
    }

    pub fn new(max_redirects: usize, max_response_bytes: usize) -> Self {
        let config = BrowserNetworkConfig {
            max_redirects,
            max_response_bytes,
            ..BrowserNetworkConfig::default()
        };
        Self::from_parts(config, CookieJar::default(), None)
    }

    pub fn with_config(config: BrowserNetworkConfig) -> Result<Self, NetworkError> {
        config.validate()?;
        Ok(Self::from_parts(config, CookieJar::default(), None))
    }

    /// Open a client whose persistent cookies are owned by the profile database at `path`.
    /// Clones share the immutable network configuration, in-memory jar, and database handle.
    pub fn open_profile(path: impl AsRef<Path>) -> Result<Self, NetworkError> {
        Self::open_profile_with_config(path, BrowserNetworkConfig::production()?)
    }

    pub fn open_profile_with_config(
        path: impl AsRef<Path>,
        config: BrowserNetworkConfig,
    ) -> Result<Self, NetworkError> {
        config.validate()?;
        let store = ProfileCookieStore::open(path.as_ref())?;
        let cookies = store.load_cookies()?;
        Ok(Self::from_parts(
            config,
            CookieJar::from_persistent(cookies),
            Some(Arc::new(Mutex::new(store))),
        ))
    }

    pub fn fetch_url(&self, url: &str) -> Result<BrowserResponse, NetworkError> {
        self.fetch(BrowserRequest::parse(url)?)
    }

    /// Fetch a URL under a caller-owned deadline and cancellation signal.
    pub fn fetch_url_with_control(
        &self,
        url: &str,
        control: &RequestControl,
    ) -> Result<BrowserResponse, NetworkError> {
        self.fetch_with_control(BrowserRequest::parse(url)?, control)
    }

    /// Return the cookies visible to page script at this document URL.
    /// HttpOnly cookies are deliberately excluded by the profile cookie jar.
    pub fn document_cookie(&self, url: &str) -> Result<String, NetworkError> {
        let url = url::Url::parse(url)?;
        Ok(self
            .cookie_jar
            .lock()
            .expect("browser cookie jar lock poisoned")
            .document_header(&url)
            .unwrap_or_default())
    }

    /// Apply one page `document.cookie` assignment through the profile cookie policy.
    pub fn store_document_cookie(&self, url: &str, header: &str) -> Result<(), NetworkError> {
        let url = url::Url::parse(url)?;
        self.cookie_jar
            .lock()
            .expect("browser cookie jar lock poisoned")
            .store_document_cookie(&url, header);
        self.persist_cookies()
    }

    /// Load the durable committed-tab snapshot owned by this profile.
    pub fn load_tab_sessions(&self) -> Result<Vec<TabSessionRecord>, NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(Vec::new());
        };
        Ok(store
            .lock()
            .expect("browser profile store lock poisoned")
            .load_tab_sessions()?)
    }

    /// Atomically replace the durable committed-tab snapshot owned by this profile.
    pub fn replace_tab_sessions(&self, tabs: &[TabSessionRecord]) -> Result<(), NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(());
        };
        store
            .lock()
            .expect("browser profile store lock poisoned")
            .replace_tab_sessions(tabs)?;
        Ok(())
    }

    /// Load the durable local-storage map owned by one serialized origin.
    pub fn load_local_storage(
        &self,
        origin: &str,
    ) -> Result<BTreeMap<String, String>, NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(BTreeMap::new());
        };
        Ok(store
            .lock()
            .expect("browser profile store lock poisoned")
            .load_local_storage(origin)?)
    }

    /// Atomically replace the durable local-storage map owned by one serialized origin.
    pub fn replace_local_storage(
        &self,
        origin: &str,
        entries: &BTreeMap<String, String>,
    ) -> Result<(), NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(());
        };
        store
            .lock()
            .expect("browser profile store lock poisoned")
            .replace_local_storage(origin, entries)?;
        Ok(())
    }

    /// Load durable completed operations owned by this profile.
    pub fn load_operation_records(&self) -> Result<Vec<OperationRecord>, NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(Vec::new());
        };
        Ok(store
            .lock()
            .expect("browser profile store lock poisoned")
            .load_operations()?)
    }

    /// Load bounded durable markers for completed operations whose replies have expired.
    pub fn load_operation_tombstones(&self) -> Result<Vec<String>, NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(Vec::new());
        };
        Ok(store
            .lock()
            .expect("browser profile store lock poisoned")
            .load_operation_tombstones()?)
    }

    /// Atomically retain one completed operation and return committed retention evictions.
    pub fn put_operation_record(
        &self,
        operation: &OperationRecord,
        max_retained: usize,
    ) -> Result<Vec<String>, NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(Vec::new());
        };
        Ok(store
            .lock()
            .expect("browser profile store lock poisoned")
            .put_operation(operation, max_retained)?)
    }

    fn persist_cookies(&self) -> Result<(), NetworkError> {
        let Some(store) = &self.profile_store else {
            return Ok(());
        };
        let records = self
            .cookie_jar
            .lock()
            .expect("browser cookie jar lock poisoned")
            .persistent_records();
        store
            .lock()
            .expect("browser profile store lock poisoned")
            .replace_cookies(&records)?;
        Ok(())
    }

    pub fn fetch(&self, request: BrowserRequest) -> Result<BrowserResponse, NetworkError> {
        let timeout = self
            .network_config
            .connect_timeout
            .saturating_add(self.network_config.io_timeout);
        let control = RequestControl::for_timeout(timeout)?;
        self.fetch_with_control(request, &control)
    }

    /// Execute one redirect chain under a caller-owned deadline and cancellation signal.
    pub fn fetch_with_control(
        &self,
        request: BrowserRequest,
        control: &RequestControl,
    ) -> Result<BrowserResponse, NetworkError> {
        let mut current = request;
        let mut redirects = Vec::new();
        loop {
            // Fetch defaults credentials to same-origin. Cross-origin page requests carry an
            // Origin header but neither send stored cookies nor accept response cookies.
            let cookie_header = current
                .origin
                .is_none()
                .then(|| {
                    self.cookie_jar
                        .lock()
                        .expect("browser cookie jar lock poisoned")
                        .request_header(&current.url)
                })
                .flatten();
            let response = self.send_once(&current, cookie_header.as_deref(), control)?;
            if current.origin.is_none() {
                {
                    let mut jar = self
                        .cookie_jar
                        .lock()
                        .expect("browser cookie jar lock poisoned");
                    for header in &response.set_cookie_headers {
                        jar.store_response_cookie(&current.url, header);
                    }
                }
                self.persist_cookies()?;
            }
            if !is_redirect(response.status) {
                return Ok(BrowserResponse {
                    url: current.url,
                    status: response.status,
                    headers: response.headers,
                    body: response.body,
                    redirects,
                });
            }
            if redirects.len() >= self.network_config.max_redirects {
                return Err(NetworkError::TooManyRedirects {
                    limit: self.network_config.max_redirects,
                });
            }
            let location =
                response
                    .headers
                    .get("location")
                    .ok_or(NetworkError::RedirectMissingLocation {
                        status: response.status,
                    })?;
            let next = current.url.join(location)?;
            redirects.push(RedirectHop {
                status: response.status,
                from: current.url.clone(),
                to: next.clone(),
            });
            current = current.redirected(response.status, next);
        }
    }

    fn send_once(
        &self,
        request: &BrowserRequest,
        cookie_header: Option<&str>,
        control: &RequestControl,
    ) -> Result<RawResponse, NetworkError> {
        let url = &request.url;
        let host = url.host_str().ok_or(NetworkError::MissingHost)?;
        let mut stream = ConnectionStream::connect(url, &self.network_config, control)?;

        let mut target = url.path().to_owned();
        if target.is_empty() {
            target.push('/');
        }
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let host_header = match url.port() {
            Some(explicit_port) => format!("{host}:{explicit_port}"),
            None => host.to_owned(),
        };
        let send_error = |source| {
            NetworkError::operation(NetworkPhase::SendRequest, NetworkEffect::Unknown, source)
        };
        write!(
            stream,
            "{} {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: rwb/{}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            request.method.as_str(),
            env!("CARGO_PKG_VERSION")
        )
        .map_err(send_error)?;
        if let Some(origin) = &request.origin {
            write!(stream, "Origin: {origin}\r\n").map_err(send_error)?;
        }
        if let Some(cookie_header) = cookie_header {
            write!(stream, "Cookie: {cookie_header}\r\n").map_err(send_error)?;
        }
        if let Some(content_type) = &request.content_type {
            write!(
                stream,
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
                request.body.len()
            )
            .map_err(send_error)?;
        } else if !request.body.is_empty() {
            write!(stream, "Content-Length: {}\r\n", request.body.len()).map_err(send_error)?;
        }
        stream.write_all(b"\r\n").map_err(send_error)?;
        stream.write_all(&request.body).map_err(send_error)?;
        stream.flush().map_err(send_error)?;

        let read_limit = self
            .network_config
            .max_response_bytes
            .checked_add(1)
            .ok_or(NetworkError::ResponseTooLarge {
                limit: self.network_config.max_response_bytes,
            })?;
        let mut bytes = Vec::new();
        let mut limited =
            stream.take(u64::try_from(read_limit).expect("usize fits u64 on supported Linux"));
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            if control.is_cancelled() || control.remaining().is_none() {
                return Err(NetworkError::Cancelled {
                    phase: NetworkPhase::ReadResponse,
                    effect: NetworkEffect::Unknown,
                });
            }
            match limited.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    bytes.extend_from_slice(&buffer[..read]);
                    if response_has_complete_framing(&bytes, self.network_config.max_response_bytes)
                    {
                        break;
                    }
                }
                Err(source)
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                // Some otherwise valid HTTPS servers close the TCP connection without a TLS
                // `close_notify`. Rustls reports that as `UnexpectedEof` after returning all
                // available plaintext. Treat it as the response boundary and leave framing
                // validation to `parse_response`, which still rejects truncated Content-Length
                // and chunked bodies.
                Err(source) if source.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(source) => {
                    return Err(NetworkError::operation(
                        NetworkPhase::ReadResponse,
                        NetworkEffect::Unknown,
                        source,
                    ));
                }
            }
        }
        if bytes.len() > self.network_config.max_response_bytes {
            return Err(NetworkError::ResponseTooLarge {
                limit: self.network_config.max_response_bytes,
            });
        }
        parse_response(&bytes, self.network_config.max_response_bytes)
    }
}

struct RawResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    set_cookie_headers: Vec<String>,
    body: Vec<u8>,
}

fn response_has_complete_framing(bytes: &[u8], limit: usize) -> bool {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let Ok(head) = std::str::from_utf8(&bytes[..header_end]) else {
        return false;
    };
    let mut content_length = None;
    let mut chunked = false;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let Ok(length) = value.trim().parse::<usize>() else {
                return false;
            };
            content_length = Some(length);
        } else if name.trim().eq_ignore_ascii_case("transfer-encoding")
            && value.trim().eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
    let body = &bytes[header_end + 4..];
    if chunked {
        return decode_chunked(body, limit).is_ok();
    }
    content_length.is_some_and(|length| length <= limit && body.len() >= length)
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn parse_response(bytes: &[u8], limit: usize) -> Result<RawResponse, NetworkError> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(NetworkError::MalformedResponse("missing header terminator"))?;
    let head = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| NetworkError::MalformedResponse("headers are not valid ASCII/UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or(NetworkError::MalformedResponse("missing status line"))?;
    let mut status_parts = status_line.split_whitespace();
    let version = status_parts
        .next()
        .ok_or(NetworkError::MalformedResponse("missing HTTP version"))?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(NetworkError::MalformedResponse("unsupported HTTP version"));
    }
    let status_text = status_parts
        .next()
        .ok_or(NetworkError::MalformedResponse("missing status"))?;
    let status = status_text
        .parse::<u16>()
        .map_err(|_| NetworkError::InvalidStatus(status_text.to_owned()))?;
    let mut headers = BTreeMap::new();
    let mut set_cookie_headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(NetworkError::MalformedResponse("header has no colon"))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(NetworkError::MalformedResponse("header name is empty"));
        }
        let value = value.trim().to_owned();
        if name == "set-cookie" {
            set_cookie_headers.push(value);
            continue;
        }
        headers
            .entry(name)
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert(value);
    }

    let encoded_body = &bytes[header_end + 4..];
    let body = match headers.get("transfer-encoding").map(String::as_str) {
        Some(value) if value.eq_ignore_ascii_case("chunked") => {
            decode_chunked(encoded_body, limit)?
        }
        Some(value) if value.eq_ignore_ascii_case("identity") => encoded_body.to_vec(),
        Some(value) => return Err(NetworkError::UnsupportedTransferEncoding(value.to_owned())),
        None => {
            if let Some(value) = headers.get("content-length") {
                let length = value
                    .parse::<usize>()
                    .map_err(|_| NetworkError::InvalidContentLength(value.clone()))?;
                if length > limit {
                    return Err(NetworkError::ResponseTooLarge { limit });
                }
                if encoded_body.len() < length {
                    return Err(NetworkError::MalformedResponse(
                        "body is shorter than Content-Length",
                    ));
                }
                encoded_body[..length].to_vec()
            } else {
                encoded_body.to_vec()
            }
        }
    };
    if body.len() > limit {
        return Err(NetworkError::ResponseTooLarge { limit });
    }
    Ok(RawResponse {
        status,
        headers,
        set_cookie_headers,
        body,
    })
}

fn decode_chunked(mut bytes: &[u8], limit: usize) -> Result<Vec<u8>, NetworkError> {
    let mut body = Vec::new();
    loop {
        let line_end = bytes
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or(NetworkError::MalformedResponse(
                "chunk size has no terminator",
            ))?;
        let size_text = std::str::from_utf8(&bytes[..line_end])
            .map_err(|_| NetworkError::MalformedResponse("chunk size is not ASCII"))?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| NetworkError::MalformedResponse("chunk size is invalid"))?;
        bytes = &bytes[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let next_length = body
            .len()
            .checked_add(size)
            .ok_or(NetworkError::ResponseTooLarge { limit })?;
        if next_length > limit {
            return Err(NetworkError::ResponseTooLarge { limit });
        }
        if bytes.len() < size + 2 || &bytes[size..size + 2] != b"\r\n" {
            return Err(NetworkError::MalformedResponse("chunk body is truncated"));
        }
        body.extend_from_slice(&bytes[..size]);
        bytes = &bytes[size + 2..];
    }
}
