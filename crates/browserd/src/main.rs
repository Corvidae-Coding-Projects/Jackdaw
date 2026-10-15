use std::{
    env, fs,
    io::{BufRead, BufReader, BufWriter, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use browser_core::{BrowserService, NavigationCancellation, ViewRetentionPolicy};
use browser_net::BrowserNetworkConfig;
use browser_protocol::{
    Command, CommandReply, CommandRequest, CommandResult, EffectState, ErrorCode, ErrorDetail,
    NavigationCommand, PROTOCOL_VERSION, ProtocolError, ReplyTarget, RequestId,
};

fn usage() -> &'static str {
    "usage: browserd (--socket PATH | serve --stdio) --profile-root PATH [--tls-ca-der PATH]... [--view-max-count COUNT] [--view-max-bytes BYTES] [--view-max-age-ms MILLISECONDS]"
}

enum Transport {
    Socket(PathBuf),
    Stdio,
}

struct Options {
    transport: Transport,
    profile_root: PathBuf,
    tls_ca_der: Vec<PathBuf>,
    view_retention: ViewRetentionPolicy,
}

fn options() -> Result<Options, String> {
    let mut arguments = env::args().skip(1).peekable();
    let serve = arguments.peek().is_some_and(|argument| argument == "serve");
    if serve {
        arguments.next();
    }
    let mut socket = None;
    let mut stdio = false;
    let mut profile_root = None;
    let mut tls_ca_der = Vec::new();
    let mut view_retention = ViewRetentionPolicy::default();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--socket" if socket.is_none() && !stdio && !serve => {
                socket = Some(PathBuf::from(
                    arguments.next().ok_or_else(|| usage().to_owned())?,
                ));
            }
            "--stdio" if !stdio && socket.is_none() && serve => stdio = true,
            "--profile-root" if profile_root.is_none() => {
                profile_root = Some(PathBuf::from(
                    arguments.next().ok_or_else(|| usage().to_owned())?,
                ));
            }
            "--tls-ca-der" => {
                tls_ca_der.push(PathBuf::from(
                    arguments.next().ok_or_else(|| usage().to_owned())?,
                ));
            }
            "--view-max-count" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| usage().to_owned())?
                    .parse::<usize>()
                    .map_err(|_| usage().to_owned())?;
                if value == 0 {
                    return Err("--view-max-count must be greater than zero".to_owned());
                }
                view_retention.max_count = value;
            }
            "--view-max-bytes" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| usage().to_owned())?
                    .parse::<usize>()
                    .map_err(|_| usage().to_owned())?;
                if value == 0 {
                    return Err("--view-max-bytes must be greater than zero".to_owned());
                }
                view_retention.max_bytes = value;
            }
            "--view-max-age-ms" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| usage().to_owned())?
                    .parse::<u64>()
                    .map_err(|_| usage().to_owned())?;
                if value == 0 {
                    return Err("--view-max-age-ms must be greater than zero".to_owned());
                }
                view_retention.max_age = Duration::from_millis(value);
            }
            _ => return Err(usage().to_owned()),
        }
    }
    let transport = match (socket, stdio) {
        (Some(socket), false) => Transport::Socket(socket),
        (None, true) => Transport::Stdio,
        _ => return Err(usage().to_owned()),
    };
    Ok(Options {
        transport,
        profile_root: profile_root.ok_or_else(|| usage().to_owned())?,
        tls_ca_der,
        view_retention,
    })
}

