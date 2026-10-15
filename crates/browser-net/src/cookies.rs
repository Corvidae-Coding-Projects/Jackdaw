use std::time::{Duration, SystemTime, UNIX_EPOCH};

use browser_store::CookieRecord;
use url::Url;

/// Profile-scoped HTTP cookie state used by one browser network client.
///
/// The jar deliberately exposes no host paths or transport handles. Invalid `Set-Cookie`
/// records are ignored, matching browser behavior, while accepted records are selected by
/// scheme, host, path, and expiry for each outbound request.
#[derive(Debug, Default)]
pub(crate) struct CookieJar {
    cookies: Vec<StoredCookie>,
    next_creation: u64,
}

#[derive(Debug, Clone)]
struct StoredCookie {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    http_only: bool,
    expires_at: Option<SystemTime>,
    creation: u64,
}

impl CookieJar {
    pub(crate) fn from_persistent(records: Vec<CookieRecord>) -> Self {
        let now = SystemTime::now();
        let mut cookies = records
            .into_iter()
            .filter_map(|record| {
                let expires_at =
                    UNIX_EPOCH.checked_add(Duration::from_secs(record.expires_unix_seconds))?;
                (expires_at > now).then_some(StoredCookie {
                    name: record.name,
                    value: record.value,
                    domain: record.domain,
                    host_only: record.host_only,
                    path: record.path,
                    secure: record.secure,
                    http_only: record.http_only,
                    expires_at: Some(expires_at),
                    creation: record.creation,
                })
            })
            .collect::<Vec<_>>();
        cookies.sort_by_key(|cookie| cookie.creation);
        let next_creation = cookies
            .iter()
            .map(|cookie| cookie.creation)
            .max()
            .map_or(0, |creation| creation.saturating_add(1));
        Self {
            cookies,
            next_creation,
        }
    }

    pub(crate) fn persistent_records(&mut self) -> Vec<CookieRecord> {
        let now = SystemTime::now();
        self.cookies
            .retain(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now));
        self.cookies
            .iter()
            .filter_map(|cookie| {
                let expires_at = cookie.expires_at?;
                let expires_unix_seconds = expires_at.duration_since(UNIX_EPOCH).ok()?.as_secs();
                Some(CookieRecord {
                    name: cookie.name.clone(),
                    value: cookie.value.clone(),
                    domain: cookie.domain.clone(),
                    host_only: cookie.host_only,
                    path: cookie.path.clone(),
                    secure: cookie.secure,
                    http_only: cookie.http_only,
                    expires_unix_seconds,
                    creation: cookie.creation,
                })
            })
            .collect()
    }

    pub(crate) fn store_response_cookie(&mut self, response_url: &Url, header: &str) {
        self.store_cookie(response_url, header, true);
    }

    pub(crate) fn store_document_cookie(&mut self, document_url: &Url, header: &str) {
        self.store_cookie(document_url, header, false);
    }

    fn store_cookie(&mut self, response_url: &Url, header: &str, from_http: bool) {
        let Some(host) = response_url.host_str().map(str::to_ascii_lowercase) else {
            return;
        };
        let mut parts = header.split(';');
        let Some((name, value)) = parts.next().and_then(|pair| pair.trim().split_once('=')) else {
            return;
        };
        let name = name.trim();
        let value = value.trim();
        if !valid_cookie_name(name) || !valid_cookie_value(value) {
            return;
        }

        let mut domain = host.clone();
        let mut host_only = true;
        let mut path = default_path(response_url.path());
        let mut secure = false;
        let mut http_only = false;
        let mut max_age = None;

        for attribute in parts {
            let attribute = attribute.trim();
            let (raw_name, raw_value) = attribute.split_once('=').unwrap_or((attribute, ""));
            match raw_name.trim().to_ascii_lowercase().as_str() {
                "domain" => {
                    let candidate = raw_value
                        .trim()
                        .trim_start_matches('.')
                        .to_ascii_lowercase();
                    if candidate.is_empty() || !domain_matches(&host, &candidate) {
                        return;
                    }
                    domain = candidate;
                    host_only = false;
                }
                "path" if raw_value.trim().starts_with('/') => {
                    path = raw_value.trim().to_owned();
                }
                "max-age" => {
                    max_age = raw_value.trim().parse::<i64>().ok();
                }
                "secure" => secure = true,
                "httponly" if from_http => http_only = true,
                _ => {}
            }
        }
        if !from_http && secure && response_url.scheme() != "https" {
            return;
        }

        let identity_matches = |cookie: &StoredCookie| {
            cookie.name == name && cookie.domain == domain && cookie.path == path
        };
        if !from_http
            && self
                .cookies
                .iter()
                .any(|cookie| identity_matches(cookie) && cookie.http_only)
        {
            return;
        }
        if max_age.is_some_and(|seconds| seconds <= 0) {
            self.cookies.retain(|cookie| !identity_matches(cookie));
            return;
        }
        let expires_at = max_age.and_then(|seconds| {
            u64::try_from(seconds).ok().and_then(|seconds| {
                SystemTime::now().checked_add(std::time::Duration::from_secs(seconds))
            })
        });

        let creation = self
            .cookies
            .iter()
            .find(|cookie| identity_matches(cookie))
            .map_or_else(
                || {
                    let creation = self.next_creation;
                    self.next_creation = self.next_creation.saturating_add(1);
                    creation
                },
                |cookie| cookie.creation,
            );
        self.cookies.retain(|cookie| !identity_matches(cookie));
        self.cookies.push(StoredCookie {
            name: name.to_owned(),
            value: value.to_owned(),
            domain,
            host_only,
            path,
            secure,
            http_only,
            expires_at,
            creation,
        });
    }

    pub(crate) fn request_header(&mut self, request_url: &Url) -> Option<String> {
        self.cookie_header(request_url, true)
    }

    pub(crate) fn document_header(&mut self, document_url: &Url) -> Option<String> {
        self.cookie_header(document_url, false)
    }

    fn cookie_header(&mut self, request_url: &Url, include_http_only: bool) -> Option<String> {
        let now = SystemTime::now();
        self.cookies
            .retain(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now));
        let host = request_url.host_str()?.to_ascii_lowercase();
        let path = if request_url.path().is_empty() {
            "/"
        } else {
            request_url.path()
        };
        let secure_transport = request_url.scheme() == "https";
        let mut selected = self
            .cookies
            .iter()
            .filter(|cookie| {
                (include_http_only || !cookie.http_only)
                    && (!cookie.secure || secure_transport)
                    && (if cookie.host_only {
                        host == cookie.domain
                    } else {
                        domain_matches(&host, &cookie.domain)
                    })
                    && path_matches(path, &cookie.path)
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|cookie| (std::cmp::Reverse(cookie.path.len()), cookie.creation));
        (!selected.is_empty()).then(|| {
            selected
                .into_iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
}

fn valid_cookie_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte > 0x20
                && byte < 0x7f
                && !matches!(
                    byte,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'\"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

fn valid_cookie_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| (0x21..0x7f).contains(&byte) && !matches!(byte, b',' | b';' | b'\\' | b'\"'))
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

fn default_path(request_path: &str) -> String {
    if !request_path.starts_with('/') || request_path == "/" {
        return "/".to_owned();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => request_path[..index].to_owned(),
    }
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}
