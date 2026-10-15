use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
};

use browser_net::{BrowserClient, BrowserRequest, NetworkError};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedRequest {
    method: String,
    target: String,
    user_agent: String,
    body: Vec<u8>,
}

fn expected_requests() -> Vec<ObservedRequest> {
    [
        ("GET", "/a/start", ""),
        ("GET", "/final?from=redirect", ""),
        ("GET", "/chunked", ""),
        ("GET", "/loop", ""),
        ("GET", "/loop", ""),
        ("GET", "/loop", ""),
        ("GET", "/large", ""),
        ("GET", "/form-get?existing=1&name=Ada+%CE%BB&empty=", ""),
        ("POST", "/form-post", "name=Ada+%CE%BB&empty="),
        ("POST", "/form-post-redirect", "name=Ada+%CE%BB&empty="),
        ("GET", "/form-post-result", ""),
        ("POST", "/form-preserve", "name=Ada+%CE%BB&empty="),
        ("POST", "/form-preserved", "name=Ada+%CE%BB&empty="),
    ]
    .into_iter()
    .map(|(method, target, body)| ObservedRequest {
        method: method.to_owned(),
        target: target.to_owned(),
        user_agent: "rwb/0.1.0".to_owned(),
        body: body.as_bytes().to_vec(),
    })
    .collect()
}

fn read_request(
    stream: &mut TcpStream,
) -> Result<ObservedRequest, Box<dyn std::error::Error + Send + Sync>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 512];
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("client closed before completing HTTP headers".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > 16 * 1024 {
            return Err("fixture request headers exceeded 16 KiB".into());
        }
    };

    let head = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = head.lines();
    let mut parts = lines
        .next()
        .ok_or("fixture request had no request line")?
        .split_whitespace();
    let method = parts
        .next()
        .ok_or("fixture request had no method")?
        .to_owned();
    let target = parts
        .next()
        .ok_or("fixture request had no target")?
        .to_owned();
    if parts.next() != Some("HTTP/1.1") {
        return Err("fixture expected HTTP/1.1".into());
    }
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .transpose()?
        .unwrap_or(0);
    let user_agent = headers
        .iter()
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("user-agent")
                .then(|| value.trim().to_owned())
        })
        .ok_or("fixture request had no User-Agent header")?;
    if content_length > 16 * 1024 {
        return Err("fixture request body exceeded 16 KiB".into());
    }
    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("client closed before completing HTTP body".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }

    Ok(ObservedRequest {
        method,
        target,
        user_agent,
        body: bytes[body_start..body_start + content_length].to_vec(),
    })
}

fn serve_connection(
    mut stream: TcpStream,
) -> Result<ObservedRequest, Box<dyn std::error::Error + Send + Sync>> {
    let request = read_request(&mut stream)?;
    let response = match request.target.as_str() {
        "/a/start" => {
            "HTTP/1.1 302 Found\r\nLocation: ../final?from=redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned()
        }
        "/final?from=redirect" => {
            let body = "<!doctype html><p id='result'>redirected λ</p>";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        "/chunked" => {
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n6\r\n λ-ok\r\n0\r\n\r\n"
                .to_owned()
        }
        "/loop" => {
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned()
        }
        "/large" => {
            "HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n".to_owned()
        }
        "/form-post-redirect" => {
            "HTTP/1.1 302 Found\r\nLocation: /form-post-result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned()
        }
        "/form-preserve" => {
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: /form-preserved\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned()
        }
        target if target.starts_with("/form-") => {
            let body = "form-ok";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
    };
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(request)
}

fn form_request(
    address: std::net::SocketAddr,
    path: &str,
    method: &str,
) -> Result<BrowserRequest, NetworkError> {
    BrowserRequest::form(
        &format!("http://{address}{path}"),
        method,
        [("name", "Ada λ"), ("empty", "")],
    )
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let expected = expected_requests();
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let (sender, receiver) = mpsc::channel();
    let request_count = expected.len();
    let server = thread::spawn(move || {
        for connection in listener.incoming().take(request_count) {
            let result = connection
                .map_err(|error| error.into())
                .and_then(serve_connection);
            sender
                .send(result)
                .map_err(|_| "fixture result receiver closed")?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });

    let client = BrowserClient::default();
    let response = client.fetch_url(&format!("http://{address}/a/start"))?;
    if response.status != 200
        || response.url.as_str() != format!("http://{address}/final?from=redirect")
        || response.redirects.len() != 1
        || response.redirects[0].status != 302
        || response.redirects[0].from.path() != "/a/start"
        || response.redirects[0].to.path() != "/final"
        || response.body != "<!doctype html><p id='result'>redirected λ</p>".as_bytes()
    {
        return Err(format!("relative redirect result was incorrect: {response:?}").into());
    }

    let chunked = client.fetch_url(&format!("http://{address}/chunked"))?;
    if chunked.status != 200 || chunked.body != "hello λ-ok".as_bytes() {
        return Err(format!("chunked response was decoded incorrectly: {chunked:?}").into());
    }

    let bounded_redirects = BrowserClient::new(2, 1024);
    if !matches!(
        bounded_redirects.fetch_url(&format!("http://{address}/loop")),
        Err(NetworkError::TooManyRedirects { limit: 2 })
    ) {
        return Err("redirect loop did not fail at the configured bound".into());
    }
    if !matches!(
        bounded_redirects.fetch_url(&format!("http://{address}/large")),
        Err(NetworkError::ResponseTooLarge { limit: 1024 })
    ) {
        return Err("oversized response did not fail at the configured bound".into());
    }
    if !matches!(
        client.fetch_url(&format!("http://user:secret@{address}/private")),
        Err(NetworkError::CredentialsInUrl)
    ) {
        return Err("URL credentials were not rejected before network dispatch".into());
    }

    let get_form = client.fetch(form_request(address, "/form-get?existing=1", "GET")?)?;
    if get_form.status != 200 || get_form.body != b"form-ok" {
        return Err("GET form did not complete".into());
    }
    let post_form = client.fetch(form_request(address, "/form-post", "post")?)?;
    if post_form.status != 200 || post_form.body != b"form-ok" {
        return Err("POST form did not complete".into());
    }
    let converted = client.fetch(form_request(address, "/form-post-redirect", "POST")?)?;
    if converted.redirects.len() != 1 || converted.redirects[0].status != 302 {
        return Err("POST 302 redirect was not retained in response history".into());
    }
    let preserved = client.fetch(form_request(address, "/form-preserve", "POST")?)?;
    if preserved.redirects.len() != 1 || preserved.redirects[0].status != 307 {
        return Err("POST 307 redirect was not retained in response history".into());
    }
    if !matches!(
        form_request(address, "/form", "PUT"),
        Err(NetworkError::UnsupportedMethod(method)) if method == "put"
    ) {
        return Err("unsupported form method was not rejected before dispatch".into());
    }

    for expected_request in expected {
        let actual = receiver.recv()??;
        if actual != expected_request {
            return Err(
                format!("fixture expected request {expected_request:?}, got {actual:?}").into(),
            );
        }
    }
    server
        .join()
        .map_err(|_| "fixture server thread panicked")??;

    println!(
        "http-probe: PASS requests=13 user-agent=rwb/0.1.0 relative-redirect=1 chunked=utf8 redirect-limit=2 response-limit=1024 forms=get,post encoding=urlencoded redirects=302-to-get,307-preserve unsupported-method=rejected credentials=rejected"
    );
    Ok(())
}
