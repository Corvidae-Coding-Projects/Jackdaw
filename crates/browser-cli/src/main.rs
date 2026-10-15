use std::{
    env,
    io::Read,
    path::{Path, PathBuf},
    process::{self, Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use browser_client::BrowserClient;
use browser_protocol::{
    Command, CommandReply, CommandRequest, DiagnosticCommand, DownloadCommand, DownloadId,
    ElementRef, EventCommand, EventCursor, FindCommand, FrameInput, FrameInputCommand,
    InteractionCommand, KeyPhase, NavigationCommand, ObservationCursor, ObservationMode,
    ObserveCommand, OperationCommand, PROTOCOL_VERSION, PointerButton, PointerPhase,
    ProfileCommand, ProfileId, ReadCommand, RequestId, ScreenshotCommand, ServiceInstanceId,
    TabCommand, TabId, TargetState, UploadCommand, ViewId, WaitCommand,
};

struct Options {
    socket: PathBuf,
    profile: ProfileId,
    tab: Option<TabId>,
    request_id: Option<RequestId>,
    timeout_ms: u64,
    command: Command,
}

enum Invocation {
    Request(Options),
    Start {
        socket: PathBuf,
        profile_root: PathBuf,
        browserd: PathBuf,
    },
}

fn usage() -> &'static str {
    "usage: rwb --socket PATH [--json] [--profile NAME] [--tab ID] [--request-id ID] [--timeout-ms MILLISECONDS] <daemon status|stop|version|capabilities|profile create|list|open|close|remove [NAME]|tab list|tab new [URL]|tab close ID|tab activate ID|open URL|back|forward|reload|stop|observe [--mode text|controls|links|all] [--max-items COUNT] [--cursor VIEW_ID OFFSET]|read [--max-items COUNT] [--cursor VIEW_ID OFFSET]|find QUERY [--max-items COUNT]|screenshot --out PATH|frame-input VIEW_ID pointer move|down|up|cancel X Y none|primary|auxiliary|secondary|frame-input VIEW_ID key down|up KEY [TEXT]|frame-input VIEW_ID scroll X Y DELTA_X DELTA_Y|click REF|click --at X Y|focus REF|fill REF VALUE|type REF TEXT|key REF KEY|scroll DELTA_X DELTA_Y|select REF VALUE [VALUE...]|check REF true|false|submit REF|wait idle|url URL|text VALUE|role ROLE NAME|target REF attached|detached|enabled|disabled|focused|checked|unchecked|download ID|upload REF --file PATH [--file PATH...]|download start URL|download list|download wait --id ID --out PATH|download cancel --id ID|operation status REQUEST_ID|events [--cursor SERVICE_INSTANCE_ID SEQUENCE] [--max-events COUNT]|render-frame [--width WIDTH --height HEIGHT]|console|network|eval --script-stdin>"
}

fn normalize_profile(value: &str) -> Result<ProfileId, String> {
    let value = if value.starts_with("p_") {
        value.to_owned()
    } else {
        format!("p_{value}")
    };
    ProfileId::parse(value).map_err(|error| error.to_string())
}

fn parse_options() -> Result<Invocation, String> {
    let mut arguments = env::args().skip(1).peekable();
    let mut socket = env::var_os("RWB_SOCKET").map(PathBuf::from);
    let mut profile = normalize_profile("default")?;
    let mut tab = None;
    let mut request_id = None;
    let mut timeout_ms = 30_000;

    while let Some(argument) = arguments.peek().cloned() {
        match argument.as_str() {
            "--socket" => {
                arguments.next();
                socket = Some(PathBuf::from(
                    arguments.next().ok_or("--socket requires a path")?,
                ));
            }
            "--profile" => {
                arguments.next();
                profile = normalize_profile(&arguments.next().ok_or("--profile requires a name")?)?;
            }
            "--tab" => {
                arguments.next();
                tab = Some(
                    TabId::parse(arguments.next().ok_or("--tab requires an ID")?)
                        .map_err(|error| error.to_string())?,
                );
            }
            "--request-id" => {
                arguments.next();
                request_id = Some(
                    RequestId::parse(arguments.next().ok_or("--request-id requires an ID")?)
                        .map_err(|error| error.to_string())?,
                );
            }
            "--timeout-ms" => {
                arguments.next();
                timeout_ms = arguments
                    .next()
                    .ok_or("--timeout-ms requires a value")?
                    .parse()
                    .map_err(|_| "--timeout-ms requires an unsigned integer")?;
            }
            "--json" => {
                arguments.next();
            }
            _ => break,
        }
    }

    let verb = arguments.next().ok_or_else(|| usage().to_owned())?;
    if verb == "daemon" && arguments.peek().is_some_and(|value| value == "start") {
        arguments.next();
        let socket = socket.ok_or("--socket or RWB_SOCKET is required")?;
        let mut profile_root = socket
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("profiles");
        let mut browserd = env::current_exe()
            .map_err(|error| format!("could not locate rwb executable: {error}"))?;
        browserd.set_file_name("browserd");
        while let Some(option) = arguments.next() {
            match option.as_str() {
                "--profile-root" => {
                    profile_root = PathBuf::from(
                        arguments
                            .next()
                            .ok_or("daemon start --profile-root requires a path")?,
                    );
                }
                "--browserd" => {
                    browserd = PathBuf::from(
                        arguments
                            .next()
                            .ok_or("daemon start --browserd requires a path")?,
                    );
                }
                _ => return Err(usage().to_owned()),
            }
        }
        return Ok(Invocation::Start {
            socket,
            profile_root,
            browserd,
        });
    }
    let command = match verb.as_str() {
        "daemon" => match arguments.next().as_deref() {
            Some("status") => Command::Status,
            Some("stop") => Command::Shutdown,
            _ => return Err(usage().to_owned()),
        },
        "version" => Command::Version,
        "capabilities" => Command::Capabilities,
        "profile" => match arguments.next().as_deref() {
            Some("create") => Command::Profile(ProfileCommand::Create {
                profile: normalize_profile(
                    &arguments.next().ok_or("profile create requires a name")?,
                )?,
            }),
            Some("list") => Command::Profile(ProfileCommand::List),
            Some("open") => Command::Profile(ProfileCommand::Open {
                profile: normalize_profile(
                    &arguments.next().ok_or("profile open requires a name")?,
                )?,
            }),
            Some("close") => Command::Profile(ProfileCommand::Close {
                profile: normalize_profile(
                    &arguments.next().ok_or("profile close requires a name")?,
                )?,
            }),
            Some("remove") => Command::Profile(ProfileCommand::Remove {
                profile: normalize_profile(
                    &arguments.next().ok_or("profile remove requires a name")?,
                )?,
            }),
            _ => return Err(usage().to_owned()),
        },
        "tab" => match arguments.next().as_deref() {
            Some("list") => Command::Tab(TabCommand::List),
            Some("new") => Command::Tab(TabCommand::New {
                url: arguments.next(),
            }),
            Some("close") => Command::Tab(TabCommand::Close {
                tab: TabId::parse(arguments.next().ok_or("tab close requires an ID")?)
                    .map_err(|error| error.to_string())?,
            }),
            Some("activate") => Command::Tab(TabCommand::Activate {
                tab: TabId::parse(arguments.next().ok_or("tab activate requires an ID")?)
                    .map_err(|error| error.to_string())?,
            }),
            _ => return Err(usage().to_owned()),
        },
        "open" => Command::Navigate(NavigationCommand::Open {
            url: arguments.next().ok_or("open requires a URL")?,
        }),
        "back" => Command::Navigate(NavigationCommand::Back),
        "forward" => Command::Navigate(NavigationCommand::Forward),
        "reload" => Command::Navigate(NavigationCommand::Reload),
        "stop" => Command::Navigate(NavigationCommand::Stop),
        "observe" => {
            let mut mode = ObservationMode::Text;
            let mut cursor = None;
            let mut max_items = 256;
            while let Some(option) = arguments.peek().cloned() {
                match option.as_str() {
                    "--mode" => {
                        arguments.next();
                        mode = match arguments.next().as_deref() {
                            Some("text") => ObservationMode::Text,
                            Some("controls") => ObservationMode::Controls,
                            Some("links") => ObservationMode::Links,
                            Some("all") => ObservationMode::All,
                            _ => {
                                return Err(
                                    "--mode requires text, controls, links, or all".to_owned()
                                );
                            }
                        };
                    }
                    "--max-items" => {
                        arguments.next();
                        max_items = arguments
                            .next()
                            .ok_or("--max-items requires a count")?
                            .parse()
                            .map_err(|_| "--max-items requires an unsigned integer")?;
                    }
                    "--cursor" => {
                        arguments.next();
                        let view_id =
                            ViewId::parse(arguments.next().ok_or("--cursor requires a view ID")?)
                                .map_err(|error| error.to_string())?;
                        let offset = arguments
                            .next()
                            .ok_or("--cursor requires an offset")?
                            .parse()
                            .map_err(|_| "--cursor offset requires an unsigned integer")?;
                        cursor = Some(ObservationCursor { view_id, offset });
                    }
                    _ => break,
                }
            }
            Command::Observe(ObserveCommand {
                mode,
                frame_id: None,
                cursor,
                max_items,
            })
        }
        "read" => {
            let mut cursor = None;
            let mut max_items = 256;
            while let Some(option) = arguments.peek().cloned() {
                match option.as_str() {
                    "--max-items" => {
                        arguments.next();
                        max_items = arguments
                            .next()
                            .ok_or("--max-items requires a count")?
                            .parse()
                            .map_err(|_| "--max-items requires an unsigned integer")?;
                    }
                    "--cursor" => {
                        arguments.next();
                        let view_id =
                            ViewId::parse(arguments.next().ok_or("--cursor requires a view ID")?)
                                .map_err(|error| error.to_string())?;
                        let offset = arguments
                            .next()
                            .ok_or("--cursor requires an offset")?
                            .parse()
                            .map_err(|_| "--cursor offset requires an unsigned integer")?;
                        cursor = Some(ObservationCursor { view_id, offset });
                    }
                    _ => break,
                }
            }
            Command::Read(ReadCommand {
                frame_id: None,
                cursor,
                max_items,
            })
        }
        "find" => {
            let query = arguments.next().ok_or("find requires a query")?;
            let mut max_items = 256;
            while let Some(option) = arguments.peek().cloned() {
                match option.as_str() {
                    "--max-items" => {
                        arguments.next();
                        max_items = arguments
                            .next()
                            .ok_or("--max-items requires a count")?
                            .parse()
                            .map_err(|_| "--max-items requires an unsigned integer")?;
                    }
                    _ => break,
                }
            }
            Command::Find(FindCommand {
                query,
                frame_id: None,
                max_items,
            })
        }
        "screenshot" => {
            if arguments.next().as_deref() != Some("--out") {
                return Err("screenshot requires --out PATH".to_owned());
            }
            Command::Screenshot(ScreenshotCommand {
                output_path: arguments.next().ok_or("screenshot requires --out PATH")?,
            })
        }
        "frame-input" => {
            let view_id = ViewId::parse(arguments.next().ok_or("frame-input requires a view ID")?)
                .map_err(|error| error.to_string())?;
            let input = match arguments.next().as_deref() {
                Some("pointer") => {
                    let phase = match arguments.next().as_deref() {
                        Some("move") => PointerPhase::Move,
                        Some("down") => PointerPhase::Down,
                        Some("up") => PointerPhase::Up,
                        Some("cancel") => PointerPhase::Cancel,
                        _ => {
                            return Err(
                                "frame-input pointer requires move, down, up, or cancel".to_owned()
                            );
                        }
                    };
                    let x = arguments
                        .next()
                        .ok_or("frame-input pointer requires X Y BUTTON")?
                        .parse()
                        .map_err(|_| "frame-input pointer coordinates must be numbers")?;
                    let y = arguments
                        .next()
                        .ok_or("frame-input pointer requires X Y BUTTON")?
                        .parse()
                        .map_err(|_| "frame-input pointer coordinates must be numbers")?;
                    let button = match arguments.next().as_deref() {
                        Some("none") => None,
                        Some("primary") => Some(PointerButton::Primary),
                        Some("auxiliary") => Some(PointerButton::Auxiliary),
                        Some("secondary") => Some(PointerButton::Secondary),
                        _ => return Err(
                            "frame-input pointer requires none, primary, auxiliary, or secondary"
                                .to_owned(),
                        ),
                    };
                    FrameInput::Pointer {
                        phase,
                        x,
                        y,
                        button,
                    }
                }
                Some("key") => {
                    let phase = match arguments.next().as_deref() {
                        Some("down") => KeyPhase::Down,
                        Some("up") => KeyPhase::Up,
                        _ => return Err("frame-input key requires down or up".to_owned()),
                    };
                    let key = arguments.next().ok_or("frame-input key requires a key")?;
                    let text = arguments.next();
                    FrameInput::Key { phase, key, text }
                }
                Some("scroll") => {
                    let mut number = |name: &str| {
                        arguments
                            .next()
                            .ok_or_else(|| format!("frame-input scroll requires {name}"))?
                            .parse::<f32>()
                            .map_err(|_| format!("frame-input scroll {name} must be a number"))
                    };
                    FrameInput::Scroll {
                        x: number("X")?,
                        y: number("Y")?,
                        delta_x: number("DELTA_X")?,
                        delta_y: number("DELTA_Y")?,
                    }
                }
                _ => return Err("frame-input requires pointer, key, or scroll".to_owned()),
            };
            Command::FrameInput(FrameInputCommand { view_id, input })
        }
        "click" => {
            let target_or_at = arguments
                .next()
                .ok_or("click requires a reference or --at X Y")?;
            if target_or_at == "--at" {
                let x = arguments
                    .next()
                    .ok_or("click --at requires X Y")?
                    .parse::<i32>()
                    .map_err(|_| "click --at requires integer X Y")?;
                let y = arguments
                    .next()
                    .ok_or("click --at requires X Y")?
                    .parse::<i32>()
                    .map_err(|_| "click --at requires integer X Y")?;
                Command::Interact(InteractionCommand::ClickAt { x, y })
            } else {
                Command::Interact(InteractionCommand::Click {
                    target: ElementRef::parse(target_or_at).map_err(|error| error.to_string())?,
                })
            }
        }
        "focus" => Command::Interact(InteractionCommand::Focus {
            target: ElementRef::parse(arguments.next().ok_or("focus requires a reference")?)
                .map_err(|error| error.to_string())?,
        }),
        "fill" => Command::Interact(InteractionCommand::Fill {
            target: ElementRef::parse(arguments.next().ok_or("fill requires a reference")?)
                .map_err(|error| error.to_string())?,
            value: arguments.next().ok_or("fill requires a value")?,
        }),
        "type" => Command::Interact(InteractionCommand::Type {
            target: ElementRef::parse(arguments.next().ok_or("type requires a reference")?)
                .map_err(|error| error.to_string())?,
            text: arguments.next().ok_or("type requires text")?,
        }),
        "key" => Command::Interact(InteractionCommand::Key {
            target: Some(
                ElementRef::parse(arguments.next().ok_or("key requires a reference")?)
                    .map_err(|error| error.to_string())?,
            ),
            key: arguments.next().ok_or("key requires a key")?,
            modifiers: arguments.by_ref().collect(),
        }),
        "scroll" => {
            let delta_x = arguments
                .next()
                .ok_or("scroll requires a horizontal delta")?
                .parse()
                .map_err(|_| "scroll horizontal delta requires a signed integer")?;
            let delta_y = arguments
                .next()
                .ok_or("scroll requires a vertical delta")?
                .parse()
                .map_err(|_| "scroll vertical delta requires a signed integer")?;
            Command::Interact(InteractionCommand::Scroll {
                target: None,
                delta_x,
                delta_y,
            })
        }
        "select" => {
            let target = ElementRef::parse(arguments.next().ok_or("select requires a reference")?)
                .map_err(|error| error.to_string())?;
            let values = arguments.by_ref().collect::<Vec<_>>();
            if values.is_empty() {
                return Err("select requires at least one value".to_owned());
            }
            Command::Interact(InteractionCommand::Select { target, values })
        }
        "check" => {
            let target = ElementRef::parse(arguments.next().ok_or("check requires a reference")?)
                .map_err(|error| error.to_string())?;
            let checked = match arguments.next().as_deref() {
                Some("true") => true,
                Some("false") => false,
                _ => return Err("check requires true or false".to_owned()),
            };
            Command::Interact(InteractionCommand::Check { target, checked })
        }
        "submit" => Command::Interact(InteractionCommand::Submit {
            target: ElementRef::parse(arguments.next().ok_or("submit requires a reference")?)
                .map_err(|error| error.to_string())?,
        }),
        "wait" => match arguments.next().as_deref() {
            Some("idle") => Command::Wait(WaitCommand::Idle),
            Some("url") => Command::Wait(WaitCommand::Url {
                url: arguments.next().ok_or("wait url requires a URL")?,
            }),
            Some("text") => Command::Wait(WaitCommand::Text {
                value: arguments.next().ok_or("wait text requires a value")?,
            }),
            Some("role") => Command::Wait(WaitCommand::RoleName {
                role: arguments.next().ok_or("wait role requires a role")?,
                name: arguments.next().ok_or("wait role requires a name")?,
            }),
            Some("target") => {
                let target =
                    ElementRef::parse(arguments.next().ok_or("wait target requires a reference")?)
                        .map_err(|error| error.to_string())?;
                let state = match arguments.next().as_deref() {
                    Some("attached") => TargetState::Attached,
                    Some("detached") => TargetState::Detached,
                    Some("enabled") => TargetState::Enabled,
                    Some("disabled") => TargetState::Disabled,
                    Some("focused") => TargetState::Focused,
                    Some("checked") => TargetState::Checked,
                    Some("unchecked") => TargetState::Unchecked,
                    _ => {
                        return Err("wait target requires attached, detached, enabled, disabled, focused, checked, or unchecked".to_owned());
                    }
                };
                Command::Wait(WaitCommand::TargetState { target, state })
            }
            Some("download") => Command::Wait(WaitCommand::Download {
                download_id: DownloadId::parse(
                    arguments.next().ok_or("wait download requires an ID")?,
                )
                .map_err(|error| error.to_string())?,
            }),
            _ => return Err(usage().to_owned()),
        },
        "upload" => {
            let target = ElementRef::parse(arguments.next().ok_or("upload requires a reference")?)
                .map_err(|error| error.to_string())?;
            let mut paths = Vec::new();
            while arguments.peek().is_some() {
                if arguments.next().as_deref() != Some("--file") {
                    return Err("upload files must be supplied as --file PATH".to_owned());
                }
                paths.push(arguments.next().ok_or("--file requires a path")?);
            }
            if paths.is_empty() {
                return Err("upload requires at least one --file PATH".to_owned());
            }
            Command::Upload(UploadCommand { target, paths })
        }
        "operation" => match arguments.next().as_deref() {
            Some("status") => Command::Operation(OperationCommand::Get {
                request_id: RequestId::parse(
                    arguments
                        .next()
                        .ok_or("operation status requires a request ID")?,
                )
                .map_err(|error| error.to_string())?,
            }),
            _ => return Err(usage().to_owned()),
        },
        "events" => {
            let mut cursor = None;
            let mut max_events = 256;
            while let Some(option) = arguments.peek().cloned() {
                match option.as_str() {
                    "--cursor" => {
                        arguments.next();
                        let service_instance_id = ServiceInstanceId::parse(
                            arguments
                                .next()
                                .ok_or("--cursor requires a service instance ID")?,
                        )
                        .map_err(|error| error.to_string())?;
                        let sequence = arguments
                            .next()
                            .ok_or("--cursor requires a sequence")?
                            .parse()
                            .map_err(|_| "--cursor sequence requires an unsigned integer")?;
                        cursor = Some(EventCursor {
                            service_instance_id,
                            sequence,
                        });
                    }
                    "--max-events" => {
                        arguments.next();
                        max_events = arguments
                            .next()
                            .ok_or("--max-events requires a count")?
                            .parse()
                            .map_err(|_| "--max-events requires an unsigned integer")?;
                    }
                    _ => break,
                }
            }
            Command::Events(EventCommand { cursor, max_events })
        }
        "render-frame" => {
            let Some(first) = arguments.next() else {
                return Ok(Invocation::Request(Options {
                    socket: socket.ok_or("--socket PATH or RWB_SOCKET is required")?,
                    profile,
                    tab,
                    request_id,
                    timeout_ms,
                    command: Command::Diagnostic(DiagnosticCommand::RenderFrame),
                }));
            };
            if first != "--width" {
                return Err("render-frame accepts --width WIDTH --height HEIGHT".to_owned());
            }
            let width = arguments
                .next()
                .ok_or("--width requires an unsigned integer")?
                .parse()
                .map_err(|_| "--width requires an unsigned integer")?;
            if arguments.next().as_deref() != Some("--height") {
                return Err("render-frame --width requires --height HEIGHT".to_owned());
            }
            let height = arguments
                .next()
                .ok_or("--height requires an unsigned integer")?
                .parse()
                .map_err(|_| "--height requires an unsigned integer")?;
            if arguments.next().is_some() {
                return Err("render-frame accepts only --width WIDTH --height HEIGHT".to_owned());
            }
            Command::Diagnostic(DiagnosticCommand::RenderFrameViewport { width, height })
        }
        "console" => Command::Diagnostic(DiagnosticCommand::Console),
        "network" => Command::Diagnostic(DiagnosticCommand::Network),
        "eval" => {
            if arguments.next().as_deref() != Some("--script-stdin") {
                return Err("eval requires --script-stdin".to_owned());
            }
            let mut source = String::new();
            std::io::stdin()
                .read_to_string(&mut source)
                .map_err(|error| format!("could not read script from stdin: {error}"))?;
            if source.is_empty() {
                return Err("eval --script-stdin requires a non-empty script".to_owned());
            }
            Command::Diagnostic(DiagnosticCommand::Evaluate { source })
        }
        "download" => match arguments.next().as_deref() {
            Some("start") => Command::Download(DownloadCommand::Start {
                url: arguments.next().ok_or("download start requires a URL")?,
            }),
            Some("list") => Command::Download(DownloadCommand::List),
            Some("wait") => {
                if arguments.next().as_deref() != Some("--id") {
                    return Err("download wait requires --id ID".to_owned());
                }
                let download_id =
                    DownloadId::parse(arguments.next().ok_or("download wait requires --id ID")?)
                        .map_err(|error| error.to_string())?;
                if arguments.next().as_deref() != Some("--out") {
                    return Err("download wait requires --out PATH".to_owned());
                }
                Command::Download(DownloadCommand::Wait {
                    download_id,
                    output_path: arguments
                        .next()
                        .ok_or("download wait requires --out PATH")?,
                })
            }
            Some("cancel") => {
                if arguments.next().as_deref() != Some("--id") {
                    return Err("download cancel requires --id ID".to_owned());
                }
                Command::Download(DownloadCommand::Cancel {
                    download_id: DownloadId::parse(
                        arguments.next().ok_or("download cancel requires --id ID")?,
                    )
                    .map_err(|error| error.to_string())?,
                })
            }
            _ => return Err(usage().to_owned()),
        },
        _ => return Err(usage().to_owned()),
    };
    if arguments.next().is_some() {
        return Err("unexpected trailing argument".to_owned());
    }
    Ok(Invocation::Request(Options {
        socket: socket.ok_or("--socket or RWB_SOCKET is required")?,
        profile,
        tab,
        request_id,
        timeout_ms,
        command,
    }))
}

fn request_id() -> Result<RequestId, String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    RequestId::parse(format!("r_{}_{nonce}", process::id())).map_err(|error| error.to_string())
}

