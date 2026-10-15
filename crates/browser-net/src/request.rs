use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use url::{Url, form_urlencoded};

use crate::NetworkError;

/// HTTP methods admitted by the initial browser document transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestMethod {
    Get,
    Post,
}

impl RequestMethod {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// A browser document request owned independently of any transport implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRequest {
    pub url: Url,
    pub method: RequestMethod,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    /// Serialized initiating origin for a cross-origin page request.
    pub origin: Option<String>,
}

impl BrowserRequest {
    pub fn parse(url: &str) -> Result<Self, url::ParseError> {
        Ok(Self {
            url: Url::parse(url)?,
            method: RequestMethod::Get,
            body: Vec::new(),
            content_type: None,
            origin: None,
        })
    }

    /// Encode successful form controls using the HTML URL-encoded representation.
    ///
    /// GET submissions extend the action query. POST submissions retain the action URL and
    /// carry the encoded fields as the request body. Other form methods are rejected explicitly.
    pub fn form<I, K, V>(url: &str, method: &str, fields: I) -> Result<Self, NetworkError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut url = Url::parse(url)?;
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        match method.to_ascii_lowercase().as_str() {
            "get" => {
                if !body.is_empty() {
                    let separator = if url.query().is_some_and(|query| !query.is_empty()) {
                        "&"
                    } else {
                        ""
                    };
                    let query = format!("{}{separator}{body}", url.query().unwrap_or_default());
                    url.set_query(Some(&query));
                }
                Ok(Self {
                    url,
                    method: RequestMethod::Get,
                    body: Vec::new(),
                    content_type: None,
                    origin: None,
                })
            }
            "post" => Ok(Self {
                url,
                method: RequestMethod::Post,
                body: body.into_bytes(),
                content_type: Some("application/x-www-form-urlencoded".to_owned()),
                origin: None,
            }),
            method => Err(NetworkError::UnsupportedMethod(method.to_owned())),
        }
    }

    /// Encode successful controls and browser-owned file bytes as multipart form data.
    pub fn multipart(
        url: &str,
        method: &str,
        fields: Vec<(String, String)>,
        files: Vec<(String, String, String, Vec<u8>)>,
    ) -> Result<Self, NetworkError> {
        if !method.eq_ignore_ascii_case("post") {
            return Err(NetworkError::UnsupportedMethod(method.to_owned()));
        }
        let boundary = multipart_boundary(&fields, &files);
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                    multipart_token(&name)
                )
                .as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        for (name, file_name, media_type, bytes) in files {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n", multipart_token(&name), multipart_token(&file_name), media_type).as_bytes());
            body.extend_from_slice(&bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Ok(Self {
            url: Url::parse(url)?,
            method: RequestMethod::Post,
            body,
            content_type: Some(format!("multipart/form-data; boundary={boundary}")),
            origin: None,
        })
    }

    pub(crate) fn redirected(&self, status: u16, url: Url) -> Self {
        let preserve_body = matches!(status, 307 | 308);
        Self {
            url,
            method: if preserve_body {
                self.method
            } else {
                RequestMethod::Get
            },
            body: if preserve_body {
                self.body.clone()
            } else {
                Vec::new()
            },
            content_type: preserve_body.then(|| self.content_type.clone()).flatten(),
            origin: self.origin.clone(),
        }
    }
}

fn multipart_token(value: &str) -> String {
    value
        .replace('\r', "%0D")
        .replace('\n', "%0A")
        .replace('\"', "%22")
}

fn multipart_boundary(
    fields: &[(String, String)],
    files: &[(String, String, String, Vec<u8>)],
) -> String {
    for suffix in 1_u32.. {
        let candidate = format!("----rwb-form-boundary-{suffix}");
        let bytes = candidate.as_bytes();
        let collides = fields.iter().any(|(_, value)| {
            value
                .as_bytes()
                .windows(bytes.len())
                .any(|part| part == bytes)
        }) || files
            .iter()
            .any(|(_, _, _, value)| value.windows(bytes.len()).any(|part| part == bytes));
        if !collides {
            return candidate;
        }
    }
    unreachable!("multipart boundary space is finite only after exhausting u32")
}

/// One redirect observed while resolving a document request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectHop {
    pub status: u16,
    pub from: Url,
    pub to: Url,
}

/// A fully buffered response from the bounded initial HTTP transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserResponse {
    pub url: Url,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub redirects: Vec<RedirectHop>,
}
