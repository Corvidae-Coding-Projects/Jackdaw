use std::{
    error::Error,
    fmt,
    path::PathBuf,
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use browser_client::BrowserClient;
use browser_protocol::{
    Command, CommandReply, CommandRequest, CommandResult, DiagnosticCommand, Frame, FrameInput,
    FrameInputCommand, NavigationCommand, PROTOCOL_VERSION, ProfileId, RequestId, TabCommand,
    TabId, TabSummary, ViewId,
};

// Full-viewport RGBA frames are encoded as JSON byte arrays on protocol v1,
// so their wire representation can exceed the general client's 16 MiB cap.
// Keep the larger bound local to the frame-consuming GUI session.
const GUI_MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct SessionError(String);

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SessionError {}

pub struct NavigationTask {
    socket: PathBuf,
    request: CommandRequest,
}

pub struct NavigationOutcome {
    pub url: Option<String>,
}

impl NavigationTask {
    pub fn execute(self) -> Result<NavigationOutcome, SessionError> {
        let reply = BrowserClient::new(self.socket)
            .send(&self.request)
            .map_err(|error| SessionError(error.to_string()))?;
        let reply = successful_reply(reply)?;
        let url = match reply.result {
            Some(CommandResult::Navigation(navigation)) => Some(navigation.url),
            _ => None,
        };
        Ok(NavigationOutcome { url })
    }
}

pub struct BrowserSession {
    client: BrowserClient,
    socket: PathBuf,
    profile_id: ProfileId,
    tab_id: TabId,
    request_namespace: String,
    next_request: u64,
    current_url: String,
}

impl BrowserSession {
    pub fn connect(
        socket: PathBuf,
        profile: &str,
        tab: Option<&str>,
        initial_url: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let profile_id = ProfileId::parse(profile)?;
        let client = BrowserClient::new(socket.clone()).with_max_reply_bytes(GUI_MAX_REPLY_BYTES);
        let mut session = Self {
            client,
            socket,
            profile_id,
            tab_id: TabId::parse("t_pending")?,
            request_namespace: format!(
                "{}_{}",
                process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| SessionError(format!(
                        "system clock is before Unix epoch: {error}"
                    )))?
                    .as_nanos()
            ),
            next_request: 1,
            current_url: initial_url.unwrap_or("about:blank").to_owned(),
        };

        if let Some(tab) = tab {
            session.tab_id = TabId::parse(tab)?;
            if let Some(url) = initial_url {
                session.navigate(url)?;
            }
        } else {
            let reply = session.send_without_tab(Command::Tab(TabCommand::New {
                url: initial_url.map(str::to_owned),
            }))?;
            session.tab_id = reply
                .target
                .and_then(|target| target.tab_id)
                .ok_or_else(|| {
                    SessionError("browserd created a tab without returning its ID".into())
                })?;
            if let Some(CommandResult::Navigation(navigation)) = reply.result {
                session.current_url = navigation.url;
            }
        }
        Ok(session)
    }

    pub fn current_url(&self) -> &str {
        &self.current_url
    }

    pub fn active_tab_id(&self) -> &TabId {
        &self.tab_id
    }

    pub fn tabs(&mut self) -> Result<Vec<TabSummary>, SessionError> {
        let reply = self.send_without_tab(Command::Tab(TabCommand::List))?;
        match reply.result {
            Some(CommandResult::Tabs(tabs)) => Ok(tabs),
            _ => Err(SessionError(
                "browserd returned no tab list for tab list".into(),
            )),
        }
    }

    pub fn activate_tab(&mut self, tab_id: TabId) -> Result<(), SessionError> {
        let current_url = self
            .tabs()?
            .into_iter()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.url)
            .ok_or_else(|| SessionError(format!("browserd has no tab {tab_id}")))?;
        self.send_without_tab(Command::Tab(TabCommand::Activate {
            tab: tab_id.clone(),
        }))?;
        self.tab_id = tab_id;
        self.current_url = current_url;
        Ok(())
    }

    pub fn new_tab(&mut self) -> Result<(), SessionError> {
        let reply = self.send_without_tab(Command::Tab(TabCommand::New { url: None }))?;
        let tab_id = reply
            .target
            .and_then(|target| target.tab_id)
            .ok_or_else(|| {
                SessionError("browserd created a tab without returning its ID".into())
            })?;
        self.tab_id = tab_id;
        self.current_url = match reply.result {
            Some(CommandResult::Navigation(navigation)) => navigation.url,
            _ => "about:blank".into(),
        };
        Ok(())
    }

    pub fn close_active_tab(&mut self) -> Result<(), SessionError> {
        let closing = self.tab_id.clone();
        self.send_without_tab(Command::Tab(TabCommand::Close { tab: closing }))?;
        let Some(next) = self.tabs()?.into_iter().next() else {
            return self.new_tab();
        };
        self.send_without_tab(Command::Tab(TabCommand::Activate {
            tab: next.tab_id.clone(),
        }))?;
        self.tab_id = next.tab_id;
        self.current_url = next.url;
        Ok(())
    }

    pub fn navigate(&mut self, url: &str) -> Result<(), SessionError> {
        let reply = self.send(Command::Navigate(NavigationCommand::Open {
            url: url.to_owned(),
        }))?;
        if let Some(CommandResult::Navigation(navigation)) = reply.result {
            self.current_url = navigation.url;
        } else {
            self.current_url = url.to_owned();
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), SessionError> {
        self.send(Command::Navigate(NavigationCommand::Stop))?;
        Ok(())
    }

    pub fn begin_navigation(
        &mut self,
        action: NavigationCommand,
    ) -> Result<NavigationTask, SessionError> {
        let request = self.make_request(Command::Navigate(action), Some(self.tab_id.clone()))?;
        Ok(NavigationTask {
            socket: self.socket.clone(),
            request,
        })
    }

    pub fn apply_navigation_outcome(&mut self, outcome: NavigationOutcome) {
        if let Some(url) = outcome.url {
            self.current_url = url;
        }
    }

    pub fn begin_frame_input(
        &mut self,
        view_id: ViewId,
        input: FrameInput,
    ) -> Result<NavigationTask, SessionError> {
        let request = self.make_request(
            Command::FrameInput(FrameInputCommand { view_id, input }),
            Some(self.tab_id.clone()),
        )?;
        Ok(NavigationTask {
            socket: self.socket.clone(),
            request,
        })
    }

    pub fn frame_input(
        &mut self,
        view_id: ViewId,
        input: FrameInput,
    ) -> Result<NavigationOutcome, SessionError> {
        let reply = self.send(Command::FrameInput(FrameInputCommand { view_id, input }))?;
        if std::env::var_os("RWB_GUI_TRACE_POINTER").is_some() {
            let outcome = match &reply.result {
                Some(CommandResult::Navigation(navigation)) => {
                    format!("navigation url={}", navigation.url)
                }
                Some(CommandResult::Empty) => "empty".to_owned(),
                Some(result) => format!("other {result:?}"),
                None => "none".to_owned(),
            };
            eprintln!("rwb-gui-frame-input-reply {outcome}");
        }
        let url = match reply.result {
            Some(CommandResult::Navigation(navigation)) => Some(navigation.url),
            _ => None,
        };
        Ok(NavigationOutcome { url })
    }

    pub fn render_frame(&mut self, width: u32, height: u32) -> Result<Frame, SessionError> {
        let reply = self.send(Command::Diagnostic(
            DiagnosticCommand::RenderFrameViewport { width, height },
        ))?;
        match reply.result {
            Some(CommandResult::Frame(frame)) => Ok(frame),
            _ => Err(SessionError(
                "browserd returned no frame for render_frame".into(),
            )),
        }
    }

    fn send(&mut self, command: Command) -> Result<CommandReply, SessionError> {
        self.send_request(command, Some(self.tab_id.clone()))
    }

    fn send_without_tab(&mut self, command: Command) -> Result<CommandReply, SessionError> {
        self.send_request(command, None)
    }

    fn send_request(
        &mut self,
        command: Command,
        tab_id: Option<TabId>,
    ) -> Result<CommandReply, SessionError> {
        let request = self.make_request(command, tab_id)?;
        let reply = self
            .client
            .send(&request)
            .map_err(|error| SessionError(error.to_string()))?;
        successful_reply(reply)
    }

    fn make_request(
        &mut self,
        command: Command,
        tab_id: Option<TabId>,
    ) -> Result<CommandRequest, SessionError> {
        let request_id = RequestId::parse(format!(
            "r_gui_{}_{}",
            self.request_namespace, self.next_request
        ))
        .map_err(|error| SessionError(error.to_string()))?;
        self.next_request = self.next_request.saturating_add(1);
        Ok(CommandRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            profile_id: self.profile_id.clone(),
            tab_id,
            command,
            timeout_ms: 30_000,
        })
    }
}

fn successful_reply(reply: CommandReply) -> Result<CommandReply, SessionError> {
    if reply.success {
        Ok(reply)
    } else {
        let message = reply
            .error
            .map(|error| format!("{}: {}", serde_error_code(error.code), error.message))
            .unwrap_or_else(|| "browserd rejected the request without an error".into());
        Err(SessionError(message))
    }
}

fn serde_error_code(code: browser_protocol::ErrorCode) -> String {
    format!("{code:?}")
}
