use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process, thread,
    time::{SystemTime, UNIX_EPOCH},
};

use browser_net::BrowserClient;

fn temporary_profile_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(std::env::temp_dir().join(format!("rwb-auth-profile-{}-{nonce}.redb", process::id())))
}

fn read_request(
    stream: &mut TcpStream,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err("client closed before completing request headers".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 16 * 1024 {
            return Err("fixture request headers exceeded 16 KiB".into());
        }
    }
    let head = std::str::from_utf8(&bytes)?;
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("fixture request had no target")?
        .to_owned();
    let cookie = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("cookie")
            .then(|| value.trim().to_owned())
    });
    Ok((target, cookie))
}

fn response(
    stream: &mut TcpStream,
    status: &str,
    headers: &[&str],
    body: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    write!(stream, "HTTP/1.1 {status}\r\n")?;
    for header in headers {
        write!(stream, "{header}\r\n")?;
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn expect_cookie(
    actual: Option<&str>,
    expected: Option<&str>,
    target: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if actual != expected {
        return Err(format!(
            "{target} received cookie {:?}; expected {:?}",
            actual, expected
        )
        .into());
    }
    Ok(())
}

fn serve(mut stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let (target, cookie) = read_request(&mut stream)?;
    match target.as_str() {
        "/login" => {
            expect_cookie(cookie.as_deref(), None, &target)?;
            response(
                &mut stream,
                "302 Found",
                &[
                    "Location: /app/home",
                    "Set-Cookie: session=alpha; Path=/; HttpOnly; Max-Age=600",
                    "Set-Cookie: scoped=inside; Path=/app",
                    "Set-Cookie: transport=tls-only; Path=/; Secure",
                    "Set-Cookie: rejected=foreign; Domain=example.invalid; Path=/",
                ],
                "",
            )?;
        }
        "/app/home" => {
            expect_cookie(
                cookie.as_deref(),
                Some("scoped=inside; session=alpha"),
                &target,
            )?;
            response(&mut stream, "200 OK", &[], "authenticated")?;
        }
        "/outside" => {
            expect_cookie(cookie.as_deref(), Some("session=alpha"), &target)?;
            response(&mut stream, "200 OK", &[], "shared-profile")?;
        }
        "/anonymous" => {
            expect_cookie(cookie.as_deref(), None, &target)?;
            response(&mut stream, "200 OK", &[], "isolated-profile")?;
        }
        "/expire" => {
            expect_cookie(cookie.as_deref(), Some("session=alpha"), &target)?;
            response(
                &mut stream,
                "302 Found",
                &[
                    "Location: /after-expiry",
                    "Set-Cookie: session=deleted; Path=/; Max-Age=0",
                ],
                "",
            )?;
        }
        "/after-expiry" => {
            expect_cookie(cookie.as_deref(), None, &target)?;
            response(&mut stream, "401 Unauthorized", &[], "expired")?;
        }
        other => return Err(format!("unexpected fixture target {other}").into()),
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<(), String> {
        for _ in 0..6 {
            let (stream, _) = listener.accept().map_err(|error| error.to_string())?;
            serve(stream).map_err(|error| error.to_string())?;
        }
        Ok(())
    });

    let profile_path = temporary_profile_path()?;
    let profile = BrowserClient::open_profile(&profile_path)?;
    let login = profile.fetch_url(&format!("http://{address}/login"))?;
    if login.status != 200 || login.body != b"authenticated" || login.redirects.len() != 1 {
        return Err("login redirect did not commit the authenticated response".into());
    }

    drop(profile);
    let reopened_profile = BrowserClient::open_profile(&profile_path)?;
    let shared = reopened_profile.fetch_url(&format!("http://{address}/outside"))?;
    if shared.status != 200 || shared.body != b"shared-profile" {
        return Err("reopened profile did not restore the persistent authenticated session".into());
    }

    let other_profile = BrowserClient::default();
    let isolated = other_profile.fetch_url(&format!("http://{address}/anonymous"))?;
    if isolated.status != 200 || isolated.body != b"isolated-profile" {
        return Err("fresh profile client did not remain isolated".into());
    }

    let expired = reopened_profile.fetch_url(&format!("http://{address}/expire"))?;
    if expired.status != 401 || expired.body != b"expired" || expired.redirects.len() != 1 {
        return Err("expired session did not reach the unauthenticated response".into());
    }

    server
        .join()
        .map_err(|_| "authentication fixture server panicked")?
        .map_err(|error| format!("authentication fixture failed: {error}"))?;
    println!(
        "auth-spike: PASS requests=6 login-redirect=authenticated profile-clone=shared fresh-profile=isolated path=filtered secure=http-rejected expiry=revoked duplicate-set-cookie=preserved"
    );
    Ok(())
}
