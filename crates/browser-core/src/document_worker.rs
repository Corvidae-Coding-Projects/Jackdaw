use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    io::{BufRead, BufReader, BufWriter, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread::{self, JoinHandle},
    time::Duration,
};

use browser_engine::{DocumentHost, DocumentRuntime, ElementIdentity, ElementTarget};
use browser_net::{BrowserResponse, RedirectHop};

use crate::worker_protocol::DocumentWorkerHttpResponse;

const WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Parent-side owner for the daemon's private document-worker process.
///
/// Replies are decoded on a dedicated pipe reader so a wedged child cannot
/// block the service thread indefinitely. A missed deadline quarantines and
/// terminates the child; callers must create a replacement generation rather
/// than replaying an operation whose effect may be uncertain.
pub(crate) struct DocumentWorkerProcess {
    child: Child,
    writer: BufWriter<ChildStdin>,
    replies: Receiver<Result<DocumentWorkerReply, String>>,
    reader: Option<JoinHandle<()>>,
    healthy: bool,
}

impl DocumentWorkerProcess {
    pub(crate) fn spawn(html: &str) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not resolve document-worker executable: {error}"))?;
        let mut child = Command::new(executable)
            .env_clear()
            .arg("__document-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("could not start document-worker process: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "document-worker stdin was not captured".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "document-worker stdout was not captured".to_owned())?;
        let (reply_sender, replies) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("rwb-document-worker-reader".to_owned())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let reply = serde_json::from_str::<DocumentWorkerReply>(
                                line.trim_end_matches(['\r', '\n']),
                            )
                            .map_err(|error| {
                                format!("invalid private document-worker reply: {error}")
                            });
                            if reply_sender.send(reply).is_err() {
                                break;
                            }
                            line.clear();
                        }
                        Err(error) => {
                            let _ = reply_sender.send(Err(format!(
                                "could not read private document-worker reply: {error}"
                            )));
                            break;
                        }
                    }
                }
            })
            .map_err(|error| format!("could not start document-worker pipe reader: {error}"))?;
        let mut process = Self {
            child,
            writer: BufWriter::new(stdin),
            replies,
            reader: Some(reader),
            healthy: true,
        };
        match process.exchange(DocumentWorkerCommand::Initialize {
            html: html.to_owned(),
        })? {
            DocumentWorkerReply::Ready(_) => Ok(process),
            DocumentWorkerReply::Error { message } => Err(message),
            reply => Err(format!(
                "unexpected document-worker initialization reply: {reply:?}"
            )),
        }
    }

    pub(crate) fn exchange(
        &mut self,
        command: DocumentWorkerCommand,
    ) -> Result<DocumentWorkerReply, String> {
        self.exchange_with_broker(command, |_| {
            Err("document-worker unexpectedly requested a broker operation".to_owned())
        })
    }

    pub(crate) fn exchange_with_broker(
        &mut self,
        command: DocumentWorkerCommand,
        mut broker: impl FnMut(DocumentWorkerBrokerRequest) -> Result<DocumentWorkerBrokerResult, String>,
    ) -> Result<DocumentWorkerReply, String> {
        self.send(command)?;
        loop {
            match self.receive()? {
                DocumentWorkerReply::BrokerRequest(request) => {
                    let request_id = request.request_id();
                    let result = broker(request).unwrap_or_else(|message| {
                        DocumentWorkerBrokerResult::Error { message }
                    });
                    self.send(DocumentWorkerCommand::BrokerReply { request_id, result })?;
                }
                reply => return Ok(reply),
            }
        }
    }

    fn send(&mut self, command: DocumentWorkerCommand) -> Result<(), String> {
        if !self.healthy {
            return Err("document-worker process is quarantined".to_owned());
        }
        if let Err(error) = serde_json::to_writer(&mut self.writer, &command) {
            self.quarantine();
            return Err(format!(
                "could not encode private document-worker command: {error}"
            ));
        }
        if let Err(error) = self
            .writer
            .write_all(b"\n")
            .and_then(|()| self.writer.flush())
        {
            self.quarantine();
            return Err(format!(
                "could not send private document-worker command: {error}"
            ));
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<DocumentWorkerReply, String> {
        match self.replies.recv_timeout(WORKER_REPLY_TIMEOUT) {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => {
                self.quarantine();
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.quarantine();
                Err("document-worker process exceeded the 30 second reply deadline".to_owned())
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.quarantine();
                Err("document-worker process exited before replying".to_owned())
            }
        }
    }

    pub(crate) fn is_healthy(&mut self) -> bool {
        self.healthy && self.child.try_wait().is_ok_and(|status| status.is_none())
    }

    fn quarantine(&mut self) {
        self.healthy = false;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DocumentWorkerProcess {
    fn drop(&mut self) {
        if self.is_healthy() {
            let _ = self.exchange(DocumentWorkerCommand::Shutdown);
        }
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

use crate::worker_protocol::{
    DocumentWorkerBrokerRequest, DocumentWorkerBrokerResult, DocumentWorkerCommand,
    DocumentWorkerControl, DocumentWorkerFormField, DocumentWorkerFormFile,
    DocumentWorkerFormSubmission, DocumentWorkerLink, DocumentWorkerMetadata, DocumentWorkerReply,
    DocumentWorkerSelectedFile, DocumentWorkerTarget,
};

/// Exercise the complete parent/child transport and crash boundary used for document isolation.
///
/// This remains outside the public browser protocol and exists so release
/// verification can prove that the daemon can supervise its exact executable,
/// keep an unrelated worker responsive after a child dies, reject use of the
/// dead worker without replay, and create a distinct replacement process.
pub fn probe_document_worker_process() -> anyhow::Result<String> {
    const SURVIVOR_HTML: &str =
        "<!doctype html><title>survivor</title><main>survivor responsive</main>";
    const VICTIM_HTML: &str = "<!doctype html><title>victim</title><main>victim content</main>";
    const REPLACEMENT_HTML: &str =
        "<!doctype html><title>replacement</title><main>replacement responsive</main>";

    let mut survivor = DocumentWorkerProcess::spawn(SURVIVOR_HTML).map_err(anyhow::Error::msg)?;
    let mut victim = DocumentWorkerProcess::spawn(VICTIM_HTML).map_err(anyhow::Error::msg)?;
    let victim_pid = victim.child.id();

    victim
        .child
        .kill()
        .map_err(|error| anyhow::anyhow!("could not kill probe worker {victim_pid}: {error}"))?;
    victim
        .child
        .wait()
        .map_err(|error| anyhow::anyhow!("could not reap probe worker {victim_pid}: {error}"))?;
    if victim.exchange(DocumentWorkerCommand::Ping).is_ok() {
        anyhow::bail!("killed document worker unexpectedly accepted another command");
    }

    let survivor_text = match survivor
        .exchange(DocumentWorkerCommand::ObserveDocumentText)
        .map_err(anyhow::Error::msg)?
    {
        DocumentWorkerReply::DocumentText { text } => text,
        reply => anyhow::bail!("unexpected survivor document-worker reply: {reply:?}"),
    };
    if !survivor_text.contains("survivor responsive") || !survivor.is_healthy() {
        anyhow::bail!("unrelated document worker did not remain responsive after peer death");
    }

    let mut replacement =
        DocumentWorkerProcess::spawn(REPLACEMENT_HTML).map_err(anyhow::Error::msg)?;
    let replacement_pid = replacement.child.id();
    if replacement_pid == victim_pid {
        anyhow::bail!("replacement reused the killed document-worker process identity");
    }
    let isolation = match replacement
        .exchange(DocumentWorkerCommand::ProbeIsolation {
            file_path: std::env::current_exe()?.to_string_lossy().into_owned(),
            socket_path: std::env::temp_dir()
                .join("rwb-document-worker-forbidden.sock")
                .to_string_lossy()
                .into_owned(),
        })
        .map_err(anyhow::Error::msg)?
    {
        DocumentWorkerReply::Isolation {
            file_access_denied,
            socket_access_denied,
        } => (file_access_denied, socket_access_denied),
        reply => anyhow::bail!("unexpected document-worker isolation reply: {reply:?}"),
    };
    if !isolation.0 || !isolation.1 {
        anyhow::bail!(
            "document-worker sandbox allowed privileged access: file_denied={} socket_denied={}",
            isolation.0,
            isolation.1
        );
    }
    let replacement_text = match replacement
        .exchange(DocumentWorkerCommand::ObserveDocumentText)
        .map_err(anyhow::Error::msg)?
    {
        DocumentWorkerReply::DocumentText { text } => text,
        reply => anyhow::bail!("unexpected replacement document-worker reply: {reply:?}"),
    };
    if !replacement_text.contains("replacement responsive") || !replacement.is_healthy() {
        anyhow::bail!("replacement document worker was not independently responsive");
    }

    Ok(format!(
        "victim_pid={victim_pid} replacement_pid={replacement_pid} file_access=denied socket_access=denied survivor={survivor_text:?} replacement={replacement_text:?}"
    ))
}

fn metadata(runtime: &DocumentRuntime) -> DocumentWorkerMetadata {
    DocumentWorkerMetadata {
        generation: runtime.document_generation(),
        url: runtime.document_url().unwrap_or("about:blank").to_owned(),
        title: runtime.observe_document_title(),
    }
}

fn resolve_target(
    runtime: &DocumentRuntime,
    target: DocumentWorkerTarget,
) -> anyhow::Result<ElementTarget> {
    ElementIdentity::new(target.document_generation, target.element_id)
        .and_then(|identity| runtime.resolve_element_identity(&identity))
}

fn worker_target(target: &ElementTarget) -> DocumentWorkerTarget {
    DocumentWorkerTarget {
        document_generation: target.document_generation(),
        element_id: target.element_id().to_owned(),
    }
}

fn write_reply(writer: &mut impl Write, reply: &DocumentWorkerReply) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, reply)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

struct StdioDocumentHost<'a, R, W> {
    reader: RefCell<&'a mut R>,
    writer: RefCell<&'a mut W>,
    next_request_id: Cell<u64>,
}

impl<'a, R, W> StdioDocumentHost<'a, R, W>
where
    R: BufRead,
    W: Write,
{
    fn new(reader: &'a mut R, writer: &'a mut W) -> Self {
        Self {
            reader: RefCell::new(reader),
            writer: RefCell::new(writer),
            next_request_id: Cell::new(1),
        }
    }

    fn request(
        &self,
        build: impl FnOnce(u64) -> DocumentWorkerBrokerRequest,
    ) -> anyhow::Result<DocumentWorkerBrokerResult> {
        let request_id = self.next_request_id.get();
        let next_request_id = request_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("document-worker broker request IDs were exhausted"))?;
        self.next_request_id.set(next_request_id);

        {
            let mut writer = self.writer.borrow_mut();
            write_reply(
                &mut **writer,
                &DocumentWorkerReply::BrokerRequest(build(request_id)),
            )?;
        }

        let mut line = String::new();
        let bytes_read = self.reader.borrow_mut().read_line(&mut line)?;
        if bytes_read == 0 {
            anyhow::bail!("document-worker broker closed before replying");
        }
        let command = serde_json::from_str::<DocumentWorkerCommand>(
            line.trim_end_matches(['\r', '\n']),
        )
        .map_err(|error| anyhow::anyhow!("invalid document-worker broker reply: {error}"))?;
        match command {
            DocumentWorkerCommand::BrokerReply {
                request_id: reply_id,
                result,
            } if reply_id == request_id => Ok(result),
            DocumentWorkerCommand::BrokerReply {
                request_id: reply_id,
                ..
            } => anyhow::bail!(
                "document-worker broker replied to request {reply_id}; expected {request_id}"
            ),
            _ => anyhow::bail!(
                "document-worker broker sent a non-reply command while request {request_id} was pending"
            ),
        }
    }
}

fn decode_broker_response(
    response: DocumentWorkerHttpResponse,
) -> anyhow::Result<BrowserResponse> {
    let requested_url = browser_net::BrowserRequest::parse(&response.requested_url)?.url;
    let final_url = browser_net::BrowserRequest::parse(&response.final_url)?.url;
    let redirects = (0..response.redirect_count)
        .map(|_| RedirectHop {
            status: 302,
            from: requested_url.clone(),
            to: final_url.clone(),
        })
        .collect();
    Ok(BrowserResponse {
        url: final_url,
        status: response.status,
        headers: response.headers.into_iter().collect(),
        body: response.body,
        redirects,
    })
}

impl<R, W> DocumentHost for StdioDocumentHost<'_, R, W>
where
    R: BufRead,
    W: Write,
{
    fn fetch_subresource(&self, requested_url: &str) -> anyhow::Result<BrowserResponse> {
        match self.request(|request_id| DocumentWorkerBrokerRequest::FetchSubresource {
            request_id,
            requested_url: requested_url.to_owned(),
        })? {
            DocumentWorkerBrokerResult::HttpResponse { response } => {
                decode_broker_response(response)
            }
            DocumentWorkerBrokerResult::Error { message } => Err(anyhow::Error::msg(message)),
            _ => anyhow::bail!("document-worker broker returned the wrong subresource result"),
        }
    }

    fn load_local_storage(&self, origin: &str) -> anyhow::Result<BTreeMap<String, String>> {
        match self.request(|request_id| DocumentWorkerBrokerRequest::LoadLocalStorage {
            request_id,
            origin: origin.to_owned(),
        })? {
            DocumentWorkerBrokerResult::Storage { entries } => Ok(entries.into_iter().collect()),
            DocumentWorkerBrokerResult::Error { message } => Err(anyhow::Error::msg(message)),
            _ => anyhow::bail!("document-worker broker returned the wrong storage result"),
        }
    }

    fn replace_local_storage(
        &self,
        origin: &str,
        entries: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        match self.request(|request_id| DocumentWorkerBrokerRequest::ReplaceLocalStorage {
            request_id,
            origin: origin.to_owned(),
            entries: entries
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        })? {
            DocumentWorkerBrokerResult::Applied => Ok(()),
            DocumentWorkerBrokerResult::Error { message } => Err(anyhow::Error::msg(message)),
            _ => anyhow::bail!("document-worker broker returned the wrong storage mutation result"),
        }
    }

    fn document_cookie(&self, document_url: &str) -> anyhow::Result<String> {
        match self.request(|request_id| DocumentWorkerBrokerRequest::DocumentCookie {
            request_id,
            document_url: document_url.to_owned(),
        })? {
            DocumentWorkerBrokerResult::Cookie { value } => Ok(value),
            DocumentWorkerBrokerResult::Error { message } => Err(anyhow::Error::msg(message)),
            _ => anyhow::bail!("document-worker broker returned the wrong cookie result"),
        }
    }

    fn store_document_cookie(
        &self,
        document_url: &str,
        assignment: &str,
    ) -> anyhow::Result<()> {
        match self.request(|request_id| DocumentWorkerBrokerRequest::StoreDocumentCookie {
            request_id,
            document_url: document_url.to_owned(),
            assignment: assignment.to_owned(),
        })? {
            DocumentWorkerBrokerResult::Applied => Ok(()),
            DocumentWorkerBrokerResult::Error { message } => Err(anyhow::Error::msg(message)),
            _ => anyhow::bail!("document-worker broker returned the wrong cookie mutation result"),
        }
    }
}

/// Run the daemon's private document-worker transport on stdin/stdout.
///
/// The entrypoint is deliberately hidden from the public browser protocol. The
/// supervising daemon owns the child process and is the only intended caller.
pub fn run_document_worker_stdio() -> anyhow::Result<()> {
    crate::sandbox::enter_document_worker_sandbox()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut runtime = None;
    let mut line = String::new();

    while reader.read_line(&mut line)? != 0 {
        let command = match serde_json::from_str::<DocumentWorkerCommand>(
            line.trim_end_matches(['\r', '\n']),
        ) {
            Ok(command) => command,
            Err(error) => {
                write_reply(
                    &mut writer,
                    &DocumentWorkerReply::Error {
                        message: format!("invalid private worker command: {error}"),
                    },
                )?;
                line.clear();
                continue;
            }
        };

        let (reply, shutdown) = match command {
            DocumentWorkerCommand::Initialize { html } => {
                if runtime.is_some() {
                    (
                        DocumentWorkerReply::Error {
                            message: "document worker is already initialized".to_owned(),
                        },
                        false,
                    )
                } else {
                    match DocumentRuntime::from_html(&html) {
                        Ok(document) => {
                            let reply = DocumentWorkerReply::Ready(metadata(&document));
                            runtime = Some(document);
                            (reply, false)
                        }
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
            }
            DocumentWorkerCommand::ObserveDocumentText => match runtime.as_mut() {
                Some(document) => match document.observe_document_text() {
                    Ok(text) => (DocumentWorkerReply::DocumentText { text }, false),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::ObserveControls => match runtime.as_mut() {
                Some(document) => match document.observe_controls() {
                    Ok(controls) => (
                        DocumentWorkerReply::Controls {
                            controls: controls
                                .into_iter()
                                .map(|control| DocumentWorkerControl {
                                    target: DocumentWorkerTarget {
                                        document_generation: control.target.document_generation(),
                                        element_id: control.target.element_id().to_owned(),
                                    },
                                    role: control.role,
                                    name: control.name,
                                    value: control.value,
                                    disabled: control.disabled,
                                    focused: control.focused,
                                    checked: control.checked,
                                    required: control.required,
                                    invalid: control.invalid,
                                })
                                .collect(),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::ObserveLinks => match runtime.as_mut() {
                Some(document) => match document.observe_links() {
                    Ok(links) => (
                        DocumentWorkerReply::Links {
                            links: links
                                .into_iter()
                                .map(|link| DocumentWorkerLink {
                                    target: DocumentWorkerTarget {
                                        document_generation: link.target.document_generation(),
                                        element_id: link.target.element_id().to_owned(),
                                    },
                                    name: link.name,
                                    url: link.url,
                                })
                                .collect(),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Metadata => match runtime.as_ref() {
                Some(document) => (DocumentWorkerReply::Metadata(metadata(document)), false),
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::HitTest { x, y } => match runtime.as_mut() {
                Some(document) => match document.hit_test(x, y) {
                    Ok(target) => (
                        DocumentWorkerReply::Target {
                            target: Some(DocumentWorkerTarget {
                                document_generation: target.document_generation(),
                                element_id: target.element_id().to_owned(),
                            }),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::LinkUrl { target } => match runtime.as_ref() {
                Some(document) => {
                    match ElementIdentity::new(target.document_generation, target.element_id)
                        .and_then(|identity| document.resolve_element_identity(&identity))
                        .and_then(|target| document.link_url(&target))
                    {
                        Ok(value) => (DocumentWorkerReply::OptionalString { value }, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::ValidateTarget { target } => match runtime.as_ref() {
                Some(document) => {
                    match ElementIdentity::new(target.document_generation, target.element_id)
                        .and_then(|identity| document.resolve_element_identity(&identity))
                        .and_then(|target| document.validate_target(&target))
                    {
                        Ok(()) => (DocumentWorkerReply::Validated, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::FocusedControl => match runtime.as_mut() {
                Some(document) => match document.observe_controls() {
                    Ok(controls) => (
                        DocumentWorkerReply::Target {
                            target: controls.into_iter().find(|control| control.focused).map(
                                |control| DocumentWorkerTarget {
                                    document_generation: control.target.document_generation(),
                                    element_id: control.target.element_id().to_owned(),
                                },
                            ),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::RunNextTimer => match runtime.as_mut() {
                Some(document) => match document.run_next_timer() {
                    Ok(advanced) => (DocumentWorkerReply::Advanced { advanced }, false),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::NavigationTarget { navigation } => match runtime.as_ref() {
                Some(document) => {
                    let url = match navigation {
                        crate::worker_protocol::DocumentWorkerNavigation::Open => None,
                        crate::worker_protocol::DocumentWorkerNavigation::Back => {
                            document.back_target_url()
                        }
                        crate::worker_protocol::DocumentWorkerNavigation::Forward => {
                            document.forward_target_url()
                        }
                        crate::worker_protocol::DocumentWorkerNavigation::Reload => {
                            document.reload_target_url()
                        }
                    };
                    (DocumentWorkerReply::NavigationTarget { url }, false)
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::NavigationSnapshot => match runtime.as_ref() {
                Some(document) => (
                    DocumentWorkerReply::NavigationSnapshot {
                        snapshot: document.navigation_snapshot().map(|(history, cursor)| {
                            crate::worker_protocol::DocumentWorkerNavigationSnapshot {
                                history,
                                cursor,
                            }
                        }),
                    },
                    false,
                ),
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Click { target } => match runtime.as_mut() {
                Some(document) => match resolve_target(document, target)
                    .and_then(|target| document.click(&target))
                {
                    Ok(value) => (DocumentWorkerReply::Boolean { value }, false),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Key {
                target,
                key,
                modifiers,
            } => match runtime.as_mut() {
                Some(document) => match resolve_target(document, target)
                    .and_then(|target| document.key(&target, &key, &modifiers))
                {
                    Ok(value) => (DocumentWorkerReply::Boolean { value }, false),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Select { target, values } => match runtime.as_mut() {
                Some(document) => match resolve_target(document, target)
                    .and_then(|target| document.select(&target, &values))
                {
                    Ok(value) => (DocumentWorkerReply::Boolean { value }, false),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::SelectFiles { target, files } => match runtime.as_mut() {
                Some(document) => {
                    let files = files
                        .into_iter()
                        .map(
                            |DocumentWorkerSelectedFile {
                                 file_name,
                                 media_type,
                                 bytes,
                             }| (file_name, media_type, bytes),
                        )
                        .collect();
                    match resolve_target(document, target)
                        .and_then(|target| document.select_files(&target, files))
                    {
                        Ok(()) => (DocumentWorkerReply::Mutated, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::SubmissionAfterClick { target } => match runtime.as_mut() {
                Some(document) => match resolve_target(document, target)
                    .and_then(|target| document.submission_after_click(&target))
                {
                    Ok(submission) => (
                        DocumentWorkerReply::FormSubmission {
                            submission: submission.map(|submission| DocumentWorkerFormSubmission {
                                method: submission.method,
                                action: submission.action,
                                enctype: submission.enctype,
                                fields: submission
                                    .fields
                                    .into_iter()
                                    .map(|field| DocumentWorkerFormField {
                                        name: field.name,
                                        value: field.value,
                                    })
                                    .collect(),
                                files: submission
                                    .files
                                    .into_iter()
                                    .map(|file| DocumentWorkerFormFile {
                                        name: file.name,
                                        file_name: file.file_name,
                                        media_type: file.media_type,
                                        bytes: file.bytes,
                                    })
                                    .collect(),
                            }),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::ResolveSubmissionTarget { target } => match runtime.as_ref() {
                Some(document) => match resolve_target(document, target)
                    .and_then(|target| document.resolve_submission_target(&target))
                {
                    Ok((form, submitter)) => (
                        DocumentWorkerReply::SubmissionTarget {
                            form: worker_target(&form),
                            submitter: submitter.as_ref().map(worker_target),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Submit { form, submitter } => match runtime.as_mut() {
                Some(document) => {
                    let result = resolve_target(document, form).and_then(|form| {
                        let submitter = submitter
                            .map(|target| resolve_target(document, target))
                            .transpose()?;
                        document.submit(&form, submitter.as_ref())
                    });
                    match result {
                        Ok(submission) => (
                            DocumentWorkerReply::FormSubmission {
                                submission: submission.map(|submission| DocumentWorkerFormSubmission {
                                    method: submission.method,
                                    action: submission.action,
                                    enctype: submission.enctype,
                                    fields: submission
                                        .fields
                                        .into_iter()
                                        .map(|field| DocumentWorkerFormField {
                                            name: field.name,
                                            value: field.value,
                                        })
                                        .collect(),
                                    files: submission
                                        .files
                                        .into_iter()
                                        .map(|file| DocumentWorkerFormFile {
                                            name: file.name,
                                            file_name: file.file_name,
                                            media_type: file.media_type,
                                            bytes: file.bytes,
                                        })
                                        .collect(),
                                }),
                            },
                            false,
                        ),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Focus { target } => match runtime.as_mut() {
                Some(document) => {
                    match ElementIdentity::new(target.document_generation, target.element_id)
                        .and_then(|identity| document.resolve_element_identity(&identity))
                        .and_then(|target| document.focus(&target))
                    {
                        Ok(()) => (DocumentWorkerReply::Mutated, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Fill { target, value } => match runtime.as_mut() {
                Some(document) => {
                    match ElementIdentity::new(target.document_generation, target.element_id)
                        .and_then(|identity| document.resolve_element_identity(&identity))
                        .and_then(|target| document.fill(&target, &value))
                    {
                        Ok(()) => (DocumentWorkerReply::Mutated, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::TypeText { target, text } => match runtime.as_mut() {
                Some(document) => {
                    match ElementIdentity::new(target.document_generation, target.element_id)
                        .and_then(|identity| document.resolve_element_identity(&identity))
                        .and_then(|target| document.type_text(&target, &text))
                    {
                        Ok(()) => (DocumentWorkerReply::Mutated, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Check { target, checked } => match runtime.as_mut() {
                Some(document) => {
                    match ElementIdentity::new(target.document_generation, target.element_id)
                        .and_then(|identity| document.resolve_element_identity(&identity))
                        .and_then(|target| document.check(&target, checked))
                    {
                        Ok(_) => (DocumentWorkerReply::Mutated, false),
                        Err(error) => (
                            DocumentWorkerReply::Error {
                                message: error.to_string(),
                            },
                            false,
                        ),
                    }
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::EvaluateScript { source } => match runtime.as_mut() {
                Some(document) => match document.evaluate_script(&source) {
                    Ok(_) => (DocumentWorkerReply::Mutated, false),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::ScrollBy { delta_x, delta_y } => match runtime.as_mut() {
                Some(document) => {
                    let (x, y) = document.scroll_by(delta_x, delta_y);
                    (DocumentWorkerReply::Scrolled { x, y }, false)
                }
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::RenderDocument { width, height } => match runtime.as_mut() {
                Some(document) => match document.render_document_at(width, height) {
                    Ok(pixels) => (
                        DocumentWorkerReply::RenderedDocument {
                            width: u32::from(pixels.width()),
                            height: u32::from(pixels.height()),
                            rgba_premultiplied: pixels
                                .data()
                                .iter()
                                .flat_map(|pixel| pixel.to_u8_array())
                                .collect(),
                        },
                        false,
                    ),
                    Err(error) => (
                        DocumentWorkerReply::Error {
                            message: error.to_string(),
                        },
                        false,
                    ),
                },
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::ProbeIsolation {
                file_path,
                socket_path,
            } => {
                let file_access_denied = std::fs::read(file_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
                let socket_access_denied = std::os::unix::net::UnixStream::connect(socket_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied);
                (
                    DocumentWorkerReply::Isolation {
                        file_access_denied,
                        socket_access_denied,
                    },
                    false,
                )
            }
            DocumentWorkerCommand::BrokerReply { .. } => (
                DocumentWorkerReply::Error {
                    message: "unexpected document-worker broker reply without a pending request"
                        .to_owned(),
                },
                false,
            ),
            DocumentWorkerCommand::Ping => match runtime.as_ref() {
                Some(document) => (DocumentWorkerReply::Pong(metadata(document)), false),
                None => (
                    DocumentWorkerReply::Error {
                        message: "document worker is not initialized".to_owned(),
                    },
                    false,
                ),
            },
            DocumentWorkerCommand::Shutdown => (DocumentWorkerReply::Shutdown, true),
        };
        write_reply(&mut writer, &reply)?;
        line.clear();
        if shutdown {
            break;
        }
    }
    Ok(())
}