fn bind_socket(path: &Path) -> Result<UnixListener, Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn decode_failure(line: &str, error: &serde_json::Error) -> CommandReply {
    let envelope = serde_json::from_str::<serde_json::Value>(line).ok();
    let request_id = envelope
        .as_ref()
        .and_then(|value| value.get("request_id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|value| RequestId::parse(value).ok())
        .unwrap_or_else(|| RequestId::parse("r_transport").expect("fallback request ID is valid"));
    let code = if envelope
        .as_ref()
        .and_then(|value| value.get("protocol_version"))
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| version != u64::from(PROTOCOL_VERSION.get()))
    {
        ErrorCode::UnsupportedProtocol
    } else {
        ErrorCode::InvalidRequest
    };
    CommandReply {
        protocol_version: PROTOCOL_VERSION,
        request_id,
        success: false,
        target: None,
        receipt: None,
        result: None,
        error: Some(ProtocolError {
            code,
            message: "request could not be decoded".to_owned(),
            retryable: false,
            effect: EffectState::None,
            details: vec![ErrorDetail {
                field: None,
                reason: error.to_string(),
            }],
        }),
    }
}

struct QueuedRequest {
    request: CommandRequest,
    reply: mpsc::Sender<CommandReply>,
}

fn write_reply(
    writer: &mut impl Write,
    reply: &CommandReply,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *writer, reply)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn serve_socket_client(
    stream: UnixStream,
    requests: mpsc::Sender<QueuedRequest>,
    cancellation: NavigationCancellation,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        match serde_json::from_str::<CommandRequest>(trimmed) {
            Ok(request) => {
                let shutdown = matches!(request.command, Command::Shutdown);
                let stop_acknowledgement =
                    matches!(request.command, Command::Navigate(NavigationCommand::Stop)).then(
                        || {
                            cancellation.cancel(&request.profile_id, request.tab_id.as_ref());
                            CommandReply {
                                protocol_version: PROTOCOL_VERSION,
                                request_id: request.request_id.clone(),
                                success: true,
                                target: Some(ReplyTarget {
                                    profile_id: request.profile_id.clone(),
                                    tab_id: request.tab_id.clone(),
                                    document_id: None,
                                }),
                                receipt: None,
                                result: Some(CommandResult::Empty),
                                error: None,
                            }
                        },
                    );
                let (reply_sender, reply_receiver) = mpsc::channel();
                requests.send(QueuedRequest {
                    request,
                    reply: reply_sender,
                })?;
                if let Some(reply) = stop_acknowledgement {
                    write_reply(&mut writer, &reply)?;
                } else {
                    let reply = reply_receiver.recv()?;
                    write_reply(&mut writer, &reply)?;
                }
                if shutdown {
                    break;
                }
            }
            Err(error) => write_reply(&mut writer, &decode_failure(trimmed, &error))?,
        }
        line.clear();
    }
    Ok(())
}

fn serve_ndjson(
    service: &mut BrowserService,
    mut reader: impl BufRead,
    mut writer: impl Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let (reply, shutdown) = match serde_json::from_str::<CommandRequest>(trimmed) {
            Ok(request) => {
                let shutdown = matches!(request.command, Command::Shutdown);
                (service.handle(request), shutdown)
            }
            Err(error) => (decode_failure(trimmed, &error), false),
        };
        write_reply(&mut writer, &reply)?;
        if shutdown {
            return Ok(true);
        }
        line.clear();
    }
    Ok(false)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().nth(1).as_deref() == Some("__document-worker") {
        browser_core::run_document_worker_stdio()?;
        return Ok(());
    }
    if env::args().nth(1).as_deref() == Some("__document-worker-probe") {
        let text = browser_core::probe_document_worker_process()?;
        println!("document-worker-process: PASS text={text:?}");
        return Ok(());
    }

    let options = options()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let mut network_config = BrowserNetworkConfig::builder();
    for path in &options.tls_ca_der {
        let certificate = fs::read(path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "could not read TLS CA certificate {}: {error}",
                    path.display()
                ),
            )
        })?;
        network_config = network_config.add_root_der(certificate).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid TLS CA certificate {}: {error}", path.display()),
            )
        })?;
    }
    let network_config = network_config.build()?;
    let mut service = BrowserService::with_profile_root_view_retention_and_config(
        &options.profile_root,
        options.view_retention,
        network_config,
    );
    match options.transport {
        Transport::Socket(socket) => {
            let listener = bind_socket(&socket)?;
            eprintln!(
                "browserd: ready socket={} profile_root={}",
                socket.display(),
                options.profile_root.display()
            );
            listener.set_nonblocking(true)?;
            let cancellation = service.navigation_cancellation();
            let (request_sender, request_receiver) = mpsc::channel::<QueuedRequest>();
            let acceptor_requests = request_sender.clone();
            let acceptor_cancellation = cancellation.clone();
            thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _address)) => {
                            let requests = acceptor_requests.clone();
                            let cancellation = acceptor_cancellation.clone();
                            thread::spawn(move || {
                                if let Err(error) =
                                    serve_socket_client(stream, requests, cancellation)
                                {
                                    eprintln!("browserd: socket client failed: {error}");
                                }
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => {
                            eprintln!("browserd: socket accept failed: {error}");
                            break;
                        }
                    }
                }
            });
            let serve_result: Result<(), Box<dyn std::error::Error>> = {
                'service: loop {
                    while let Ok(queued) = request_receiver.try_recv() {
                        let shutdown = matches!(queued.request.command, Command::Shutdown);
                        let reply = service.handle(queued.request);
                        let _ = queued.reply.send(reply);
                        if shutdown {
                            break 'service;
                        }
                    }

                    if let Err(error) = service.advance_background_work() {
                        eprintln!(
                            "browserd: background browser work failed: {}",
                            error.message
                        );
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            };
            let cleanup_result = fs::remove_file(&socket);
            serve_result?;
            if let Err(error) = cleanup_result
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
        }
        Transport::Stdio => {
            eprintln!(
                "browserd: ready transport=stdio profile_root={}",
                options.profile_root.display()
            );
            let _shutdown = serve_ndjson(
                &mut service,
                std::io::stdin().lock(),
                BufWriter::new(std::io::stdout().lock()),
            )?;
        }
    }
    Ok(())
}
