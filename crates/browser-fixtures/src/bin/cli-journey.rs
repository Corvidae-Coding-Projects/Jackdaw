use std::{
    fs,
    io::{BufRead, BufReader, Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use browser_protocol::{
    Command as ProtocolCommand, CommandReply, CommandRequest, CommandResult, DocumentReadyState,
    EffectState, ElementRef, ErrorCode, EventCursor, EventKind, FramePixels, ObservedItem,
    OperationStatus, PROTOCOL_VERSION, PixelFormat, ProfileId, RequestId, TabCommand,
    TransferStatus,
};
use sha2::{Digest, Sha256};

const DOWNLOAD_BYTES: &[u8] = b"\x00rwb-download\xff\nsecond-line";
const UPLOAD_BYTES: &[u8] = b"\x00rwb-upload\xff\nsecond-line";
// A deterministic 1x1 opaque red PNG used to prove that the packaged browser
// fetches, decodes, and paints image resources through its normal frame path.
const FIXTURE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x72, 0x9c, 0x52, 0x67, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const DEFAULT_SEED: u64 = 0x0c11_e2e5;
const DEFAULT_CASES: usize = 16;
const CASE_TEXTS: [&str; 6] = [
    "",
    "plain ASCII",
    "line one\nline two",
    "combining e\u{301}",
    "emoji 🧭",
    "CJK 第二",
];

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

struct DaemonGuard {
    child: Child,
    directory: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn temporary_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rwb-cli-journey-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory)?;
    Ok(directory)
}

struct FixtureRequest {
    method: String,
    target: String,
    cookie: Option<String>,
    content_type: Option<String>,
    user_agent: Option<String>,
    body: Vec<u8>,
}

struct UploadObservation {
    filename: String,
    bytes: Vec<u8>,
}

struct FormPostObservation {
    method: String,
    target: String,
    content_type: Option<String>,
    user_agent: Option<String>,
    body: Vec<u8>,
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

fn parse_upload(
    content_type: &str,
    body: &[u8],
) -> Result<UploadObservation, Box<dyn std::error::Error>> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|parameter| parameter.strip_prefix("boundary="))
        .map(|value| value.trim_matches('\"'))
        .filter(|value| !value.is_empty())
        .ok_or("multipart upload did not declare a boundary")?;
    let filename_marker = b"filename=\"";
    let filename_start = find_bytes(body, filename_marker)
        .map(|offset| offset + filename_marker.len())
        .ok_or("multipart upload did not contain a filename")?;
    let filename_end = find_bytes(&body[filename_start..], b"\"")
        .map(|offset| filename_start + offset)
        .ok_or("multipart upload filename was not terminated")?;
    let filename = std::str::from_utf8(&body[filename_start..filename_end])?.to_owned();
    let payload_start = find_bytes(&body[filename_end..], b"\r\n\r\n")
        .map(|offset| filename_end + offset + 4)
        .ok_or("multipart upload file headers were not terminated")?;
    let payload_end_marker = format!("\r\n--{boundary}");
    let payload_end = find_bytes(&body[payload_start..], payload_end_marker.as_bytes())
        .map(|offset| payload_start + offset)
        .ok_or("multipart upload file payload was not terminated")?;
    Ok(UploadObservation {
        filename,
        bytes: body[payload_start..payload_end].to_vec(),
    })
}