fn send_request(
    socket: &Path,
    request: &CommandRequest,
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    Ok(BrowserClient::new(socket).send(request)?)
}

fn status_request() -> Result<CommandRequest, String> {
    Ok(CommandRequest {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id()?,
        profile_id: normalize_profile("default")?,
        tab_id: None,
        command: Command::Status,
        timeout_ms: 5_000,
    })
}

fn start_daemon(
    socket: &Path,
    profile_root: &Path,
    browserd: &Path,
) -> Result<CommandReply, Box<dyn std::error::Error>> {
    if let Ok(reply) = send_request(socket, &status_request()?)
        && reply.success
    {
        return Err(format!("browserd is already running at {}", socket.display()).into());
    }

    let mut child = ProcessCommand::new(browserd)
        .arg("--socket")
        .arg(socket)
        .arg("--profile-root")
        .arg(profile_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("browserd exited before readiness: {status}").into());
        }
        if let Ok(reply) = send_request(socket, &status_request()?)
            && reply.success
        {
            return Ok(reply);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("browserd did not become ready within five seconds".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let invocation = parse_options()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let reply = match invocation {
        Invocation::Start {
            socket,
            profile_root,
            browserd,
        } => start_daemon(&socket, &profile_root, &browserd)?,
        Invocation::Request(options) => {
            let request = CommandRequest {
                protocol_version: PROTOCOL_VERSION,
                request_id: options
                    .request_id
                    .unwrap_or(request_id().map_err(|message| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
                    })?),
                profile_id: options.profile,
                tab_id: options.tab,
                command: options.command,
                timeout_ms: options.timeout_ms,
            };
            send_request(&options.socket, &request)?
        }
    };
    serde_json::to_writer(std::io::stdout().lock(), &reply)?;
    println!();
    Ok(reply
        .error
        .as_ref()
        .map_or(0, |error| i32::from(error.exit_status())))
}

fn main() {
    match run() {
        Ok(status) => process::exit(status),
        Err(error) => {
            eprintln!("rwb: {error}");
            process::exit(5);
        }
    }
}