fn read_request(stream: &mut TcpStream) -> Result<FixtureRequest, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 512];
    let header_end = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("fixture client closed before completing request headers".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 16 * 1024 {
            return Err("fixture request headers exceeded 16 KiB".into());
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = headers.lines();
    let mut parts = lines
        .next()
        .ok_or("fixture request line was missing")?
        .split_whitespace();
    let method = parts
        .next()
        .ok_or("fixture request method was missing")?
        .to_owned();
    let target = parts
        .next()
        .ok_or("fixture request target was missing")?
        .to_owned();
    let mut cookie = None;
    let mut content_type = None;
    let mut user_agent = None;
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("cookie") {
            cookie = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("user-agent") {
            user_agent = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse()?;
        }
    }
    if content_length > 1024 * 1024 {
        return Err("fixture request body exceeded 1 MiB".into());
    }
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("fixture client closed before completing request body".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(FixtureRequest {
        method,
        target,
        cookie,
        content_type,
        user_agent,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn serve_fixture(
    mut stream: TcpStream,
    upload_sender: &mpsc::Sender<UploadObservation>,
    form_post_sender: &mpsc::Sender<FormPostObservation>,
) -> Result<(), Box<dyn std::error::Error>> {
    let FixtureRequest {
        method,
        target,
        cookie,
        content_type,
        user_agent,
        body: request_body,
    } = read_request(&mut stream)?;
    if matches!(target.as_str(), "/posted" | "/gap-submitted") && method == "POST" {
        let response_text = if target == "/gap-submitted" {
            "Clicked submit accepted"
        } else {
            "Posted Ada λ via send"
        };
        form_post_sender
            .send(FormPostObservation {
                method,
                target,
                content_type,
                user_agent,
                body: request_body,
            })
            .map_err(|_| std::io::Error::other("form POST observation receiver disconnected"))?;
        let body = format!(
            "<!doctype html><html><body><main id='content'>{response_text}</main></body></html>"
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        stream.flush()?;
        return Ok(());
    }
    if target == "/uploaded" && method == "POST" {
        let content_type = content_type
            .as_deref()
            .ok_or("upload request omitted Content-Type")?;
        let observation = parse_upload(content_type, &request_body)?;
        upload_sender
            .send(observation)
            .map_err(|_| std::io::Error::other("upload observation receiver disconnected"))?;
        let body =
            "<!doctype html><html><body><main id='content'>Upload accepted</main></body></html>";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        stream.flush()?;
        return Ok(());
    }
    if target == "/go" {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: /beta\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        stream.flush()?;
        return Ok(());
    }
    if target == "/download" {
        let authenticated = cookie.as_deref().is_some_and(|header| {
            header
                .split(';')
                .any(|part| part.trim() == "rwb_session=work")
        });
        if !authenticated {
            write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )?;
        } else {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=fixture.bin\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                DOWNLOAD_BYTES.len()
            )?;
            stream.write_all(DOWNLOAD_BYTES)?;
        }
        stream.flush()?;
        return Ok(());
    }
    if matches!(target.as_str(), "/fixture.png" | "/responsive-wide.png") {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            FIXTURE_PNG.len()
        )?;
        stream.write_all(FIXTURE_PNG)?;
        stream.flush()?;
        return Ok(());
    }
    if target == "/slow-navigation" {
        thread::sleep(Duration::from_secs(2));
        let body = "<!doctype html><html><body><main id='content'>Slow navigation committed unexpectedly</main></body></html>";
        if write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .is_ok()
        {
            let _ = stream.flush();
        }
        return Ok(());
    }
    let (text, response_headers) = match target.as_str() {
        "/alpha" => (
            "Alpha λ navigation<input id='agent-toggle' type='checkbox' aria-label='Agent toggle'><a id='beta-link' href='/go' aria-label='Follow beta'></a>"
                .to_owned(),
            "",
        ),
        "/timer" => (
            "<span id='timer-status'>Timer pending</span><script>setTimeout(function(){document.getElementById('timer-status').textContent='Timer fired';},25);</script>"
                .to_owned(),
            "",
        ),
        "/fetch-page" => (
            "<span id='fetch-status'>Fetch pending</span><script>fetch('/fetch-data').then(function(response){return response.text();}).then(function(text){document.getElementById('fetch-status').textContent=text;}).catch(function(){document.getElementById('fetch-status').textContent='Fetch failed';});</script>"
                .to_owned(),
            "",
        ),
        "/fetch-data" => ("Fetched λ payload".to_owned(), ""),
        _ if target.starts_with("/cors-page?origin=") => {
            let cross_origin = target
                .strip_prefix("/cors-page?origin=")
                .ok_or("CORS fixture origin was missing")?;
            (
                format!(
                    "<span id='cors-allowed-status'>CORS allowed pending</span><span id='cors-denied-status'>CORS denied pending</span><script>fetch('{cross_origin}/cors-allowed').then(function(response){{return response.text();}}).then(function(text){{document.getElementById('cors-allowed-status').textContent=text;}}).catch(function(){{document.getElementById('cors-allowed-status').textContent='CORS allowed failed';}});fetch('{cross_origin}/cors-denied').then(function(response){{return response.text();}}).then(function(text){{document.getElementById('cors-denied-status').textContent='CORS leaked: '+text;}}).catch(function(){{document.getElementById('cors-denied-status').textContent='CORS denied';}});</script>"
                ),
                "",
            )
        }
        "/cors-allowed" => (
            "CORS allowed λ payload".to_owned(),
            "Access-Control-Allow-Origin: *\r\n",
        ),
        "/cors-denied" => ("CORS secret body".to_owned(), ""),
        "/external-script-page" => (
            "<span id='external-script-status'>External script pending</span><script src='/external-script.js'></script>"
                .to_owned(),
            "",
        ),
        "/external-script.js" => (
            "document.getElementById('external-script-status').textContent='External script loaded λ';"
                .to_owned(),
            "Content-Type: application/javascript; charset=utf-8\r\n",
        ),
        "/selector-page" => (
            "<span id='selector-status'>Selector pending</span><script>const match=document.querySelector('#selector-status');const missing=document.querySelector('#missing-selector');const matches=document.querySelectorAll('#selector-status');match.textContent=missing===null&&matches.length===1&&matches[0]===match?'Selector matched λ':'Selector contract failed';</script>"
                .to_owned(),
            "",
        ),
        "/console-page" => (
            "<script>for(let i=1;i<=260;i++){if(i===260){console.warn('tail λ',{ok:true});}else{console.log('entry-'+i);}}</script>"
                .to_owned(),
            "",
        ),
        "/render-page" => (
            "<main style='margin:0;width:800px;height:600px;background:rgb(17,34,51)'><div style='width:64px;height:48px;background:rgb(204,85,17)'></div><img src='/fixture.png' width='40' height='30' alt='Decoded fixture image'><p style='margin:0;color:rgb(238,221,68);font-size:32px'>Glyph λ</p></main>"
                .to_owned(),
            "",
        ),
        "/responsive-image-page" => (
            "<style>html,body{margin:0;padding:0}</style><img src='/responsive-fallback.png' srcset='/responsive-narrow.png 320w, /responsive-wide.png 800w' width='40' height='30' alt='Responsive fixture image'>"
                .to_owned(),
            "",
        ),
        "/responsive-render" => (
            "<style>html,body{margin:0;padding:0}</style><main style='width:100%;height:200px;background:rgb(17,34,51)'><div style='width:50%;height:100px;background:rgb(204,85,17)'></div><div style='width:120px;height:60px;border:4px solid rgb(51,204,102);background:transparent'></div></main>"
                .to_owned(),
            "",
        ),
        "/control-label-render" => (
            "<style>html,body{margin:0;padding:0;background:white}</style><input type='submit' value='Visible control label' aria-label='Visible control label' style='display:block;width:240px;height:80px;margin:0;padding:0;border:0;background:rgb(204,221,238);color:rgb(17,34,51);font-size:20px'>"
                .to_owned(),
            "",
        ),
        "/coordinate-click" => (
            "<style>html,body,#content{margin:0;padding:0}</style><input id='agent-toggle' type='checkbox' aria-label='Agent toggle' style='display:block;width:80px;height:80px;margin:0'>"
                .to_owned(),
            "",
        ),
        "/multiline-edit" => (
            "<textarea id='multiline-editor' aria-label='Multiline editor'></textarea>".to_owned(),
            "",
        ),
        "/beta" => (
            "<div style='width:1200px;height:900px'>Beta 第二 page</div>".to_owned(),
            "",
        ),
        "/form" => (
            "<form id='agent-form' action='/wrong' method='post'><input id='agent-name' name='name' aria-label='Agent name' value='' required><button id='agent-submit' type='submit' name='intent' value='save' formaction='/submitted?fixed=1' formmethod='get' aria-label='Save form'>Save</button></form>"
                .to_owned(),
            "",
        ),
        "/post-form" => (
            "<form id='agent-post-form' action='/posted' method='post'><input id='agent-post-name' name='name' aria-label='POST name' value='' required><button id='agent-post-submit' type='submit' name='intent' value='send' aria-label='Send POST'>Send</button></form>"
                .to_owned(),
            "",
        ),
        "/upload" => (
            "<form id='upload-form' action='/uploaded' method='post' enctype='multipart/form-data'><input id='upload-file' name='asset' type='file' aria-label='Upload asset'><button id='upload-submit' type='submit' name='intent' value='send' aria-label='Send upload'>Send</button></form>"
                .to_owned(),
            "",
        ),
        "/submitted?fixed=1&name=Ada+%CE%BB&intent=save" if method == "GET" => {
            ("Submitted Ada λ via save".to_owned(), "")
        }
        "/login" => (
            "Profile authenticated".to_owned(),
            "Set-Cookie: rwb_session=work; Path=/; Max-Age=3600; HttpOnly\r\n",
        ),
        "/who" => {
            let authenticated = cookie.as_deref().is_some_and(|header| {
                header
                    .split(';')
                    .any(|part| part.trim() == "rwb_session=work")
            });
            (
                if authenticated {
                    "Session authenticated"
                } else {
                    "Session anonymous"
                }
                .to_owned(),
                "",
            )
        }
        "/cookie-view" => (
            "<span id='cookie-status'>Cookie pending</span><script>document.getElementById('cookie-status').textContent=document.cookie===''?'Cookie hidden':'Cookie exposed';</script>"
                .to_owned(),
            "",
        ),
        "/storage-write" => (
            "<span id='storage-status'>Storage pending</span><script>localStorage.setItem('journey-key','durable λ');document.getElementById('storage-status').textContent='Storage '+localStorage.getItem('journey-key');</script>"
                .to_owned(),
            "",
        ),
        "/storage-read" => (
            "<span id='storage-status'>Storage pending</span><script>const value=localStorage.getItem('journey-key');document.getElementById('storage-status').textContent=value===null?'Storage empty':'Storage '+value;</script>"
                .to_owned(),
            "",
        ),
        "/session-write" => (
            "<span id='session-status'>Session pending</span><script>sessionStorage.setItem('journey-key','tab λ');document.getElementById('session-status').textContent='Session '+sessionStorage.getItem('journey-key');</script>"
                .to_owned(),
            "",
        ),
        "/session-read" => (
            "<span id='session-status'>Session pending</span><script>const value=sessionStorage.getItem('journey-key');document.getElementById('session-status').textContent=value===null?'Session empty':'Session '+value;</script>"
                .to_owned(),
            "",
        ),
        _ if target.starts_with("/case/") => {
            let parts = target.split('/').collect::<Vec<_>>();
            let (case_index, variant, initially_checked) = match parts.as_slice() {
                ["", "case", case_index, variant, initially_checked] => (
                    case_index.parse::<usize>()?,
                    variant.parse::<usize>()?,
                    *initially_checked == "checked",
                ),
                _ => return Err("malformed generated fixture path".into()),
            };
            let case_text = CASE_TEXTS
                .get(variant)
                .ok_or("generated fixture variant was out of range")?;
            let checked = if initially_checked { " checked" } else { "" };
            (
                format!(
                    "Case {case_index}: {case_text}<input id='agent-toggle' type='checkbox' aria-label='Agent toggle'{checked}>"
                ),
                "",
            )
        }
        _ => {
            let body = "not found";
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()?;
            return Ok(());
        }
    };
    let body = if matches!(
        target.as_str(),
        "/fetch-data" | "/cors-allowed" | "/cors-denied" | "/external-script.js"
    ) {
        text
    } else if target == "/beta" {
        format!(
            "<!doctype html><html><head><title>Beta 第二</title></head><body><main id='content' style='width:1200px;height:1200px'>{text}</main></body></html>"
        )
    } else {
        format!("<!doctype html><html><body><main id='content'>{text}</main></body></html>")
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n{response_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn run_rwb(
    rwb: &Path,
    socket: &Path,
    arguments: &[&str],
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    let output = Command::new(rwb)
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(arguments)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "rwb {:?} failed with {:?}: stdout={} stderr={}",
            arguments,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    if !output.stderr.is_empty() {
        return Err(format!(
            "rwb {:?} emitted unexpected stderr: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let reply: CommandReply = serde_json::from_slice(&output.stdout)?;
    if !reply.success {
        return Err(format!("rwb {:?} returned failure: {:?}", arguments, reply.error).into());
    }
    Ok(reply)
}

fn run_rwb_failure(
    rwb: &Path,
    socket: &Path,
    arguments: &[&str],
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    run_rwb_failure_with_status(rwb, socket, arguments, 1)
}

fn run_rwb_failure_with_status(
    rwb: &Path,
    socket: &Path,
    arguments: &[&str],
    expected_status: i32,
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    let output = Command::new(rwb)
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(arguments)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()?;
    if output.status.code() != Some(expected_status) {
        return Err(format!(
            "rwb {:?} returned unexpected failure status {:?}: stdout={} stderr={}",
            arguments,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    if !output.stderr.is_empty() {
        return Err(format!(
            "rwb {:?} emitted unexpected stderr: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let reply: CommandReply = serde_json::from_slice(&output.stdout)?;
    if reply.success {
        return Err(format!("rwb {:?} unexpectedly succeeded", arguments).into());
    }
    Ok(reply)
}

fn raw_daemon_reply(
    socket: &Path,
    request: &str,
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if response.is_empty() {
        return Err("browserd closed without replying to raw NDJSON".into());
    }
    Ok(serde_json::from_str(response.trim_end())?)
}

fn stdio_daemon_reply(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: &str,
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    writer.write_all(request.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.is_empty() {
        return Err("browserd serve --stdio closed without replying to NDJSON".into());
    }
    Ok(serde_json::from_str(response.trim_end())?)
}

fn assert_stdio_transport(browserd: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let directory = temporary_directory()?;
    let profile_root = directory.join("profiles");
    let mut child = Command::new(browserd)
        .args(["serve", "--stdio", "--profile-root"])
        .arg(&profile_root)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut writer = child.stdin.take().ok_or("stdio daemon omitted stdin")?;
    let stdout = child.stdout.take().ok_or("stdio daemon omitted stdout")?;
    let mut reader = BufReader::new(stdout);
    let profile_id = ProfileId::parse("p_stdio")?;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let status_request = CommandRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: RequestId::parse("r_stdio_status")?,
            profile_id: profile_id.clone(),
            tab_id: None,
            command: ProtocolCommand::Status,
            timeout_ms: 1_000,
        };
        let status_reply = stdio_daemon_reply(
            &mut writer,
            &mut reader,
            &serde_json::to_string(&status_request)?,
        )?;
        let service_instance = match status_reply.result.as_ref() {
            Some(CommandResult::Status(status)) if status.running => {
                status.service_instance_id.clone()
            }
            _ => return Err("stdio status did not report a running service".into()),
        };

        let malformed = stdio_daemon_reply(&mut writer, &mut reader, "{broken")?;
        if malformed.request_id.as_str() != "r_transport"
            || malformed
                .error
                .as_ref()
                .map(|error| (error.code, error.effect))
                != Some((ErrorCode::InvalidRequest, EffectState::None))
        {
            return Err(
                "stdio malformed NDJSON did not return canonical INVALID_REQUEST/none".into(),
            );
        }

        let recovered_status_request = CommandRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: RequestId::parse("r_stdio_recovered")?,
            profile_id: profile_id.clone(),
            tab_id: None,
            command: ProtocolCommand::Status,
            timeout_ms: 1_000,
        };
        let recovered_status = stdio_daemon_reply(
            &mut writer,
            &mut reader,
            &serde_json::to_string(&recovered_status_request)?,
        )?;
        match recovered_status.result.as_ref() {
            Some(CommandResult::Status(status))
                if status.running && status.service_instance_id == service_instance => {}
            _ => return Err("stdio daemon did not recover with the same service instance".into()),
        }

        let new_tab_request = CommandRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: RequestId::parse("r_stdio_tab_new")?,
            profile_id: profile_id.clone(),
            tab_id: None,
            command: ProtocolCommand::Tab(TabCommand::New { url: None }),
            timeout_ms: 1_000,
        };
        let new_tab_reply = stdio_daemon_reply(
            &mut writer,
            &mut reader,
            &serde_json::to_string(&new_tab_request)?,
        )?;
        let tab_id = new_tab_reply
            .target
            .as_ref()
            .and_then(|target| target.tab_id.clone())
            .ok_or("stdio tab creation did not return a tab ID")?;

        let list_tabs_request = CommandRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id: RequestId::parse("r_stdio_tab_list")?,
            profile_id,
            tab_id: None,
            command: ProtocolCommand::Tab(TabCommand::List),
            timeout_ms: 1_000,
        };
        let listed_tabs = stdio_daemon_reply(
            &mut writer,
            &mut reader,
            &serde_json::to_string(&list_tabs_request)?,
        )?;
        let Some(CommandResult::Tabs(tabs)) = listed_tabs.result.as_ref() else {
            return Err("stdio tab list returned the wrong result kind".into());
        };
        if tabs.len() != 1 || tabs[0].tab_id != tab_id || tabs[0].url != "about:blank" {
            return Err("stdio tab list did not preserve the created tab state".into());
        }
        Ok(())
    })();

    drop(writer);
    let mut trailing_stdout = String::new();
    let trailing_result = reader.read_to_string(&mut trailing_stdout);
    let wait_result = child.wait();
    let mut stderr = String::new();
    let stderr_result = child
        .stderr
        .take()
        .ok_or("stdio daemon omitted stderr")?
        .read_to_string(&mut stderr);
    let cleanup_result = fs::remove_dir_all(&directory);

    result?;
    trailing_result?;
    let status = wait_result?;
    stderr_result?;
    cleanup_result?;
    if !status.success() {
        return Err(format!("browserd serve --stdio exited with {status}").into());
    }
    if !trailing_stdout.is_empty() {
        return Err("stdio daemon emitted an unsolicited extra reply".into());
    }
    if !stderr.contains("browserd: ready transport=stdio") {
        return Err("stdio daemon did not report its transport readiness on stderr".into());
    }
    Ok(())
}

fn assert_view_retention_policy(
    browserd: &Path,
    rwb: &Path,
    alpha_url: &str,
    policy_arguments: &[&str],
    expiry_delay: Duration,
    policy_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = temporary_directory()?;
    let socket = directory.join("browser.sock");
    let profile_root = directory.join("profiles");
    let mut command = Command::new(browserd);
    command
        .arg("--socket")
        .arg(&socket)
        .arg("--profile-root")
        .arg(&profile_root)
        .args(policy_arguments)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn()?;
    let mut daemon = DaemonGuard { child, directory };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        if let Some(status) = daemon.child.try_wait()? {
            return Err(format!(
                "{policy_name} retention daemon exited before readiness: {status}"
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{policy_name} retention daemon did not create its socket within five seconds"
            )
            .into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let new_tab = run_rwb(rwb, &socket, &["tab", "new", alpha_url])?;
    let tab = new_tab
        .target
        .as_ref()
        .and_then(|target| target.tab_id.as_ref())
        .ok_or_else(|| format!("{policy_name} retention tab omitted its tab ID"))?;
    let first_page = run_rwb(
        rwb,
        &socket,
        &[
            "--tab",
            tab.as_str(),
            "observe",
            "--mode",
            "all",
            "--max-items",
            "1",
        ],
    )?;
    let Some(CommandResult::Observation(observation)) = first_page.result.as_ref() else {
        return Err(
            format!("{policy_name} retention observation returned the wrong result kind").into(),
        );
    };
    let cursor = observation
        .next_cursor
        .as_ref()
        .ok_or_else(|| format!("{policy_name} retention observation omitted its cursor"))?;
    if !expiry_delay.is_zero() {
        thread::sleep(expiry_delay);
    }
    let offset = cursor.offset.to_string();
    let expired = run_rwb_failure(
        rwb,
        &socket,
        &[
            "--tab",
            tab.as_str(),
            "observe",
            "--cursor",
            cursor.view_id.as_str(),
            &offset,
            "--max-items",
            "1",
        ],
    )?;
    if expired
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::ViewExpired, EffectState::None))
    {
        return Err(format!(
            "{policy_name} retention cursor did not fail with VIEW_EXPIRED/none: {:?}",
            expired.error
        )
        .into());
    }
    Ok(())
}

fn assert_cli_usage(rwb: &Path, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(rwb)
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()?;
    if output.status.code() != Some(5) || !output.stdout.is_empty() {
        return Err("rwb usage probe did not fail solely through stderr with status 5".into());
    }
    let usage = String::from_utf8(output.stderr)?;
    for required in [
        "capabilities",
        "tab close ID",
        "click --at X Y",
        "focus REF",
        "fill REF VALUE",
        "type REF TEXT",
        "key REF KEY",
        "select REF VALUE [VALUE...]",
        "check REF true|false",
        "submit REF",
        "upload REF --file PATH",
        "operation status REQUEST_ID",
        "events [--cursor SERVICE_INSTANCE_ID SEQUENCE]",
        "render-frame [--width WIDTH --height HEIGHT]",
        "console",
    ] {
        if !usage.contains(required) {
            return Err(format!("rwb usage omitted {required:?}").into());
        }
    }
    Ok(())
}

fn observed_checkbox(
    reply: &CommandReply,
) -> Result<(&ElementRef, bool), Box<dyn std::error::Error>> {
    let Some(CommandResult::Observation(observation)) = reply.result.as_ref() else {
        return Err("reply did not contain a control observation".into());
    };
    match observation.items.as_slice() {
        [ObservedItem::Control(control)]
            if control.role == "checkbox" && control.name == "Agent toggle" =>
        {
            Ok((&control.target, control.states.checked.unwrap_or(false)))
        }
        _ => Err("control observation did not contain the expected checkbox".into()),
    }
}

fn observation_text(reply: &CommandReply) -> Result<&str, Box<dyn std::error::Error>> {
    let Some(CommandResult::Observation(observation)) = reply.result.as_ref() else {
        return Err("reply did not contain an observation".into());
    };
    match observation.items.as_slice() {
        [ObservedItem::Text(text)] => Ok(&text.text),
        _ => Err("observation did not contain exactly one text item".into()),
    }
}

fn observed_control<'a>(
    reply: &'a CommandReply,
    role: &str,
    name: &str,
) -> Result<&'a browser_protocol::ObservedControl, Box<dyn std::error::Error>> {
    let Some(CommandResult::Observation(observation)) = reply.result.as_ref() else {
        return Err("reply did not contain a control observation".into());
    };
    let mut matches = observation.items.iter().filter_map(|item| match item {
        ObservedItem::Control(control) if control.role == role && control.name == name => {
            Some(control)
        }
        _ => None,
    });
    let control = matches
        .next()
        .ok_or_else(|| format!("control observation did not contain {role} {name:?}"))?;
    if matches.next().is_some() {
        return Err(format!("control observation contained duplicate {role} {name:?}").into());
    }
    Ok(control)
}

fn observed_link(
    reply: &CommandReply,
) -> Result<(&ElementRef, &str, &str), Box<dyn std::error::Error>> {
    let Some(CommandResult::Observation(observation)) = reply.result.as_ref() else {
        return Err("reply did not contain a link observation".into());
    };
    match observation.items.as_slice() {
        [ObservedItem::Link(link)] => Ok((&link.target, &link.name, &link.url)),
        _ => Err("link observation did not contain exactly one link".into()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let usage = "usage: cli-journey BROWSERD RWB [--seed SEED] [--cases COUNT]";
    let browserd = PathBuf::from(arguments.next().ok_or(usage)?);
    let rwb = PathBuf::from(arguments.next().ok_or(usage)?);
    let mut seed = DEFAULT_SEED;
    let mut cases = DEFAULT_CASES;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--seed" => {
                let value = arguments.next().ok_or(usage)?;
                seed = value
                    .strip_prefix("0x")
                    .map(|hex| u64::from_str_radix(hex, 16))
                    .unwrap_or_else(|| value.parse())?;
            }
            "--cases" => cases = arguments.next().ok_or(usage)?.parse()?,
            _ => return Err(usage.into()),
        }
    }
    if cases > 128 {
        return Err("generated CLI exploration is bounded to 128 cases".into());
    }
    assert_stdio_transport(&browserd)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let cors_listener = TcpListener::bind(("127.0.0.1", 0))?;
    let cors_address = cors_listener.local_addr()?;
    let expected_requests = cases + 1_082;
    let request_count = Arc::new(AtomicUsize::new(0));
    let fixture_request_count = Arc::clone(&request_count);
    let cors_fixture_request_count = Arc::clone(&request_count);
    let (upload_sender, upload_receiver) = mpsc::channel();
    let cors_upload_sender = upload_sender.clone();
    let (form_post_sender, form_post_receiver) = mpsc::channel();
    let cors_form_post_sender = form_post_sender.clone();
    let (fixture_stop_sender, fixture_stop_receiver) = mpsc::channel();
    let (cors_fixture_stop_sender, cors_fixture_stop_receiver) = mpsc::channel();
    cors_listener.set_nonblocking(true)?;
    let cors_fixture = thread::spawn(move || -> Result<(), String> {
        loop {
            match cors_listener.accept() {
                Ok((stream, _)) => {
                    cors_fixture_request_count.fetch_add(1, Ordering::SeqCst);
                    serve_fixture(stream, &cors_upload_sender, &cors_form_post_sender)
                        .map_err(|error| error.to_string())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    match cors_fixture_stop_receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    });
    listener.set_nonblocking(true)?;
    let fixture = thread::spawn(move || -> Result<(), String> {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    fixture_request_count.fetch_add(1, Ordering::SeqCst);
                    serve_fixture(stream, &upload_sender, &form_post_sender)
                        .map_err(|error| error.to_string())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    match fixture_stop_receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    });

    let directory = temporary_directory()?;
    let socket = directory.join("browser.sock");
    let profile_root = directory.join("profiles");
    let child = Command::new(&browserd)
        .arg("--socket")
        .arg(&socket)
        .arg("--profile-root")
        .arg(&profile_root)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut daemon = DaemonGuard { child, directory };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        if let Some(status) = daemon.child.try_wait()? {
            return Err(format!("browserd exited before readiness: {status}").into());
        }
        if Instant::now() >= deadline {
            return Err("browserd did not create its socket within five seconds".into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let alpha = format!("http://{address}/alpha");
    assert_view_retention_policy(
        &browserd,
        &rwb,
        &alpha,
        &[
            "--view-max-count",
            "16",
            "--view-max-bytes",
            "1",
            "--view-max-age-ms",
            "300000",
        ],
        Duration::ZERO,
        "byte-budget",
    )?;
    assert_view_retention_policy(
        &browserd,
        &rwb,
        &alpha,
        &[
            "--view-max-count",
            "16",
            "--view-max-bytes",
            "1048576",
            "--view-max-age-ms",
            "20",
        ],
        Duration::from_millis(50),
        "age-budget",
    )?;

    assert_cli_usage(&rwb, &socket)?;
    let version_reply = run_rwb(&rwb, &socket, &["version"])?;
    let Some(CommandResult::Version(version)) = version_reply.result.as_ref() else {
        return Err("version command returned the wrong result kind".into());
    };
    if version.product_version != env!("CARGO_PKG_VERSION")
        || version.protocol_version != version_reply.protocol_version
    {
        return Err(format!(
            "version command returned inconsistent runtime identity: product={} result_protocol={:?} envelope_protocol={:?}",
            version.product_version, version.protocol_version, version_reply.protocol_version
        )
        .into());
    }

    let advertised = run_rwb(&rwb, &socket, &["capabilities"])?;
    let Some(CommandResult::Capabilities(manifest)) = advertised.result.as_ref() else {
        return Err("capabilities command returned the wrong result kind".into());
    };
    let advertised_names = manifest
        .commands
        .iter()
        .map(|capability| capability.name.as_str())
        .collect::<Vec<_>>();
    let expected_capabilities = [
        "status",
        "daemon.stop",
        "version",
        "capabilities",
        "profile.create",
        "profile.list",
        "profile.open",
        "profile.close",
        "profile.remove",
        "tab.new",
        "tab.list",
        "tab.close",
        "tab.activate",
        "navigate.open",
        "navigate.back",
        "navigate.forward",
        "navigate.reload",
        "navigate.stop",
        "observe.text",
        "observe.controls",
        "observe.links",
        "observe.all",
        "read",
        "find",
        "screenshot",
        "frame_input",
        "interact.click",
        "interact.focus",
        "interact.fill",
        "interact.type",
        "interact.key",
        "interact.select",
        "interact.check",
        "interact.submit",
        "interact.scroll",
        "wait.idle",
        "wait.url",
        "wait.text",
        "wait.role_name",
        "wait.target_state",
        "wait.download",
        "upload",
        "download.start",
        "download.list",
        "download.wait",
        "download.cancel",
        "operation.get",
        "events.read",
        "diagnostic.render_frame",
        "diagnostic.evaluate",
        "diagnostic.console",
        "diagnostic.network",
    ];
    if advertised_names != expected_capabilities
        || manifest
            .commands
            .iter()
            .any(|capability| capability.schema_version != 1)
        || manifest.frame_formats != ["rgba8_premultiplied"]
    {
        return Err(format!(
            "capability manifest did not exactly match the routed schema-v1 surface: {advertised_names:?}"
        )
        .into());
    }

    let malformed = raw_daemon_reply(&socket, "{broken")?;
    let malformed_error = malformed
        .error
        .as_ref()
        .ok_or("malformed NDJSON did not return a protocol error")?;
    if malformed.success
        || malformed.request_id.as_str() != "r_transport"
        || malformed.target.is_some()
        || malformed.receipt.is_some()
        || malformed.result.is_some()
        || malformed_error.code != ErrorCode::InvalidRequest
        || malformed_error.effect != EffectState::None
    {
        return Err("malformed NDJSON did not return canonical INVALID_REQUEST/none".into());
    }
    let unknown_field = raw_daemon_reply(
        &socket,
        r#"{"protocol_version":1,"request_id":"r_unknown_field","profile_id":"p_default","tab_id":null,"command":{"command":"status"},"timeout_ms":1000,"unexpected":true}"#,
    )?;
    if unknown_field.request_id.as_str() != "r_unknown_field"
        || unknown_field
            .error
            .as_ref()
            .map(|error| (error.code, error.effect))
            != Some((ErrorCode::InvalidRequest, EffectState::None))
    {
        return Err(
            "unknown request field did not preserve its ID and return INVALID_REQUEST/none".into(),
        );
    }
    let unsupported_version = raw_daemon_reply(
        &socket,
        r#"{"protocol_version":2,"request_id":"r_bad_version","profile_id":"p_default","tab_id":null,"command":{"command":"status"},"timeout_ms":1000}"#,
    )?;
    if unsupported_version.request_id.as_str() != "r_bad_version"
        || unsupported_version
            .error
            .as_ref()
            .map(|error| (error.code, error.effect))
            != Some((ErrorCode::UnsupportedProtocol, EffectState::None))
    {
        return Err("protocol version 2 did not return UNSUPPORTED_PROTOCOL/none".into());
    }

    let first_status = run_rwb(&rwb, &socket, &["daemon", "status"])?;
    let first_instance = match first_status.result.as_ref() {
        Some(CommandResult::Status(status)) if status.running => status.service_instance_id.clone(),
        _ => return Err("daemon status did not report a running service".into()),
    };

    let new_tab = run_rwb(&rwb, &socket, &["tab", "new", &alpha])?;
    let tab = new_tab
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("tab creation did not return a tab ID")?;
    let tab_argument = tab.as_str();

    let initial_events = run_rwb(&rwb, &socket, &["events", "--max-events", "1"])?;
    let Some(CommandResult::Events(initial_batch)) = initial_events.result.as_ref() else {
        return Err("initial events command returned the wrong result kind".into());
    };
    if !initial_batch.events.is_empty()
        || initial_batch.next_cursor.service_instance_id != first_instance
    {
        return Err("initial event cursor was not empty and bound to the running daemon".into());
    }
    let pre_navigation_cursor: EventCursor = initial_batch.next_cursor.clone();

    let observed_alpha = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    let observed_alpha_text = observation_text(&observed_alpha)?;
    if observed_alpha_text != "Alpha λ navigation" {
        return Err(format!(
            "fresh CLI observed {observed_alpha_text:?} instead of the first Unicode document"
        )
        .into());
    }

    let read_alpha_reply = run_rwb(&rwb, &socket, &["--tab", tab_argument, "read"])?;
    if read_alpha_reply
        .receipt
        .as_ref()
        .is_none_or(|receipt| receipt.effect != EffectState::None)
    {
        return Err("read did not return an effect-free operation receipt".into());
    }
    let Some(CommandResult::Observation(read_alpha)) = read_alpha_reply.result.as_ref() else {
        return Err("read returned the wrong result kind".into());
    };
    if !matches!(
        read_alpha.items.as_slice(),
        [ObservedItem::Text(text)] if text.text == "Alpha λ navigation"
    ) || read_alpha.ready_state != DocumentReadyState::Complete
        || read_alpha.truncated
        || read_alpha.next_cursor.is_some()
    {
        return Err("read did not return the complete Unicode readable-text projection".into());
    }

    let found_text = run_rwb(&rwb, &socket, &["--tab", tab_argument, "find", "ALPHA λ"])?;
    let Some(CommandResult::Observation(found_text)) = found_text.result.as_ref() else {
        return Err("find text returned the wrong result kind".into());
    };
    if !matches!(
        found_text.items.as_slice(),
        [ObservedItem::Text(text)] if text.text == "Alpha λ navigation"
    ) || found_text.truncated
    {
        return Err("case-insensitive find did not return only the matching readable text".into());
    }

    let found_control = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "find", "agent toggle"],
    )?;
    let Some(CommandResult::Observation(found_control)) = found_control.result.as_ref() else {
        return Err("find control returned the wrong result kind".into());
    };
    if !matches!(
        found_control.items.as_slice(),
        [ObservedItem::Control(control)]
            if control.role == "checkbox" && control.name == "Agent toggle"
    ) {
        return Err("find did not return only the matching semantic control".into());
    }

    let found_link = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "find", "follow beta"],
    )?;
    let Some(CommandResult::Observation(found_link)) = found_link.result.as_ref() else {
        return Err("find link returned the wrong result kind".into());
    };
    if !matches!(
        found_link.items.as_slice(),
        [ObservedItem::Link(link)] if link.name == "Follow beta" && link.url.ends_with("/go")
    ) {
        return Err("find did not return only the matching semantic link".into());
    }

    let found_first = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "find", "a", "--max-items", "1"],
    )?;
    let Some(CommandResult::Observation(found_first)) = found_first.result.as_ref() else {
        return Err("paged find returned the wrong result kind".into());
    };
    if found_first.items.len() != 1 || !found_first.truncated {
        return Err("paged find did not report the first of multiple matches".into());
    }
    let find_cursor = found_first
        .next_cursor
        .as_ref()
        .ok_or("paged find omitted its immutable-view cursor")?;
    let find_offset = find_cursor.offset.to_string();
    let found_remainder = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "observe",
            "--cursor",
            find_cursor.view_id.as_str(),
            &find_offset,
            "--max-items",
            "8",
        ],
    )?;
    let Some(CommandResult::Observation(found_remainder)) = found_remainder.result.as_ref() else {
        return Err("find continuation returned the wrong result kind".into());
    };
    if found_remainder.view_id != found_first.view_id
        || found_remainder.items.len() != 2
        || found_remainder.truncated
    {
        return Err("find continuation did not return all remaining immutable-view matches".into());
    }

    let found_nothing_reply = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "find", "definitely absent"],
    )?;
    if found_nothing_reply
        .receipt
        .as_ref()
        .is_none_or(|receipt| receipt.effect != EffectState::None)
    {
        return Err("find did not return an effect-free operation receipt".into());
    }
    let Some(CommandResult::Observation(found_nothing)) = found_nothing_reply.result.as_ref()
    else {
        return Err("empty find returned the wrong result kind".into());
    };
    if !found_nothing.items.is_empty()
        || found_nothing.truncated
        || found_nothing.next_cursor.is_some()
    {
        return Err("empty find was not a valid empty observation".into());
    }

    let paged_first = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "observe",
            "--mode",
            "all",
            "--max-items",
            "1",
        ],
    )?;
    let Some(CommandResult::Observation(first_page)) = paged_first.result.as_ref() else {
        return Err("first paged observation returned the wrong result kind".into());
    };
    if !matches!(
        first_page.items.as_slice(),
        [ObservedItem::Text(text)] if text.text == "Alpha λ navigation"
    ) || first_page.ready_state != DocumentReadyState::Complete
        || !first_page.truncated
    {
        return Err("first paged observation was not the expected truncated text page".into());
    }
    let first_cursor = first_page
        .next_cursor
        .clone()
        .ok_or("truncated first observation omitted its continuation cursor")?;
    if first_cursor.view_id != first_page.view_id || first_cursor.offset != 1 {
        return Err("first observation cursor did not preserve its view at offset 1".into());
    }

    let controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let (checkbox, initially_checked) = observed_checkbox(&controls)?;
    if initially_checked {
        return Err("fixture checkbox was unexpectedly checked before interaction".into());
    }
    let checkbox = checkbox.clone();

    // A newer observation must not mutate the first view. Continue the original cursor and
    // require its remaining control and link in the fixture-defined order.
    let first_offset = first_cursor.offset.to_string();
    let paged_second = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "observe",
            "--cursor",
            first_cursor.view_id.as_str(),
            &first_offset,
            "--max-items",
            "1",
        ],
    )?;
    let Some(CommandResult::Observation(second_page)) = paged_second.result.as_ref() else {
        return Err("second paged observation returned the wrong result kind".into());
    };
    if !matches!(
        second_page.items.as_slice(),
        [ObservedItem::Control(control)]
            if control.role == "checkbox" && control.name == "Agent toggle"
    ) || second_page.view_id != first_cursor.view_id
        || second_page.ready_state != DocumentReadyState::Complete
        || !second_page.truncated
    {
        return Err("continued observation did not return the frozen checkbox page".into());
    }
    let second_cursor = second_page
        .next_cursor
        .clone()
        .ok_or("truncated second observation omitted its continuation cursor")?;
    if second_cursor.view_id != first_cursor.view_id || second_cursor.offset != 2 {
        return Err("second observation cursor did not preserve its view at offset 2".into());
    }
    let second_offset = second_cursor.offset.to_string();
    let paged_third = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "observe",
            "--cursor",
            second_cursor.view_id.as_str(),
            &second_offset,
            "--max-items",
            "1",
        ],
    )?;
    let Some(CommandResult::Observation(third_page)) = paged_third.result.as_ref() else {
        return Err("third paged observation returned the wrong result kind".into());
    };
    if !matches!(
        third_page.items.as_slice(),
        [ObservedItem::Link(link)]
            if link.name == "Follow beta" && link.url == format!("http://{address}/go")
    ) || third_page.view_id != first_cursor.view_id
        || third_page.ready_state != DocumentReadyState::Complete
        || third_page.truncated
        || third_page.next_cursor.is_some()
    {
        return Err("final observation page was not the frozen terminal link page".into());
    }

    let replay_request_id = "r_cli_replay_click";
    let first_click = run_rwb(
        &rwb,
        &socket,
        &[
            "--request-id",
            replay_request_id,
            "--tab",
            tab_argument,
            "click",
            checkbox.as_str(),
        ],
    )?;
    let first_receipt = first_click
        .receipt
        .as_ref()
        .ok_or("first explicit-ID click omitted its operation receipt")?;
    if first_receipt.status != OperationStatus::Completed
        || first_receipt.effect != EffectState::Applied
    {
        return Err("first explicit-ID click did not return a completed/applied receipt".into());
    }
    let replayed_click = run_rwb(
        &rwb,
        &socket,
        &[
            "--request-id",
            replay_request_id,
            "--tab",
            tab_argument,
            "click",
            checkbox.as_str(),
        ],
    )?;
    if replayed_click != first_click {
        return Err("identical request-ID replay did not return the retained reply".into());
    }
    let operation_status = run_rwb(&rwb, &socket, &["operation", "status", replay_request_id])?;
    let Some(CommandResult::Operation(operation)) = operation_status.result.as_ref() else {
        return Err("operation status returned the wrong result kind".into());
    };
    if operation.request_id.as_str() != replay_request_id
        || operation.receipt != *first_receipt
        || operation_status
            .receipt
            .as_ref()
            .map(|receipt| receipt.effect)
            != Some(EffectState::None)
    {
        return Err(
            "operation status did not return the retained receipt as a read-only query".into(),
        );
    }
    let missing_operation = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &["operation", "status", "r_missing_operation"],
        2,
    )?;
    if missing_operation
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::InvalidRequest, EffectState::None))
    {
        return Err("missing operation did not fail with INVALID_REQUEST/none".into());
    }
    let hidden_operation = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &[
            "--profile",
            "other",
            "operation",
            "status",
            replay_request_id,
        ],
        2,
    )?;
    if hidden_operation
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::InvalidRequest, EffectState::None))
    {
        return Err("operation status leaked a receipt across profiles".into());
    }
    let controls_after = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    if !observed_checkbox(&controls_after)?.1 {
        return Err("identical request-ID replay dispatched the checkbox click twice".into());
    }
    let conflicting_reuse = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &[
            "--request-id",
            replay_request_id,
            "--tab",
            tab_argument,
            "check",
            checkbox.as_str(),
            "false",
        ],
        5,
    )?;
    let conflict_error = conflicting_reuse
        .error
        .as_ref()
        .ok_or("conflicting request-ID reuse omitted its protocol error")?;
    if conflict_error.code != ErrorCode::RequestIdConflict
        || conflict_error.effect != EffectState::None
        || conflicting_reuse.receipt.is_some()
    {
        return Err("conflicting request-ID reuse did not fail before dispatch".into());
    }
    let controls_after_conflict = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    if !observed_checkbox(&controls_after_conflict)?.1 {
        return Err("conflicting request-ID reuse mutated the checkbox".into());
    }

    let timeout_request_id = "r_cli_timeout_before_dispatch";
    let timeout_arguments = [
        "--request-id",
        timeout_request_id,
        "--timeout-ms",
        "0",
        "--tab",
        tab_argument,
        "check",
        checkbox.as_str(),
        "false",
    ];
    let timed_out_check = run_rwb_failure_with_status(&rwb, &socket, &timeout_arguments, 3)?;
    let timeout_error = timed_out_check
        .error
        .as_ref()
        .ok_or("pre-dispatch timeout omitted its protocol error")?;
    let timed_out_receipt = timed_out_check
        .receipt
        .as_ref()
        .ok_or("pre-dispatch timeout omitted its operation receipt")?
        .clone();
    if timeout_error.code != ErrorCode::Timeout
        || timeout_error.effect != EffectState::None
        || timed_out_receipt.status != OperationStatus::TimedOut
        || timed_out_receipt.effect != EffectState::None
        || timed_out_check.result.is_some()
    {
        return Err("zero-budget request was not rejected before dispatch as TIMEOUT/none".into());
    }
    let timed_out_replay = run_rwb_failure_with_status(&rwb, &socket, &timeout_arguments, 3)?;
    if timed_out_replay != timed_out_check {
        return Err("identical timed-out request did not return the retained reply".into());
    }
    let timed_out_status = run_rwb(&rwb, &socket, &["operation", "status", timeout_request_id])?;
    let Some(CommandResult::Operation(operation)) = timed_out_status.result.as_ref() else {
        return Err("timed-out operation status returned the wrong result kind".into());
    };
    if operation.request_id.as_str() != timeout_request_id
        || operation.receipt != timed_out_receipt
        || timed_out_status
            .receipt
            .as_ref()
            .map(|receipt| receipt.effect)
            != Some(EffectState::None)
    {
        return Err("timed-out operation status did not return its retained receipt".into());
    }
    let controls_after_timeout = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    if !observed_checkbox(&controls_after_timeout)?.1 {
        return Err("pre-dispatch timeout mutated the checkbox".into());
    }

    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "check", checkbox.as_str(), "false"],
    )?;
    let controls_false = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    if observed_checkbox(&controls_false)?.1 {
        return Err("fresh CLI did not observe the requested unchecked state".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "check", checkbox.as_str(), "false"],
    )?;
    let controls_idempotent = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    if observed_checkbox(&controls_idempotent)?.1 {
        return Err("repeated unchecked request changed the checkbox state".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "check", checkbox.as_str(), "true"],
    )?;
    let controls_true = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    if !observed_checkbox(&controls_true)?.1 {
        return Err("fresh CLI did not observe the requested checked state".into());
    }

    let links = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "links"],
    )?;
    let (link, link_name, link_url) = observed_link(&links)?;
    let expected_link_url = format!("http://{address}/go");
    if link_name != "Follow beta" || link_url != expected_link_url {
        return Err(format!(
            "link observation returned name {link_name:?} and URL {link_url:?}, expected Follow beta and {expected_link_url:?}"
        )
        .into());
    }
    let link = link.clone();
    let followed = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "click", link.as_str()],
    )?;
    let expected_beta = format!("http://{address}/beta");
    let followed_document = match followed.result.as_ref() {
        Some(CommandResult::Navigation(navigation)) if navigation.url == expected_beta => {
            navigation.document_id.clone()
        }
        _ => return Err("link click did not report the redirected final URL".into()),
    };
    let pre_navigation_sequence = pre_navigation_cursor.sequence.to_string();
    let navigation_events = run_rwb(
        &rwb,
        &socket,
        &[
            "events",
            "--cursor",
            pre_navigation_cursor.service_instance_id.as_str(),
            &pre_navigation_sequence,
            "--max-events",
            "1",
        ],
    )?;
    let Some(CommandResult::Events(navigation_batch)) = navigation_events.result.as_ref() else {
        return Err("navigation events command returned the wrong result kind".into());
    };
    match navigation_batch.events.as_slice() {
        [event]
            if event.service_instance_id == first_instance
                && event.sequence > pre_navigation_cursor.sequence
                && matches!(
                    &event.event,
                    EventKind::NavigationCommitted { tab_id, document_id, url }
                        if tab_id == &tab
                            && document_id == &followed_document
                            && url == &expected_beta
                ) => {}
        _ => {
            return Err(
                "event log did not return the exact redirected navigation commit in order".into(),
            );
        }
    }
    if navigation_batch.next_cursor.service_instance_id != first_instance
        || navigation_batch.next_cursor.sequence != navigation_batch.events[0].sequence
    {
        return Err("navigation event cursor did not advance to the delivered event".into());
    }
    let navigation_event_cursor = navigation_batch.next_cursor.clone();
    let navigation_event_sequence = navigation_event_cursor.sequence.to_string();
    let no_duplicate_events = run_rwb(
        &rwb,
        &socket,
        &[
            "events",
            "--cursor",
            navigation_event_cursor.service_instance_id.as_str(),
            &navigation_event_sequence,
            "--max-events",
            "1",
        ],
    )?;
    let Some(CommandResult::Events(no_duplicate_batch)) = no_duplicate_events.result.as_ref()
    else {
        return Err("continued events command returned the wrong result kind".into());
    };
    if !no_duplicate_batch.events.is_empty()
        || no_duplicate_batch.next_cursor != navigation_event_cursor
    {
        return Err("continued event cursor duplicated or skipped the navigation boundary".into());
    }

    let eviction_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &alpha])?;
    let eviction_tab = eviction_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("eviction tab creation did not return a tab ID")?;
    let eviction_tab_argument = eviction_tab.as_str();
    let eviction_candidate = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            eviction_tab_argument,
            "observe",
            "--mode",
            "all",
            "--max-items",
            "1",
        ],
    )?;
    let Some(CommandResult::Observation(eviction_first_page)) = eviction_candidate.result.as_ref()
    else {
        return Err("eviction candidate returned the wrong result kind".into());
    };
    let eviction_cursor = eviction_first_page
        .next_cursor
        .clone()
        .ok_or("eviction candidate omitted its continuation cursor")?;
    for _ in 0..16 {
        run_rwb(&rwb, &socket, &["--tab", eviction_tab_argument, "observe"])?;
    }
    let eviction_offset = eviction_cursor.offset.to_string();
    let expired_cursor = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "--tab",
            eviction_tab_argument,
            "observe",
            "--cursor",
            eviction_cursor.view_id.as_str(),
            &eviction_offset,
            "--max-items",
            "1",
        ],
    )?;
    let expired_cursor_error = expired_cursor
        .error
        .ok_or("capacity-evicted observation cursor did not return an error")?;
    if expired_cursor_error.code != ErrorCode::ViewExpired
        || expired_cursor_error.effect != EffectState::None
    {
        return Err(format!(
            "evicted observation cursor returned {:?}/{:?} instead of VIEW_EXPIRED/none",
            expired_cursor_error.code, expired_cursor_error.effect
        )
        .into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", eviction_tab_argument])?;

    let stale_cursor_offset = first_cursor.offset.to_string();
    let stale_cursor = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "observe",
            "--cursor",
            first_cursor.view_id.as_str(),
            &stale_cursor_offset,
            "--max-items",
            "1",
        ],
    )?;
    let stale_cursor_error = stale_cursor
        .error
        .ok_or("navigation-invalidated observation cursor did not return an error")?;
    if stale_cursor_error.code != ErrorCode::StaleTarget
        || stale_cursor_error.effect != EffectState::None
    {
        return Err(format!(
            "old observation cursor returned {:?}/{:?} instead of STALE_TARGET/none",
            stale_cursor_error.code, stale_cursor_error.effect
        )
        .into());
    }

    let stale = run_rwb_failure(
        &rwb,
        &socket,
        &["--tab", tab_argument, "check", checkbox.as_str(), "false"],
    )?;
    let stale_error = stale.error.ok_or("stale check did not return an error")?;
    if stale_error.code != ErrorCode::StaleTarget || stale_error.effect != EffectState::None {
        return Err(format!(
            "old target returned {:?}/{:?} instead of STALE_TARGET/none",
            stale_error.code, stale_error.effect
        )
        .into());
    }
    let observed_beta = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    let Some(CommandResult::Observation(beta_observation)) = observed_beta.result.as_ref() else {
        return Err("fresh CLI did not return the second document observation".into());
    };
    let beta_text = observation_text(&observed_beta)?;
    if beta_observation.title != "Beta 第二"
        || beta_observation.loading
        || beta_observation.ready_state != DocumentReadyState::Complete
        || beta_text != "Beta 第二 page"
    {
        return Err(format!(
            "fresh CLI observed title {:?}, loading={}, ready_state={:?}, and text {:?} instead of the complete second Unicode document",
            beta_observation.title,
            beta_observation.loading,
            beta_observation.ready_state,
            beta_text
        )
        .into());
    }

    let scrolled = run_rwb(&rwb, &socket, &["--tab", tab_argument, "scroll", "7", "19"])?;
    let Some(CommandResult::Scroll(scroll)) = scrolled.result.as_ref() else {
        return Err("scroll returned the wrong result kind".into());
    };
    if scroll.offset_x != 7 || scroll.offset_y != 19 {
        return Err(format!(
            "scroll reported offset ({}, {}) instead of (7, 19)",
            scroll.offset_x, scroll.offset_y
        )
        .into());
    }
    let clamped_scroll = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "scroll", "-100", "-100"],
    )?;
    let Some(CommandResult::Scroll(clamped)) = clamped_scroll.result.as_ref() else {
        return Err("clamped scroll returned the wrong result kind".into());
    };
    if clamped.offset_x != 0 || clamped.offset_y != 0 {
        return Err(format!(
            "negative scroll reported offset ({}, {}) instead of clamping to (0, 0)",
            clamped.offset_x, clamped.offset_y
        )
        .into());
    }

    run_rwb(&rwb, &socket, &["--tab", tab_argument, "back"])?;
    let after_back = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&after_back)? != "Alpha λ navigation" {
        return Err("back did not restore the first document".into());
    }
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "forward"])?;
    let after_forward = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&after_forward)? != "Beta 第二 page" {
        return Err("forward did not restore the second document".into());
    }
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "reload"])?;
    let after_reload = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&after_reload)? != "Beta 第二 page" {
        return Err("reload did not preserve the second document".into());
    }

    let slow_navigation_url = format!("http://{address}/slow-navigation");
    let slow_navigation = Command::new(&rwb)
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .args(["--tab", tab_argument, "open", slow_navigation_url.as_str()])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    thread::sleep(Duration::from_millis(150));
    let stop_started = std::time::Instant::now();
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "stop"])?;
    if stop_started.elapsed() >= Duration::from_secs(1) {
        return Err("stop did not respond while navigation was in flight".into());
    }
    let slow_navigation_output = slow_navigation.wait_with_output()?;
    if slow_navigation_output.status.success() {
        return Err("stopped navigation unexpectedly completed successfully".into());
    }
    let slow_navigation_reply: CommandReply =
        serde_json::from_slice(&slow_navigation_output.stdout)?;
    if slow_navigation_reply.success
        || slow_navigation_reply
            .error
            .as_ref()
            .map(|error| error.effect)
            != Some(EffectState::None)
    {
        return Err(format!(
            "stopped navigation returned the wrong result: {slow_navigation_reply:?}"
        )
        .into());
    }
    let after_stop = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&after_stop)? != "Beta 第二 page" {
        return Err("stop did not preserve the committed document".into());
    }

    let multiline_url = format!("http://{address}/multiline-edit");
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &multiline_url],
    )?;
    let multiline_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let multiline_editor = observed_control(&multiline_controls, "textbox", "Multiline editor")?
        .target
        .clone();
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "fill",
            multiline_editor.as_str(),
            "ab\nγδ\n尾",
        ],
    )?;
    for (key, modifiers) in [
        ("Home", &[][..]),
        ("ArrowUp", &[][..]),
        ("Ω", &[][..]),
        ("End", &[][..]),
        ("ArrowUp", &[][..]),
        ("!", &[][..]),
        ("Home", &[][..]),
        ("ArrowDown", &[][..]),
        ("ArrowRight", &[][..]),
        ("ArrowRight", &["Shift"][..]),
        ("λ", &[][..]),
    ] {
        let mut arguments = vec!["--tab", tab_argument, "key", multiline_editor.as_str(), key];
        arguments.extend_from_slice(modifiers);
        run_rwb(&rwb, &socket, &arguments)?;
    }
    let edited_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let edited_multiline = observed_control(&edited_controls, "textbox", "Multiline editor")?;
    if edited_multiline.value.as_deref() != Some("ab!\nΩλδ\n尾") {
        return Err(format!(
            "multiline caret and selection editing exposed {:?} instead of the expected Unicode value",
            edited_multiline.value
        )
        .into());
    }

    let form_url = format!("http://{address}/form");
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "open", &form_url])?;
    let form_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let initial_name = observed_control(&form_controls, "textbox", "Agent name")?;
    if initial_name.value.as_deref() != Some("")
        || !initial_name.states.required
        || !initial_name.states.invalid
        || initial_name.states.focused
    {
        return Err(format!(
            "required form textbox exposed value={:?} required={} invalid={} focused={} before submission",
            initial_name.value,
            initial_name.states.required,
            initial_name.states.invalid,
            initial_name.states.focused
        )
        .into());
    }
    let name_input = initial_name.target.clone();
    let submitter = observed_control(&form_controls, "button", "Save form")?
        .target
        .clone();
    let requests_before_rejection = request_count.load(Ordering::SeqCst);
    let rejected_submission = run_rwb_failure(
        &rwb,
        &socket,
        &["--tab", tab_argument, "submit", submitter.as_str()],
    )?;
    let rejection = rejected_submission
        .error
        .ok_or("required-field submission did not return an error")?;
    if rejection.code != ErrorCode::TargetNotInteractable || rejection.effect != EffectState::None {
        return Err(format!(
            "required-field submission returned {:?}/{:?} instead of TARGET_NOT_INTERACTABLE/none",
            rejection.code, rejection.effect
        )
        .into());
    }
    if request_count.load(Ordering::SeqCst) != requests_before_rejection {
        return Err("required-field rejection reached the fixture server".into());
    }
    let rejected_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let rejected_name = observed_control(&rejected_controls, "textbox", "Agent name")?;
    if rejected_name.value.as_deref() != Some("")
        || !rejected_name.states.required
        || !rejected_name.states.invalid
        || !rejected_name.states.focused
    {
        return Err(format!(
            "rejected required textbox exposed value={:?} required={} invalid={} focused={}",
            rejected_name.value,
            rejected_name.states.required,
            rejected_name.states.invalid,
            rejected_name.states.focused
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "fill", name_input.as_str(), "Ada λ"],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "ArrowLeft",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "!"],
    )?;
    let caret_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let caret_name = observed_control(&caret_controls, "textbox", "Agent name")?;
    if caret_name.value.as_deref() != Some("Ada !λ") {
        return Err(format!(
            "caret insertion exposed value {:?} instead of editing before the final Unicode scalar",
            caret_name.value
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "Backspace",
        ],
    )?;
    let filled_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let filled_name = observed_control(&filled_controls, "textbox", "Agent name")?;
    if filled_name.value.as_deref() != Some("Ada λ")
        || !filled_name.states.required
        || filled_name.states.invalid
        || !filled_name.states.focused
    {
        return Err(format!(
            "filled required textbox exposed value={:?} required={} invalid={} focused={}",
            filled_name.value,
            filled_name.states.required,
            filled_name.states.invalid,
            filled_name.states.focused
        )
        .into());
    }

    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "Delete"],
    )?;
    let deleted_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let deleted_name = observed_control(&deleted_controls, "textbox", "Agent name")?;
    if deleted_name.value.as_deref() != Some("Ada ") {
        return Err(format!(
            "Delete did not remove the Unicode scalar at the caret: {:?}",
            deleted_name.value
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "λ"],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "Home"],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "ArrowRight",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), ">"],
    )?;
    let moved_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let moved_name = observed_control(&moved_controls, "textbox", "Agent name")?;
    if moved_name.value.as_deref() != Some("A>da λ") {
        return Err(format!(
            "Home plus ArrowRight did not place the caret after the first scalar: {:?}",
            moved_name.value
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "Backspace",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "End"],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "!"],
    )?;
    let ended_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let ended_name = observed_control(&ended_controls, "textbox", "Agent name")?;
    if ended_name.value.as_deref() != Some("Ada λ!") {
        return Err(format!(
            "End did not place the caret after the final Unicode scalar: {:?}",
            ended_name.value
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "Backspace",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "ArrowLeft",
            "Shift",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "Ω"],
    )?;
    let replaced_selection_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let replaced_selection_name =
        observed_control(&replaced_selection_controls, "textbox", "Agent name")?;
    if replaced_selection_name.value.as_deref() != Some("Ada Ω") {
        return Err(format!(
            "Shift-extended selection was not replaced by one Unicode scalar: {:?}",
            replaced_selection_name.value
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "ArrowLeft",
            "Shift",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "key",
            name_input.as_str(),
            "Backspace",
        ],
    )?;
    let deleted_selection_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let deleted_selection_name =
        observed_control(&deleted_selection_controls, "textbox", "Agent name")?;
    if deleted_selection_name.value.as_deref() != Some("Ada ") {
        return Err(format!(
            "Backspace did not delete the Shift-extended Unicode selection: {:?}",
            deleted_selection_name.value
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "key", name_input.as_str(), "λ"],
    )?;

    let submitted = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "submit", submitter.as_str()],
    )?;
    let expected_submission =
        format!("http://{address}/submitted?fixed=1&name=Ada+%CE%BB&intent=save");
    match submitted.result.as_ref() {
        Some(CommandResult::Navigation(navigation)) if navigation.url == expected_submission => {}
        _ => {
            return Err(
                "submitter-targeted form did not report the exact fixture-accepted URL".into(),
            );
        }
    }
    let submitted_document = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&submitted_document)? != "Submitted Ada λ via save" {
        return Err("submitted response was not committed through the real CLI path".into());
    }

    let post_form_url = format!("http://{address}/post-form");
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &post_form_url],
    )?;
    let post_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let post_name = observed_control(&post_controls, "textbox", "POST name")?
        .target
        .clone();
    let post_submitter = observed_control(&post_controls, "button", "Send POST")?
        .target
        .clone();
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "fill", post_name.as_str(), "Ada λ"],
    )?;
    let posted = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "submit", post_submitter.as_str()],
    )?;
    let expected_post_url = format!("http://{address}/posted");
    match posted.result.as_ref() {
        Some(CommandResult::Navigation(navigation)) if navigation.url == expected_post_url => {}
        _ => return Err("POST form did not navigate to the exact fixture route".into()),
    }
    let captured_post = form_post_receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "fixture did not observe the form POST")?;
    let expected_post_body = b"name=Ada+%CE%BB&intent=send";
    if captured_post.method != "POST"
        || captured_post.target != "/posted"
        || captured_post.content_type.as_deref() != Some("application/x-www-form-urlencoded")
        || captured_post.user_agent.as_deref() != Some("rwb/0.1.0")
        || captured_post.body != expected_post_body
    {
        return Err(format!(
            "fixture captured form request method={:?} target={:?} content-type={:?} user-agent={:?} body={:?}; expected POST /posted application/x-www-form-urlencoded rwb/0.1.0 {:?}",
            captured_post.method,
            captured_post.target,
            captured_post.content_type,
            captured_post.user_agent,
            String::from_utf8_lossy(&captured_post.body),
            String::from_utf8_lossy(expected_post_body)
        )
        .into());
    }
    let posted_document = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&posted_document)? != "Posted Ada λ via send" {
        return Err("POST response was not committed through the real CLI path".into());
    }

    let upload_bytes = UPLOAD_BYTES;
    let upload_path = daemon.directory.join("upload-λ.bin");
    fs::write(&upload_path, upload_bytes)?;
    let upload_path_text = upload_path
        .to_str()
        .ok_or("temporary upload path was not UTF-8")?;
    let upload_url = format!("http://{address}/upload");
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "open", &upload_url])?;
    let upload_controls = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "observe", "--mode", "controls"],
    )?;
    let Some(CommandResult::Observation(upload_observation)) = upload_controls.result.as_ref()
    else {
        return Err("upload page did not return a control observation".into());
    };
    let upload_target = upload_observation
        .items
        .iter()
        .find_map(|item| match item {
            ObservedItem::Control(control) if control.name == "Upload asset" => {
                Some(control.target.clone())
            }
            _ => None,
        })
        .ok_or("upload page did not expose its file input")?;
    let upload_submitter = upload_observation
        .items
        .iter()
        .find_map(|item| match item {
            ObservedItem::Control(control)
                if control.role == "button" && control.name == "Send upload" =>
            {
                Some(control.target.clone())
            }
            _ => None,
        })
        .ok_or("upload page did not expose its submit button")?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            tab_argument,
            "upload",
            upload_target.as_str(),
            "--file",
            upload_path_text,
        ],
    )?;
    let uploaded = run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "submit", upload_submitter.as_str()],
    )?;
    let expected_upload_url = format!("http://{address}/uploaded");
    match uploaded.result.as_ref() {
        Some(CommandResult::Navigation(navigation)) if navigation.url == expected_upload_url => {}
        _ => {
            return Err("multipart form submission did not navigate to the upload endpoint".into());
        }
    }
    let captured_upload = upload_receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "fixture did not observe the multipart upload")?;
    let expected_upload_hash = format!("{:x}", Sha256::digest(upload_bytes));
    let captured_upload_hash = format!("{:x}", Sha256::digest(&captured_upload.bytes));
    if captured_upload.filename != "upload-λ.bin"
        || captured_upload.bytes != upload_bytes
        || captured_upload_hash != expected_upload_hash
    {
        return Err(format!(
            "fixture captured upload filename {:?}, {} bytes, and hash {}; expected upload-λ.bin, {} bytes, and hash {}",
            captured_upload.filename,
            captured_upload.bytes.len(),
            captured_upload_hash,
            upload_bytes.len(),
            expected_upload_hash
        )
        .into());
    }
    let uploaded_document = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&uploaded_document)? != "Upload accepted" {
        return Err("uploaded response was not committed through the real CLI path".into());
    }

    let login_url = format!("http://{address}/login");
    let who_url = format!("http://{address}/who");
    let cookie_view_url = format!("http://{address}/cookie-view");
    let work_login = run_rwb(
        &rwb,
        &socket,
        &["--profile", "work", "tab", "new", &login_url],
    )?;
    let work_tab = work_login
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("work-profile login did not return a tab ID")?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "open",
            &who_url,
        ],
    )?;
    let work_identity = run_rwb(
        &rwb,
        &socket,
        &["--profile", "work", "--tab", work_tab.as_str(), "observe"],
    )?;
    if observation_text(&work_identity)? != "Session authenticated" {
        return Err("authenticated profile did not send its newly stored cookie".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "open",
            &cookie_view_url,
        ],
    )?;
    let cookie_view = run_rwb(
        &rwb,
        &socket,
        &["--profile", "work", "--tab", work_tab.as_str(), "observe"],
    )?;
    if observation_text(&cookie_view)? != "Cookie hidden" {
        return Err("HttpOnly cookie was exposed to page JavaScript".into());
    }

    let inactive_work_tab_reply = run_rwb(
        &rwb,
        &socket,
        &["--profile", "work", "tab", "new", &login_url],
    )?;
    let inactive_work_tab = inactive_work_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("second work-profile tab did not return a tab ID")?;
    let activated = run_rwb(
        &rwb,
        &socket,
        &["--profile", "work", "tab", "activate", work_tab.as_str()],
    )?;
    if activated
        .target
        .as_ref()
        .and_then(|target| target.tab_id.as_ref())
        != Some(&work_tab)
        || !matches!(activated.result, Some(CommandResult::Empty))
    {
        return Err("tab activate did not report the selected profile-owned tab".into());
    }
    let active_work_identity = run_rwb(&rwb, &socket, &["--profile", "work", "observe"])?;
    if active_work_identity
        .target
        .as_ref()
        .and_then(|target| target.tab_id.as_ref())
        != Some(&work_tab)
        || observation_text(&active_work_identity)? != "Cookie hidden"
    {
        return Err("a command without --tab did not use the activated work-profile tab".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "tab",
            "close",
            inactive_work_tab.as_str(),
        ],
    )?;

    let download_url = format!("http://{address}/download");
    let started_download = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "start",
            &download_url,
        ],
    )?;
    let Some(CommandResult::Download(started)) = started_download.result.as_ref() else {
        return Err("download start returned the wrong result kind".into());
    };
    let expected_download_hash = format!("{:x}", Sha256::digest(DOWNLOAD_BYTES));
    if started.status != TransferStatus::Completed
        || started.bytes != DOWNLOAD_BYTES.len() as u64
        || started.sha256.as_deref() != Some(expected_download_hash.as_str())
        || started.path.is_some()
    {
        return Err(
            "download start did not report the independently expected bytes and hash".into(),
        );
    }
    let download_id = started.download_id.clone();
    let listed_downloads = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "list",
        ],
    )?;
    let Some(CommandResult::Downloads(listed)) = listed_downloads.result.as_ref() else {
        return Err("download list returned the wrong result kind".into());
    };
    if listed_downloads
        .receipt
        .as_ref()
        .map(|receipt| receipt.effect)
        != Some(EffectState::None)
    {
        return Err("download list incorrectly reported a mutating effect".into());
    }
    if listed.as_slice() != [started.clone()] {
        return Err("download list did not return the exact active transfer".into());
    }
    let download_path = daemon.directory.join("download-λ.bin");
    let download_path_text = download_path
        .to_str()
        .ok_or("temporary download path was not UTF-8")?;
    let completed_download = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "wait",
            "--id",
            download_id.as_str(),
            "--out",
            download_path_text,
        ],
    )?;
    let Some(CommandResult::Download(completed)) = completed_download.result.as_ref() else {
        return Err("download wait returned the wrong result kind".into());
    };
    let downloaded_bytes = fs::read(&download_path)?;
    let downloaded_hash = format!("{:x}", Sha256::digest(&downloaded_bytes));
    if downloaded_bytes != DOWNLOAD_BYTES
        || downloaded_hash != expected_download_hash
        || completed.status != TransferStatus::Completed
        || completed.bytes != DOWNLOAD_BYTES.len() as u64
        || completed.sha256.as_deref() != Some(expected_download_hash.as_str())
        || completed.path.as_deref() != Some(download_path_text)
    {
        return Err("published download bytes, hash, or receipt did not match the fixture".into());
    }
    let repeated_wait = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "wait",
            "--id",
            download_id.as_str(),
            "--out",
            download_path_text,
        ],
    )?;
    if repeated_wait.error.as_ref().map(|error| error.code) != Some(ErrorCode::TransferFailed)
        || fs::read(&download_path)? != DOWNLOAD_BYTES
    {
        return Err("download replay overwrote the destination or returned the wrong error".into());
    }
    let listed_after_wait = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "list",
        ],
    )?;
    if !matches!(listed_after_wait.result, Some(CommandResult::Downloads(ref listed)) if listed.is_empty())
    {
        return Err("published download remained in the active transfer list".into());
    }

    let cancellable_download = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "start",
            &download_url,
        ],
    )?;
    let Some(CommandResult::Download(cancellable)) = cancellable_download.result.as_ref() else {
        return Err("cancellable download start returned the wrong result kind".into());
    };
    let listed_cancellable = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "list",
        ],
    )?;
    let Some(CommandResult::Downloads(listed)) = listed_cancellable.result.as_ref() else {
        return Err("download list returned the wrong result kind before cancellation".into());
    };
    if listed.as_slice() != [cancellable.clone()] {
        return Err("download list did not return the exact cancellable transfer".into());
    }
    let cancelled_download = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "cancel",
            "--id",
            cancellable.download_id.as_str(),
        ],
    )?;
    let Some(CommandResult::Download(cancelled)) = cancelled_download.result.as_ref() else {
        return Err("download cancel returned the wrong result kind".into());
    };
    if cancelled.status != TransferStatus::Cancelled
        || cancelled.bytes != DOWNLOAD_BYTES.len() as u64
        || cancelled.sha256.is_some()
        || cancelled.path.is_some()
    {
        return Err("download cancel returned an incorrect terminal receipt".into());
    }
    let cancelled_path = daemon.directory.join("cancelled-download.bin");
    let cancelled_path_text = cancelled_path
        .to_str()
        .ok_or("temporary cancelled-download path was not UTF-8")?;
    let cancelled_wait = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "wait",
            "--id",
            cancellable.download_id.as_str(),
            "--out",
            cancelled_path_text,
        ],
    )?;
    if cancelled_wait.error.as_ref().map(|error| error.code) != Some(ErrorCode::TransferFailed)
        || cancelled_path.exists()
    {
        return Err("cancelled download remained publishable or created an output artifact".into());
    }
    let listed_after_cancel = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            work_tab.as_str(),
            "download",
            "list",
        ],
    )?;
    if !matches!(listed_after_cancel.result, Some(CommandResult::Downloads(ref listed)) if listed.is_empty())
    {
        return Err("cancelled download remained in the active transfer list".into());
    }

    let other_identity = run_rwb(
        &rwb,
        &socket,
        &["--profile", "other", "tab", "new", &who_url],
    )?;
    let other_tab = other_identity
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("other-profile request did not return a tab ID")?;
    let other_observation = run_rwb(
        &rwb,
        &socket,
        &["--profile", "other", "--tab", other_tab.as_str(), "observe"],
    )?;
    if observation_text(&other_observation)? != "Session anonymous" {
        return Err("cookie state leaked into the isolated profile".into());
    }
    let isolated_event_sequence = pre_navigation_cursor.sequence.to_string();
    let isolated_events = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "other",
            "events",
            "--cursor",
            pre_navigation_cursor.service_instance_id.as_str(),
            &isolated_event_sequence,
            "--max-events",
            "256",
        ],
    )?;
    let Some(CommandResult::Events(isolated_batch)) = isolated_events.result.as_ref() else {
        return Err("isolated-profile events command returned the wrong result kind".into());
    };
    if !isolated_batch.events.is_empty() {
        return Err("event log leaked another profile's navigation or transfer events".into());
    }

    let cross_profile = run_rwb_failure(
        &rwb,
        &socket,
        &["--profile", "other", "--tab", work_tab.as_str(), "observe"],
    )?;
    let cross_profile_error = cross_profile
        .error
        .ok_or("cross-profile tab access did not return an error")?;
    if cross_profile_error.code != ErrorCode::TabNotFound
        || cross_profile_error.effect != EffectState::None
    {
        return Err(format!(
            "cross-profile tab access returned {:?}/{:?} instead of TAB_NOT_FOUND/none",
            cross_profile_error.code, cross_profile_error.effect
        )
        .into());
    }

    for (profile, owned_tab, hidden_tab) in [
        ("work", &work_tab, &other_tab),
        ("other", &other_tab, &work_tab),
    ] {
        let listed = run_rwb(&rwb, &socket, &["--profile", profile, "tab", "list"])?;
        let Some(CommandResult::Tabs(tabs)) = listed.result.as_ref() else {
            return Err(
                format!("profile {profile} tab list returned the wrong result kind").into(),
            );
        };
        if !tabs.iter().any(|tab| &tab.tab_id == owned_tab)
            || tabs.iter().any(|tab| &tab.tab_id == hidden_tab)
        {
            return Err(format!(
                "profile {profile} tab list did not retain its own tab and hide the other profile's tab"
            )
            .into());
        }
    }

    let mut random = seed;
    let mut previous_checkbox = checkbox.clone();
    for case_index in 0..cases {
        let sample = next_random(&mut random);
        let variant = (sample as usize) % CASE_TEXTS.len();
        let initially_checked = sample & (1 << 8) != 0;
        let requested = sample & (1 << 16) != 0;
        let action_order = ((sample >> 24) & 3) as u8;
        let initial_segment = if initially_checked {
            "checked"
        } else {
            "unchecked"
        };
        let case_url = format!("http://{address}/case/{case_index}/{variant}/{initial_segment}");
        run_rwb(&rwb, &socket, &["--tab", tab_argument, "open", &case_url])?;

        let stale = run_rwb_failure(
            &rwb,
            &socket,
            &[
                "--tab",
                tab_argument,
                "check",
                previous_checkbox.as_str(),
                "false",
            ],
        )?;
        let stale_error = stale
            .error
            .ok_or("generated stale check did not return an error")?;
        if stale_error.code != ErrorCode::StaleTarget || stale_error.effect != EffectState::None {
            return Err(format!(
                "case {case_index} old target returned {:?}/{:?} instead of STALE_TARGET/none",
                stale_error.code, stale_error.effect
            )
            .into());
        }

        let observed = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
        let expected_text = format!("Case {case_index}: {}", CASE_TEXTS[variant]);
        if observation_text(&observed)? != expected_text {
            return Err(format!(
                "case {case_index} observed {:?} instead of {expected_text:?}",
                observation_text(&observed)?
            )
            .into());
        }
        let controls = run_rwb(
            &rwb,
            &socket,
            &["--tab", tab_argument, "observe", "--mode", "controls"],
        )?;
        let (target, observed_initial) = observed_checkbox(&controls)?;
        if observed_initial != initially_checked {
            return Err(format!(
                "case {case_index} initial checkbox state was {observed_initial}, expected {initially_checked}"
            )
            .into());
        }
        let target = target.clone();
        let requested_argument = if requested { "true" } else { "false" };
        let expected_final = match action_order {
            0 => {
                run_rwb(
                    &rwb,
                    &socket,
                    &[
                        "--tab",
                        tab_argument,
                        "check",
                        target.as_str(),
                        requested_argument,
                    ],
                )?;
                run_rwb(
                    &rwb,
                    &socket,
                    &[
                        "--tab",
                        tab_argument,
                        "check",
                        target.as_str(),
                        requested_argument,
                    ],
                )?;
                requested
            }
            1 => {
                run_rwb(
                    &rwb,
                    &socket,
                    &["--tab", tab_argument, "click", target.as_str()],
                )?;
                run_rwb(
                    &rwb,
                    &socket,
                    &["--tab", tab_argument, "click", target.as_str()],
                )?;
                initially_checked
            }
            2 => {
                run_rwb(
                    &rwb,
                    &socket,
                    &["--tab", tab_argument, "click", target.as_str()],
                )?;
                run_rwb(
                    &rwb,
                    &socket,
                    &[
                        "--tab",
                        tab_argument,
                        "check",
                        target.as_str(),
                        requested_argument,
                    ],
                )?;
                requested
            }
            _ => {
                run_rwb(
                    &rwb,
                    &socket,
                    &[
                        "--tab",
                        tab_argument,
                        "check",
                        target.as_str(),
                        requested_argument,
                    ],
                )?;
                run_rwb(
                    &rwb,
                    &socket,
                    &["--tab", tab_argument, "click", target.as_str()],
                )?;
                !requested
            }
        };
        let final_controls = run_rwb(
            &rwb,
            &socket,
            &["--tab", tab_argument, "observe", "--mode", "controls"],
        )?;
        let (_, observed_final) = observed_checkbox(&final_controls)?;
        if observed_final != expected_final {
            return Err(format!(
                "case {case_index} final checkbox state was {observed_final}, expected {expected_final} for action order {action_order}"
            )
            .into());
        }
        previous_checkbox = target;
    }

    // Write origin-scoped storage through page JavaScript before the hard daemon replacement.
    // A semantic readback proves the live page observed the value before persistence is tested.
    let storage_write_url = format!("http://{address}/storage-write");
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &storage_write_url],
    )?;
    let storage_before_restart = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&storage_before_restart)? != "Storage durable λ" {
        return Err("page localStorage write was not visible before daemon replacement".into());
    }

    // Session storage belongs to one live tab and is partitioned by serialized origin.
    // Prove same-origin retention, cross-origin isolation and restoration, and tab isolation
    // through fresh release-CLI processes before replacing the daemon.
    let session_write_url = format!("http://{address}/session-write");
    let session_read_url = format!("http://{address}/session-read");
    let isolated_session_url = format!("http://{cors_address}/session-read");
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &session_write_url],
    )?;
    let session_written = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&session_written)? != "Session tab λ" {
        return Err("page sessionStorage write was not visible in its tab".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &session_read_url],
    )?;
    let session_same_origin = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&session_same_origin)? != "Session tab λ" {
        return Err("same-tab same-origin sessionStorage did not survive navigation".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &isolated_session_url],
    )?;
    let session_cross_origin = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&session_cross_origin)? != "Session empty" {
        return Err("sessionStorage leaked across serialized origins".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &session_read_url],
    )?;
    let session_restored_origin = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&session_restored_origin)? != "Session tab λ" {
        return Err("sessionStorage was not restored after returning to its origin".into());
    }
    let isolated_session_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &session_read_url])?;
    let isolated_session_tab = isolated_session_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("isolated sessionStorage check did not return a tab ID")?;
    let isolated_session_tab_observation = run_rwb(
        &rwb,
        &socket,
        &["--tab", isolated_session_tab.as_str(), "observe"],
    )?;
    if observation_text(&isolated_session_tab_observation)? != "Session empty" {
        return Err("sessionStorage leaked into a separate tab".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["tab", "close", isolated_session_tab.as_str()],
    )?;

    // End the pre-restart journey on a known two-entry history suffix. The replacement
    // daemon must restore this exact tab and cursor rather than creating a substitute.
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "open", &alpha])?;
    run_rwb(
        &rwb,
        &socket,
        &["--tab", tab_argument, "open", &expected_beta],
    )?;

    daemon.child.kill()?;
    let stopped = daemon.child.wait()?;
    if !stopped.success() && stopped.code().is_some() {
        return Err(
            format!("browserd replacement stopped with unexpected status {stopped}").into(),
        );
    }
    daemon.child = Command::new(&browserd)
        .arg("--socket")
        .arg(&socket)
        .arg("--profile-root")
        .arg(&profile_root)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let restart_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(status) = daemon.child.try_wait()? {
            return Err(format!("replacement browserd exited before readiness: {status}").into());
        }
        if Instant::now() >= restart_deadline {
            return Err(
                "replacement browserd did not accept socket connections within five seconds".into(),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }

    let restarted_status = run_rwb(&rwb, &socket, &["daemon", "status"])?;
    let restarted_instance = match restarted_status.result.as_ref() {
        Some(CommandResult::Status(status))
            if status.running && status.service_instance_id != first_instance =>
        {
            status.service_instance_id.clone()
        }
        _ => return Err("replacement daemon did not report a new running service instance".into()),
    };
    let old_event_sequence = navigation_event_cursor.sequence.to_string();
    let stale_events = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "events",
            "--cursor",
            navigation_event_cursor.service_instance_id.as_str(),
            &old_event_sequence,
            "--max-events",
            "1",
        ],
    )?;
    let stale_event_error = stale_events
        .error
        .as_ref()
        .ok_or("old daemon event cursor did not return an error")?;
    if stale_event_error.code != ErrorCode::EventGap
        || stale_event_error.effect != EffectState::None
        || stale_event_error.details.iter().all(|detail| {
            detail.field.as_deref() != Some("cursor")
                || !detail.reason.starts_with("first_available_sequence=")
        })
    {
        return Err("old daemon event cursor did not return structured EVENT_GAP/none".into());
    }

    let overflow_start = run_rwb(&rwb, &socket, &["events", "--max-events", "1"])?;
    let Some(CommandResult::Events(overflow_start_batch)) = overflow_start.result.as_ref() else {
        return Err("same-instance overflow cursor returned the wrong result kind".into());
    };
    if !overflow_start_batch.events.is_empty()
        || overflow_start_batch.next_cursor.service_instance_id != restarted_instance
    {
        return Err("same-instance overflow cursor was not empty and replacement-bound".into());
    }
    let overflow_cursor = overflow_start_batch.next_cursor.clone();

    let restored_tabs = run_rwb(&rwb, &socket, &["tab", "list"])?;
    let Some(CommandResult::Tabs(restored_tabs)) = restored_tabs.result.as_ref() else {
        return Err("restarted default-profile tab list returned the wrong result kind".into());
    };
    if !restored_tabs.iter().any(|summary| summary.tab_id == tab) {
        return Err("replacement daemon did not restore the original tab identity".into());
    }
    let restored_current = run_rwb(&rwb, &socket, &["observe"])?;
    if restored_current
        .target
        .as_ref()
        .and_then(|target| target.tab_id.as_ref())
        != Some(&tab)
    {
        return Err("replacement daemon did not restore the active tab selection".into());
    }
    let Some(CommandResult::Observation(restored_observation)) = restored_current.result.as_ref()
    else {
        return Err("replacement daemon did not return the restored document observation".into());
    };
    if restored_observation.title != "Beta 第二"
        || restored_observation.loading
        || restored_observation.ready_state != DocumentReadyState::Complete
        || observation_text(&restored_current)? != "Beta 第二 page"
    {
        return Err(
            "replacement daemon did not restore the tab's complete current document and title"
                .into(),
        );
    }

    let storage_read_url = format!("http://{address}/storage-read");
    let storage_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &storage_read_url])?;
    let storage_tab = storage_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("restored localStorage check did not return a tab ID")?;
    let storage_after_restart =
        run_rwb(&rwb, &socket, &["--tab", storage_tab.as_str(), "observe"])?;
    if observation_text(&storage_after_restart)? != "Storage durable λ" {
        return Err("same-origin localStorage did not survive daemon replacement".into());
    }

    let isolated_storage_url = format!("http://{cors_address}/storage-read");
    run_rwb(
        &rwb,
        &socket,
        &["--tab", storage_tab.as_str(), "open", &isolated_storage_url],
    )?;
    let isolated_storage = run_rwb(&rwb, &socket, &["--tab", storage_tab.as_str(), "observe"])?;
    if observation_text(&isolated_storage)? != "Storage empty" {
        return Err("localStorage leaked across serialized origins".into());
    }

    let restarted_session_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &session_read_url])?;
    let restarted_session_tab = restarted_session_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("restarted sessionStorage check did not return a tab ID")?;
    let session_after_restart = run_rwb(
        &rwb,
        &socket,
        &["--tab", restarted_session_tab.as_str(), "observe"],
    )?;
    if observation_text(&session_after_restart)? != "Session empty" {
        return Err("sessionStorage survived daemon replacement".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["tab", "close", restarted_session_tab.as_str()],
    )?;

    let restored_operation = run_rwb(&rwb, &socket, &["operation", "status", replay_request_id])?;
    let Some(CommandResult::Operation(operation)) = restored_operation.result.as_ref() else {
        return Err(
            "completed operation status after daemon replacement returned the wrong result kind"
                .into(),
        );
    };
    if operation.request_id.as_str() != replay_request_id
        || operation.receipt != *first_receipt
        || restored_operation
            .receipt
            .as_ref()
            .map(|receipt| (receipt.status, receipt.effect))
            != Some((OperationStatus::Completed, EffectState::None))
    {
        return Err("completed operation did not survive daemon replacement".into());
    }
    let restored_replay = run_rwb(
        &rwb,
        &socket,
        &[
            "--request-id",
            replay_request_id,
            "--tab",
            tab_argument,
            "click",
            checkbox.as_str(),
        ],
    )?;
    if restored_replay != first_click {
        return Err(
            "identical completed request did not return its retained reply after daemon replacement"
                .into(),
        );
    }
    let restored_conflict = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &[
            "--request-id",
            replay_request_id,
            "--tab",
            tab_argument,
            "check",
            checkbox.as_str(),
            "false",
        ],
        5,
    )?;
    if restored_conflict
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::RequestIdConflict, EffectState::None))
        || restored_conflict.receipt.is_some()
    {
        return Err(
            "completed request identity did not reject a different payload after daemon replacement"
                .into(),
        );
    }
    let restored_timeout = run_rwb(&rwb, &socket, &["operation", "status", timeout_request_id])?;
    let Some(CommandResult::Operation(operation)) = restored_timeout.result.as_ref() else {
        return Err(
            "timed-out operation status after daemon replacement returned the wrong result kind"
                .into(),
        );
    };
    if operation.request_id.as_str() != timeout_request_id
        || operation.receipt != timed_out_receipt
        || restored_timeout
            .receipt
            .as_ref()
            .map(|receipt| (receipt.status, receipt.effect))
            != Some((OperationStatus::Completed, EffectState::None))
    {
        return Err("retained timed-out operation did not survive daemon replacement".into());
    }

    run_rwb(&rwb, &socket, &["--tab", tab_argument, "back"])?;
    let restored_back = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&restored_back)? != "Alpha λ navigation" {
        return Err("restored history cursor could not traverse to the preceding document".into());
    }
    run_rwb(&rwb, &socket, &["--tab", tab_argument, "forward"])?;
    let restored_forward = run_rwb(&rwb, &socket, &["--tab", tab_argument, "observe"])?;
    if observation_text(&restored_forward)? != "Beta 第二 page" {
        return Err(
            "restored history cursor could not traverse back to the active document".into(),
        );
    }

    let restored_identity = run_rwb(
        &rwb,
        &socket,
        &["--profile", "work", "tab", "new", &who_url],
    )?;
    let restored_tab = restored_identity
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("restarted work-profile request did not return a tab ID")?;
    let restored_observation = run_rwb(
        &rwb,
        &socket,
        &[
            "--profile",
            "work",
            "--tab",
            restored_tab.as_str(),
            "observe",
        ],
    )?;
    if observation_text(&restored_observation)? != "Session authenticated" {
        return Err("restarted daemon did not reload the authenticated profile cookie".into());
    }

    for overflow_index in 0..1_025 {
        let overflow_url = if overflow_index % 2 == 0 {
            alpha.as_str()
        } else {
            expected_beta.as_str()
        };
        run_rwb(
            &rwb,
            &socket,
            &["--tab", tab_argument, "open", overflow_url],
        )?;
    }
    let overflow_sequence = overflow_cursor.sequence.to_string();
    let overflow_events = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "events",
            "--cursor",
            overflow_cursor.service_instance_id.as_str(),
            &overflow_sequence,
            "--max-events",
            "1",
        ],
    )?;
    let overflow_error = overflow_events
        .error
        .as_ref()
        .ok_or("same-instance expired event cursor did not return an error")?;
    if overflow_error.code != ErrorCode::EventGap
        || overflow_error.effect != EffectState::None
        || overflow_error.details.iter().all(|detail| {
            detail.field.as_deref() != Some("cursor")
                || !detail.reason.starts_with("first_available_sequence=")
        })
    {
        return Err(
            "same-instance expired event cursor did not return structured EVENT_GAP/none".into(),
        );
    }

    let final_status = run_rwb(&rwb, &socket, &["daemon", "status"])?;
    match final_status.result.as_ref() {
        Some(CommandResult::Status(status))
            if status.service_instance_id == restarted_instance && status.running => {}
        _ => return Err("fresh CLI did not reconnect to the replacement service instance".into()),
    }

    let lifecycle_name = "lifecycle";
    run_rwb(&rwb, &socket, &["profile", "create", lifecycle_name])?;
    let listed = run_rwb(&rwb, &socket, &["profile", "list"])?;
    let Some(CommandResult::Profiles(profiles)) = listed.result.as_ref() else {
        return Err("profile list returned the wrong result kind".into());
    };
    if !profiles
        .iter()
        .any(|profile| profile.as_str() == "p_lifecycle")
    {
        return Err("newly created profile was absent from profile list".into());
    }
    run_rwb(&rwb, &socket, &["profile", "close", lifecycle_name])?;
    run_rwb(&rwb, &socket, &["profile", "open", lifecycle_name])?;
    run_rwb(&rwb, &socket, &["profile", "close", lifecycle_name])?;
    run_rwb(&rwb, &socket, &["profile", "remove", lifecycle_name])?;
    let after_remove = run_rwb(&rwb, &socket, &["profile", "list"])?;
    let Some(CommandResult::Profiles(profiles)) = after_remove.result.as_ref() else {
        return Err("profile list after removal returned the wrong result kind".into());
    };
    if profiles
        .iter()
        .any(|profile| profile.as_str() == "p_lifecycle")
    {
        return Err("removed profile remained in profile list".into());
    }
    let missing = run_rwb_failure(&rwb, &socket, &["profile", "open", lifecycle_name])?;
    let missing_error = missing
        .error
        .ok_or("opening a removed profile did not return an error")?;
    if missing_error.code != ErrorCode::ProfileNotFound || missing_error.effect != EffectState::None
    {
        return Err(format!(
            "opening a removed profile returned {:?}/{:?} instead of PROFILE_NOT_FOUND/none",
            missing_error.code, missing_error.effect
        )
        .into());
    }

    let timer_url = format!("http://{address}/timer");
    let timer_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &timer_url])?;
    let timer_tab = timer_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("timer tab creation did not return a tab ID")?;
    let timer_tab_argument = timer_tab.as_str();
    std::thread::sleep(Duration::from_millis(75));
    let timer_observation = run_rwb(
        &rwb,
        &socket,
        &["--tab", timer_tab_argument, "observe", "--mode", "text"],
    )?;
    let Some(CommandResult::Observation(timer_page)) = timer_observation.result.as_ref() else {
        return Err("timer readback returned the wrong result kind".into());
    };
    if !timer_page
        .items
        .iter()
        .any(|item| matches!(item, ObservedItem::Text(text) if text.text == "Timer fired"))
    {
        return Err("page timer did not advance before a fresh observe request".into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", timer_tab_argument])?;

    let fetch_url = format!("http://{address}/fetch-page");
    let fetch_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &fetch_url])?;
    let fetch_tab = fetch_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("Fetch tab creation did not return a tab ID")?;
    let fetch_tab_argument = fetch_tab.as_str();
    std::thread::sleep(Duration::from_millis(75));
    let fetch_observation = run_rwb(
        &rwb,
        &socket,
        &["--tab", fetch_tab_argument, "observe", "--mode", "text"],
    )?;
    let Some(CommandResult::Observation(fetch_page)) = fetch_observation.result.as_ref() else {
        return Err("Fetch readback returned the wrong result kind".into());
    };
    if !fetch_page
        .items
        .iter()
        .any(|item| matches!(item, ObservedItem::Text(text) if text.text == "Fetched λ payload"))
    {
        return Err("packaged Fetch wait succeeded without the expected DOM mutation".into());
    }
    let network_reply = run_rwb(&rwb, &socket, &["--tab", fetch_tab_argument, "network"])?;
    if network_reply.receipt.as_ref().map(|receipt| receipt.effect) != Some(EffectState::None) {
        return Err("network did not report read-only effect none".into());
    }
    let Some(CommandResult::Network(records)) = network_reply.result.as_ref() else {
        return Err("network returned the wrong result kind".into());
    };
    let expected_requested_url = "/fetch-data";
    let expected_final_url = format!("http://{address}/fetch-data");
    let [record] = records.as_slice() else {
        return Err(format!("network returned {} records instead of one", records.len()).into());
    };
    if record.sequence != 1
        || record.request_id.is_empty()
        || record.requested_url != expected_requested_url
        || record.final_url.as_deref() != Some(expected_final_url.as_str())
        || record.status != Some(200)
        || record.error.is_some()
    {
        return Err(format!("network record was incorrect: {record:?}").into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", fetch_tab_argument])?;

    let cors_page_url = format!("http://{address}/cors-page?origin=http://{cors_address}");
    let cors_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &cors_page_url])?;
    let cors_tab = cors_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("CORS tab creation did not return a tab ID")?;
    let cors_tab_argument = cors_tab.as_str();
    std::thread::sleep(Duration::from_millis(75));
    let cors_observation = run_rwb(
        &rwb,
        &socket,
        &["--tab", cors_tab_argument, "observe", "--mode", "text"],
    )?;
    let Some(CommandResult::Observation(cors_page)) = cors_observation.result.as_ref() else {
        return Err("CORS readback returned the wrong result kind".into());
    };
    let cors_text = cors_page
        .items
        .iter()
        .filter_map(|item| match item {
            ObservedItem::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .concat();
    if !cors_text.contains("CORS allowed λ payload")
        || !cors_text.contains("CORS denied")
        || cors_text.contains("CORS secret body")
    {
        return Err(format!("CORS DOM readback was incorrect: {cors_text:?}").into());
    }
    let cors_network_reply = run_rwb(&rwb, &socket, &["--tab", cors_tab_argument, "network"])?;
    let Some(CommandResult::Network(cors_records)) = cors_network_reply.result.as_ref() else {
        return Err("CORS network readback returned the wrong result kind".into());
    };
    let cors_allowed_url = format!("http://{cors_address}/cors-allowed");
    let cors_denied_url = format!("http://{cors_address}/cors-denied");
    let [allowed_record, denied_record] = cors_records.as_slice() else {
        return Err(format!(
            "CORS network returned {} records instead of two",
            cors_records.len()
        )
        .into());
    };
    if allowed_record.requested_url != cors_allowed_url
        || allowed_record.final_url.as_deref() != Some(cors_allowed_url.as_str())
        || allowed_record.status != Some(200)
        || allowed_record.error.is_some()
        || denied_record.requested_url != cors_denied_url
        || denied_record.final_url.as_deref() != Some(cors_denied_url.as_str())
        || denied_record.status != Some(200)
        || !denied_record
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Access-Control-Allow-Origin"))
    {
        return Err(format!("CORS network records were incorrect: {cors_records:?}").into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", cors_tab_argument])?;

    let external_script_url = format!("http://{address}/external-script-page");
    let external_script_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &external_script_url])?;
    let external_script_tab = external_script_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("external-script tab creation did not return a tab ID")?;
    let external_script_tab_argument = external_script_tab.as_str();
    let external_script_observation = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            external_script_tab_argument,
            "observe",
            "--mode",
            "text",
        ],
    )?;
    let Some(CommandResult::Observation(external_script_page)) =
        external_script_observation.result.as_ref()
    else {
        return Err("external-script readback returned the wrong result kind".into());
    };
    if !external_script_page.items.iter().any(
        |item| matches!(item, ObservedItem::Text(text) if text.text == "External script loaded λ"),
    ) {
        return Err("packaged external script did not produce the expected DOM mutation".into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["tab", "close", external_script_tab_argument],
    )?;

    let selector_url = format!("http://{address}/selector-page");
    let selector_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &selector_url])?;
    let selector_tab = selector_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("selector tab creation did not return a tab ID")?;
    let selector_tab_argument = selector_tab.as_str();
    let selector_observation = run_rwb(
        &rwb,
        &socket,
        &["--tab", selector_tab_argument, "observe", "--mode", "text"],
    )?;
    let Some(CommandResult::Observation(selector_page)) = selector_observation.result.as_ref()
    else {
        return Err("selector readback returned the wrong result kind".into());
    };
    if !selector_page
        .items
        .iter()
        .any(|item| matches!(item, ObservedItem::Text(text) if text.text == "Selector matched λ"))
    {
        return Err("packaged querySelector did not produce the expected DOM mutation".into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", selector_tab_argument])?;

    let console_url = format!("http://{address}/console-page");
    let console_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &console_url])?;
    let console_tab = console_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("console tab creation did not return a tab ID")?;
    let console_tab_argument = console_tab.as_str();
    let console_reply = run_rwb(&rwb, &socket, &["--tab", console_tab_argument, "console"])?;
    if console_reply.receipt.as_ref().map(|receipt| receipt.effect) != Some(EffectState::None) {
        return Err("console did not report read-only effect none".into());
    }
    let Some(CommandResult::Console(records)) = console_reply.result.as_ref() else {
        return Err("console returned the wrong result kind".into());
    };
    let first = records.first().ok_or("console returned no records")?;
    let last = records.last().ok_or("console returned no records")?;
    if records.len() != 256
        || first.sequence != 5
        || first.level != browser_protocol::ConsoleLevel::Log
        || first.text != "entry-5"
        || last.sequence != 260
        || last.level != browser_protocol::ConsoleLevel::Warn
        || last.text != "tail λ {\"ok\":true}"
    {
        return Err(format!(
            "console retention or structured record contract was incorrect: count={} first={first:?} last={last:?}",
            records.len()
        )
        .into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", console_tab_argument])?;

    let render_url = format!("http://{address}/render-page");
    let render_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &render_url])?;
    let render_tab = render_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("render tab creation did not return a tab ID")?;
    let render_tab_argument = render_tab.as_str();
    let frame_reply = run_rwb(
        &rwb,
        &socket,
        &["--tab", render_tab_argument, "render-frame"],
    )?;
    if frame_reply.receipt.as_ref().map(|receipt| receipt.effect) != Some(EffectState::None) {
        return Err("render-frame did not report read-only effect none".into());
    }
    let Some(CommandResult::Frame(frame)) = frame_reply.result.as_ref() else {
        return Err("render-frame returned the wrong result kind".into());
    };
    if frame.tab_id != render_tab
        || frame.width != 800
        || frame.height != 600
        || frame.pixel_scale != 1.0
    {
        return Err(format!("render-frame metadata was incorrect: {frame:?}").into());
    }
    let FramePixels::Owned {
        format,
        stride,
        bytes,
    } = &frame.pixels
    else {
        return Err("render-frame did not return owned pixels".into());
    };
    if *format != PixelFormat::Rgba8Premultiplied
        || *stride != 3_200
        || bytes.len() != 800 * 600 * 4
    {
        return Err("render-frame pixel contract was incorrect".into());
    }
    let orange_pixels = bytes
        .chunks_exact(4)
        .filter(|pixel| *pixel == [204, 85, 17, 255])
        .count();
    let blue_pixels = bytes
        .chunks_exact(4)
        .filter(|pixel| *pixel == [17, 34, 51, 255])
        .count();
    let decoded_image_pixels = bytes
        .chunks_exact(4)
        .filter(|pixel| *pixel == [255, 0, 0, 255])
        .count();
    // Vello antialiases glyph coverage into the page background, so edge pixels need not
    // equal the CSS color byte-for-byte. Count pixels on the independently expected
    // yellow-to-blue blend line while excluding both solid fixture backgrounds.
    let glyph_pixels = bytes
        .chunks_exact(4)
        .filter(|pixel| {
            let [red, green, blue, alpha] = **pixel else {
                unreachable!("RGBA chunks always contain four bytes")
            };
            alpha == 255
                && red >= 40
                && green >= 50
                && blue >= 45
                && red > blue
                && green > blue
                && red.saturating_sub(green) < 80
        })
        .count();
    if orange_pixels != 64 * 48
        || decoded_image_pixels != 40 * 30
        || blue_pixels < 300_000
        || glyph_pixels < 20
    {
        return Err(format!(
            "document frame did not paint the independently expected regions: orange={orange_pixels} image={decoded_image_pixels} blue={blue_pixels} glyph={glyph_pixels}"
        )
        .into());
    }

    let control_label_url = format!("http://{address}/control-label-render");
    let control_label_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &control_label_url])?;
    let control_label_tab = control_label_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("control-label render tab creation did not return a tab ID")?;
    let control_label_tab_argument = control_label_tab.as_str();
    let control_label_frame = run_rwb(
        &rwb,
        &socket,
        &["--tab", control_label_tab_argument, "render-frame"],
    )?;
    let Some(CommandResult::Frame(frame)) = control_label_frame.result.as_ref() else {
        return Err("control-label render returned the wrong result kind".into());
    };
    let FramePixels::Owned { bytes, .. } = &frame.pixels else {
        return Err("control-label render did not return owned pixels".into());
    };
    let visible_label_pixels = (0..80_usize)
        .flat_map(|y| (0..240_usize).map(move |x| (y * frame.width as usize + x) * 4))
        .filter(|offset| {
            let pixel = &bytes[*offset..*offset + 4];
            pixel[3] == 255 && pixel[0] < 180 && pixel[1] < 190 && pixel[2] < 210
        })
        .count();
    if visible_label_pixels < 40 {
        return Err(format!(
            "value-backed submit control did not paint its visible label: observed {visible_label_pixels} independently distinguishable label pixels"
        )
        .into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", control_label_tab_argument])?;

    let responsive_url = format!("http://{address}/responsive-render");
    let responsive_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &responsive_url])?;
    let responsive_tab = responsive_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("responsive render tab creation did not return a tab ID")?;
    let responsive_tab_argument = responsive_tab.as_str();
    for (width, height, expected_orange) in [(1_000_u32, 300_u32, 50_000_usize), (400, 300, 20_000)]
    {
        let width_argument = width.to_string();
        let height_argument = height.to_string();
        let reply = run_rwb(
            &rwb,
            &socket,
            &[
                "--tab",
                responsive_tab_argument,
                "render-frame",
                "--width",
                &width_argument,
                "--height",
                &height_argument,
            ],
        )?;
        let Some(CommandResult::Frame(frame)) = reply.result.as_ref() else {
            return Err("viewport render returned the wrong result kind".into());
        };
        if frame.width != width || frame.height != height {
            return Err(format!("viewport render metadata was incorrect: {frame:?}").into());
        }
        let FramePixels::Owned { bytes, .. } = &frame.pixels else {
            return Err("viewport render did not return owned pixels".into());
        };
        let orange = bytes
            .chunks_exact(4)
            .filter(|pixel| *pixel == [204, 85, 17, 255])
            .count();
        let green_border = bytes
            .chunks_exact(4)
            .filter(|pixel| *pixel == [51, 204, 102, 255])
            .count();
        let pixel_at = |x: usize, y: usize| {
            let offset = (y * width as usize + x) * 4;
            &bytes[offset..offset + 4]
        };
        let border_geometry_matches = pixel_at(0, 100) == [51, 204, 102, 255]
            && pixel_at(127, 100) == [51, 204, 102, 255]
            && pixel_at(0, 167) == [51, 204, 102, 255]
            && pixel_at(127, 167) == [51, 204, 102, 255]
            && pixel_at(4, 104) == [17, 34, 51, 255]
            && pixel_at(123, 163) == [17, 34, 51, 255];
        if orange != expected_orange {
            return Err(format!(
                "responsive layout did not resolve 50% against viewport {width}x{height}: expected {expected_orange} orange pixels, observed {orange}"
            )
            .into());
        }
        if green_border != 1_504 || !border_geometry_matches {
            return Err(format!(
                "responsive layout did not paint the independently expected transparent 120x60 box with a 4px border at viewport {width}x{height}: expected 1504 green border pixels, observed {green_border}, geometry_match={border_geometry_matches}"
            )
            .into());
        }
    }
    run_rwb(&rwb, &socket, &["tab", "close", responsive_tab_argument])?;

    let responsive_image_url = format!("http://{address}/responsive-image-page");
    let responsive_image_tab_reply =
        run_rwb(&rwb, &socket, &["tab", "new", &responsive_image_url])?;
    let responsive_image_tab = responsive_image_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("responsive image tab creation did not return a tab ID")?;
    let responsive_image_tab_argument = responsive_image_tab.as_str();
    let responsive_image_frame = run_rwb(
        &rwb,
        &socket,
        &["--tab", responsive_image_tab_argument, "render-frame"],
    )?;
    let Some(CommandResult::Frame(frame)) = responsive_image_frame.result.as_ref() else {
        return Err("responsive image render returned the wrong result kind".into());
    };
    let FramePixels::Owned { bytes, .. } = &frame.pixels else {
        return Err("responsive image render did not return owned pixels".into());
    };
    let responsive_red_pixels = bytes
        .chunks_exact(4)
        .filter(|pixel| *pixel == [255, 0, 0, 255])
        .count();
    if responsive_red_pixels != 40 * 30 {
        return Err(format!(
            "responsive image did not select, fetch, decode, and paint the independently expected 800w candidate: expected 1200 red pixels, observed {responsive_red_pixels}"
        )
        .into());
    }
    run_rwb(
        &rwb,
        &socket,
        &["tab", "close", responsive_image_tab_argument],
    )?;

    let coordinate_url = format!("http://{address}/coordinate-click");
    let coordinate_tab_reply = run_rwb(&rwb, &socket, &["tab", "new", &coordinate_url])?;
    let coordinate_tab = coordinate_tab_reply
        .target
        .as_ref()
        .and_then(|target| target.tab_id.clone())
        .ok_or("coordinate-click tab creation did not return a tab ID")?;
    let coordinate_tab_argument = coordinate_tab.as_str();
    let coordinate_before = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "observe",
            "--mode",
            "controls",
        ],
    )?;
    let (_, coordinate_before_checked) = observed_checkbox(&coordinate_before)?;
    if coordinate_before_checked {
        return Err("coordinate-click checkbox unexpectedly began checked".into());
    }
    let coordinate_frame_reply = run_rwb(
        &rwb,
        &socket,
        &["--tab", coordinate_tab_argument, "render-frame"],
    )?;
    let Some(CommandResult::Frame(coordinate_frame)) = coordinate_frame_reply.result.as_ref()
    else {
        return Err("coordinate frame returned the wrong result kind".into());
    };
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "frame-input",
            coordinate_frame.view_id.as_str(),
            "pointer",
            "up",
            "40",
            "40",
            "primary",
        ],
    )?;
    let coordinate_after_release_only = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "observe",
            "--mode",
            "controls",
        ],
    )?;
    let (_, checked_after_release_only) = observed_checkbox(&coordinate_after_release_only)?;
    if checked_after_release_only {
        return Err("pointer release without a press activated the checkbox".into());
    }

    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "frame-input",
            coordinate_frame.view_id.as_str(),
            "pointer",
            "down",
            "40",
            "40",
            "primary",
        ],
    )?;
    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "frame-input",
            coordinate_frame.view_id.as_str(),
            "pointer",
            "up",
            "120",
            "120",
            "primary",
        ],
    )?;
    let coordinate_after_mismatched_release = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "observe",
            "--mode",
            "controls",
        ],
    )?;
    let (_, checked_after_mismatched_release) =
        observed_checkbox(&coordinate_after_mismatched_release)?;
    if checked_after_mismatched_release {
        return Err("pointer release on a different target activated the checkbox".into());
    }

    run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "frame-input",
            coordinate_frame.view_id.as_str(),
            "pointer",
            "down",
            "40",
            "40",
            "primary",
        ],
    )?;
    let coordinate_frame_input = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "frame-input",
            coordinate_frame.view_id.as_str(),
            "pointer",
            "up",
            "40",
            "40",
            "primary",
        ],
    )?;
    if coordinate_frame_input
        .receipt
        .as_ref()
        .map(|receipt| (receipt.status, receipt.effect))
        != Some((OperationStatus::Completed, EffectState::Applied))
    {
        return Err(
            "matching frame pointer input did not return a completed/applied receipt".into(),
        );
    }
    let coordinate_after = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "observe",
            "--mode",
            "controls",
        ],
    )?;
    let (_, coordinate_after_checked) = observed_checkbox(&coordinate_after)?;
    if !coordinate_after_checked {
        return Err(
            "matching frame pointer press/release did not check the rendered checkbox".into(),
        );
    }
    let stale_frame_input = run_rwb_failure(
        &rwb,
        &socket,
        &[
            "--tab",
            coordinate_tab_argument,
            "frame-input",
            coordinate_frame.view_id.as_str(),
            "pointer",
            "move",
            "40",
            "40",
            "none",
        ],
    )?;
    if stale_frame_input
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::StaleTarget, EffectState::None))
    {
        return Err("invalidated frame input did not fail with STALE_TARGET/none".into());
    }
    run_rwb(&rwb, &socket, &["tab", "close", coordinate_tab_argument])?;

    let screenshot_path = daemon.directory.join("render-page.png");
    let screenshot_path_text = screenshot_path
        .to_str()
        .ok_or("temporary screenshot path was not UTF-8")?;
    let screenshot_reply = run_rwb(
        &rwb,
        &socket,
        &[
            "--tab",
            render_tab_argument,
            "screenshot",
            "--out",
            screenshot_path_text,
        ],
    )?;
    if screenshot_reply
        .receipt
        .as_ref()
        .map(|receipt| receipt.effect)
        != Some(EffectState::None)
    {
        return Err("screenshot did not report read-only effect none".into());
    }
    let Some(CommandResult::Screenshot(screenshot)) = screenshot_reply.result.as_ref() else {
        return Err("screenshot returned the wrong result kind".into());
    };
    let screenshot_bytes = fs::read(&screenshot_path)?;
    let screenshot_hash = format!("{:x}", Sha256::digest(&screenshot_bytes));
    if screenshot.path != screenshot_path_text
        || screenshot.width != 800
        || screenshot.height != 600
        || screenshot.bytes != screenshot_bytes.len() as u64
        || screenshot.format != "png"
        || screenshot.sha256 != screenshot_hash
        || !screenshot_bytes.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err(format!(
            "screenshot metadata or encoded artifact was incorrect: receipt={screenshot:?} actual_bytes={} actual_hash={screenshot_hash}",
            screenshot_bytes.len()
        )
        .into());
    }
    let decoder = png::Decoder::new(Cursor::new(&screenshot_bytes));
    let mut reader = decoder.read_info()?;
    let mut decoded_storage = vec![0; reader.output_buffer_size()];
    let decoded_info = reader.next_frame(&mut decoded_storage)?;
    let decoded = &decoded_storage[..decoded_info.buffer_size()];
    let screenshot_orange = decoded
        .chunks_exact(4)
        .filter(|pixel| *pixel == [204, 85, 17, 255])
        .count();
    let screenshot_blue = decoded
        .chunks_exact(4)
        .filter(|pixel| *pixel == [17, 34, 51, 255])
        .count();
    let screenshot_glyph = decoded
        .chunks_exact(4)
        .filter(|pixel| {
            let [red, green, blue, alpha] = **pixel else {
                unreachable!("RGBA chunks always contain four bytes")
            };
            alpha == 255
                && red >= 40
                && green >= 50
                && blue >= 45
                && red > blue
                && green > blue
                && red.saturating_sub(green) < 80
        })
        .count();
    if decoded_info.width != 800
        || decoded_info.height != 600
        || decoded_info.color_type != png::ColorType::Rgba
        || decoded_info.bit_depth != png::BitDepth::Eight
        || screenshot_orange != 64 * 48
        || screenshot_blue < 300_000
        || screenshot_glyph < 20
    {
        return Err(format!(
            "decoded screenshot did not preserve the expected frame: info={decoded_info:?} orange={screenshot_orange} blue={screenshot_blue} glyph={screenshot_glyph}"
        )
        .into());
    }
    let repeated_screenshot = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &[
            "--tab",
            render_tab_argument,
            "screenshot",
            "--out",
            screenshot_path_text,
        ],
        5,
    )?;
    if repeated_screenshot
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::Internal, EffectState::None))
        || fs::read(&screenshot_path)? != screenshot_bytes
    {
        return Err(
            "repeated screenshot overwrote the destination or returned the wrong error".into(),
        );
    }
    run_rwb(&rwb, &socket, &["tab", "close", render_tab_argument])?;

    let expired_operation = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &["operation", "status", replay_request_id],
        1,
    )?;
    if expired_operation
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::OperationExpired, EffectState::None))
        || expired_operation
            .receipt
            .as_ref()
            .map(|receipt| (receipt.status, receipt.effect))
            != Some((OperationStatus::Failed, EffectState::None))
    {
        return Err(
            "evicted operation status did not return OPERATION_EXPIRED with a read-only none receipt"
                .into(),
        );
    }
    let expired_replay = run_rwb_failure_with_status(
        &rwb,
        &socket,
        &[
            "--request-id",
            replay_request_id,
            "--tab",
            tab_argument,
            "click",
            checkbox.as_str(),
        ],
        1,
    )?;
    if expired_replay
        .error
        .as_ref()
        .map(|error| (error.code, error.effect))
        != Some((ErrorCode::OperationExpired, EffectState::None))
        || expired_replay.receipt.is_some()
    {
        return Err("expired mutating request ID was not rejected before redispatch".into());
    }

    let _ = fixture_stop_sender.send(());
    let _ = cors_fixture_stop_sender.send(());
    fixture
        .join()
        .map_err(|_| "fixture server thread panicked")?
        .map_err(|error| format!("fixture server failed: {error}"))?;
    cors_fixture
        .join()
        .map_err(|_| "CORS fixture server thread panicked")?
        .map_err(|error| format!("CORS fixture server failed: {error}"))?;
    let observed_requests = request_count.load(Ordering::SeqCst);
    if observed_requests != expected_requests {
        return Err(format!(
            "fixture observed {observed_requests} requests, expected {expected_requests}"
        )
        .into());
    }
    println!(
        "cli-journey: PASS fresh-processes={} service=restart-reconnected transport=canonical-invalid-request,unsupported-protocol,stdio-multi-request-recovery capabilities=exact-routed-surface cli-usage=complete operations=replay,conflict,status,missing,profile-isolated,deadline-before-dispatch,durable-timeout,durable-expiry,no-expired-replay events=ordered,cursor-exact,profile-isolated,same-instance-gap,restart-gap profile-auth=restart-persisted profile-storage=localStorage-restart-persisted,origin-isolated,sessionStorage-tab-scoped,origin-isolated,restart-cleared profile-tabs=isolated profile-lifecycle=create,list,close,open,remove,missing tab={} navigation=open,discover-link,click-relative-link,redirect,back,forward,reload,form-submit title=live,restart-restored observation=immutable,paginated,truthful,count-byte-age-expired,stale-on-navigation controls=discover,click,check,idempotent,fill,caret-selection-unicode,submit,readback async=external-script,selector,timer,fetch-exact-unicode,cors-allowed-denied diagnostics=console-bounded,sequenced,structured frame=metadata,owned-rgba,layout-backgrounds,input-applied,stale-view-rejected scroll=offset,top-left-clamped download=authenticated,binary,sha256,atomic,no-overwrite,cancelled-unpublishable server-request=GET+POST+unicode+submitter+exact-body stale-target=verified unicode=verified generated-cases={} seed=0x{seed:08x} headless=true",
        423 + cases * 7,
        tab,
        cases,
    );
    Ok(())
}
