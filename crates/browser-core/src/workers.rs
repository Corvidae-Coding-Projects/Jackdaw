use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use browser_engine::{
    DocumentHost, DocumentRuntime, ElementIdentity, ElementTarget, FormNavigationRequest,
    FormSubmission, NavigationCommit, PageConsoleRecord, PageFetchDispatch, PageFetchRequest,
    PageNetworkRecord, SemanticControl, SemanticLink,
};
use browser_net::BrowserResponse;
use browser_protocol::{ProfileId, TabId};

/// Read-only document state returned across the worker ownership boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentMetadata {
    pub(crate) generation: u64,
    pub(crate) url: String,
    pub(crate) title: String,
}

/// Owned raster data returned by a document owner.
///
/// Keeping Vello's pixmap type behind this boundary prevents frontends and the
/// service from depending on engine-owned rendering values.
pub(crate) struct RenderedDocument {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba_premultiplied: Vec<u8>,
}

/// Typed queries accepted by a document owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerQuery {
    DocumentText,
    Controls,
    Links,
    HitTest { x: i32, y: i32 },
    LinkUrl(ElementIdentity),
    ValidateTarget(ElementIdentity),
    FocusedControl,
    RunNextTimer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerMutation {
    Focus(ElementIdentity),
    Fill {
        target: ElementIdentity,
        value: String,
    },
    TypeText {
        target: ElementIdentity,
        text: String,
    },
    Check {
        target: ElementIdentity,
        checked: bool,
    },
    EvaluateScript(String),
    ScrollBy {
        delta_x: i32,
        delta_y: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerNavigation {
    Open,
    Back,
    Forward,
    Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerCommand {
    RenderDocumentAt { width: u16, height: u16 },
}

#[derive(Debug, Clone)]
pub(crate) enum WorkerReply {
    DocumentText(String),
    Controls(Vec<SemanticControl>),
    Links(Vec<SemanticLink>),
    Target(Option<ElementTarget>),
    LinkUrl(Option<String>),
    TargetValidated,
    Advanced(bool),
}

type RuntimeClosure = Box<dyn FnOnce(&mut DocumentRuntime) + Send + 'static>;

/// Owned command envelope for document-runtime work.
///
/// Keeping the envelope explicit lets production callers migrate away from
/// anonymous in-process closures one command family at a time. Once every
/// family is represented by owned data, this same boundary can be carried by
/// the supervised private worker process without exposing Boa or Blitz values.
enum RuntimeTask {
    Execute(RuntimeClosure),
    Query {
        query: WorkerQuery,
        reply: Sender<Result<WorkerReply, String>>,
    },
}

impl RuntimeTask {
    fn run(self, runtime: &mut DocumentRuntime) {
        match self {
            Self::Execute(operation) => operation(runtime),
            Self::Query { query, reply } => {
                let _ = reply.send(execute_worker_query(runtime, query));
            }
        }
    }
}

fn execute_worker_query(
    runtime: &mut DocumentRuntime,
    query: WorkerQuery,
) -> Result<WorkerReply, String> {
    match query {
        WorkerQuery::DocumentText => runtime
            .observe_document_text()
            .map(WorkerReply::DocumentText)
            .map_err(|error| error.to_string()),
        WorkerQuery::Controls => runtime
            .observe_controls()
            .map(WorkerReply::Controls)
            .map_err(|error| error.to_string()),
        WorkerQuery::Links => runtime
            .observe_links()
            .map(WorkerReply::Links)
            .map_err(|error| error.to_string()),
        WorkerQuery::HitTest { x, y } => runtime
            .hit_test(x, y)
            .map(|target| WorkerReply::Target(Some(target)))
            .map_err(|error| error.to_string()),
        WorkerQuery::LinkUrl(identity) => runtime
            .resolve_element_identity(&identity)
            .and_then(|target| runtime.link_url(&target))
            .map(WorkerReply::LinkUrl)
            .map_err(|error| error.to_string()),
        WorkerQuery::ValidateTarget(identity) => runtime
            .resolve_element_identity(&identity)
            .and_then(|target| runtime.validate_target(&target))
            .map(|()| WorkerReply::TargetValidated)
            .map_err(|error| error.to_string()),
        WorkerQuery::FocusedControl => runtime
            .observe_controls()
            .map(|controls| {
                WorkerReply::Target(
                    controls
                        .into_iter()
                        .find(|control| control.focused)
                        .map(|control| control.target),
                )
            })
            .map_err(|error| error.to_string()),
        WorkerQuery::RunNextTimer => runtime
            .run_next_timer()
            .map(WorkerReply::Advanced)
            .map_err(|error| error.to_string()),
    }
}

enum HostRequest {
    FetchSubresource {
        requested_url: String,
        reply: Sender<Result<BrowserResponse, String>>,
    },
    LoadLocalStorage {
        origin: String,
        reply: Sender<Result<BTreeMap<String, String>, String>>,
    },
    ReplaceLocalStorage {
        origin: String,
        entries: BTreeMap<String, String>,
        reply: Sender<Result<(), String>>,
    },
    DocumentCookie {
        document_url: String,
        reply: Sender<Result<String, String>>,
    },
    StoreDocumentCookie {
        document_url: String,
        assignment: String,
        reply: Sender<Result<(), String>>,
    },
}

struct WorkerDocumentHost {
    sender: Sender<HostRequest>,
}

impl WorkerDocumentHost {
    fn request<T>(
        &self,
        build: impl FnOnce(Sender<Result<T, String>>) -> HostRequest,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.sender
            .send(build(reply_sender))
            .map_err(|_| anyhow::anyhow!("document broker callback channel closed"))?;
        reply_receiver
            .recv()
            .map_err(|_| anyhow::anyhow!("document broker callback reply was lost"))?
            .map_err(anyhow::Error::msg)
    }
}

impl DocumentHost for WorkerDocumentHost {
    fn fetch_subresource(&self, requested_url: &str) -> anyhow::Result<BrowserResponse> {
        self.request(|reply| HostRequest::FetchSubresource {
            requested_url: requested_url.to_owned(),
            reply,
        })
    }

    fn load_local_storage(&self, origin: &str) -> anyhow::Result<BTreeMap<String, String>> {
        self.request(|reply| HostRequest::LoadLocalStorage {
            origin: origin.to_owned(),
            reply,
        })
    }

    fn replace_local_storage(
        &self,
        origin: &str,
        entries: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        self.request(|reply| HostRequest::ReplaceLocalStorage {
            origin: origin.to_owned(),
            entries: entries.clone(),
            reply,
        })
    }

    fn document_cookie(&self, document_url: &str) -> anyhow::Result<String> {
        self.request(|reply| HostRequest::DocumentCookie {
            document_url: document_url.to_owned(),
            reply,
        })
    }

    fn store_document_cookie(&self, document_url: &str, assignment: &str) -> anyhow::Result<()> {
        self.request(|reply| HostRequest::StoreDocumentCookie {
            document_url: document_url.to_owned(),
            assignment: assignment.to_owned(),
            reply,
        })
    }
}

/// Document execution owner used by a tab.
///
/// The live Boa realm and Blitz document are constructed and retained exclusively
/// on this owner's thread. Calls cross an owned-message boundary, and privileged
/// document callbacks are synchronously mediated by the service-side broker.
pub(crate) struct DocumentWorker {
    sender: Option<Sender<RuntimeTask>>,
    join_handle: Option<JoinHandle<()>>,
    healthy: AtomicBool,
}

impl DocumentWorker {
    pub(crate) fn from_html(html: &str) -> Result<Self, String> {
        let html = html.to_owned();
        let (task_sender, task_receiver) = mpsc::channel::<RuntimeTask>();
        let (startup_sender, startup_receiver) = mpsc::channel();
        let join_handle = thread::Builder::new()
            .name("rwb-document-owner".to_owned())
            .spawn(move || match DocumentRuntime::from_html(&html) {
                Ok(mut runtime) => {
                    let _ = startup_sender.send(Ok(()));
                    while let Ok(task) = task_receiver.recv() {
                        task.run(&mut runtime);
                    }
                }
                Err(error) => {
                    let _ = startup_sender.send(Err(error.to_string()));
                }
            })
            .map_err(|error| format!("could not start document owner: {error}"))?;

        match startup_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                sender: Some(task_sender),
                join_handle: Some(join_handle),
                healthy: AtomicBool::new(true),
            }),
            Ok(Err(error)) => {
                drop(task_sender);
                let _ = join_handle.join();
                Err(error)
            }
            Err(_) => {
                drop(task_sender);
                let _ = join_handle.join();
                Err("document owner exited during startup".to_owned())
            }
        }
    }

    fn sender(&self) -> Result<&Sender<RuntimeTask>, String> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err("document owner is quarantined after a prior failure".to_owned());
        }
        self.sender
            .as_ref()
            .ok_or_else(|| "document owner is shut down".to_owned())
    }

    fn worker_failed(&self, message: &str) -> String {
        self.healthy.store(false, Ordering::Release);
        message.to_owned()
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    fn transact<T>(
        &self,
        operation: impl FnOnce(&mut DocumentRuntime) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String>
    where
        T: Send + 'static,
    {
        const WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

        let (reply_sender, reply_receiver) = mpsc::channel();
        self.sender()?
            .send(RuntimeTask::Execute(Box::new(move |runtime| {
                let _ = reply_sender.send(operation(runtime));
            })))
            .map_err(|_| self.worker_failed("document owner is unavailable"))?;
        match reply_receiver.recv_timeout(WORKER_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err(self.worker_failed("document owner exceeded the 30 second reply deadline"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(self.worker_failed("document owner exited before replying"))
            }
        }
    }

    fn transact_with_host<T>(
        &self,
        host: &impl DocumentHost,
        operation: impl FnOnce(&mut DocumentRuntime, &dyn DocumentHost) -> Result<T, String>
        + Send
        + 'static,
    ) -> Result<T, String>
    where
        T: Send + 'static,
    {
        const WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
        const BROKER_POLL_INTERVAL: Duration = Duration::from_millis(10);

        let (result_sender, result_receiver) = mpsc::channel();
        let (host_sender, host_receiver) = mpsc::channel();
        self.sender()?
            .send(RuntimeTask::Execute(Box::new(move |runtime| {
                let worker_host = WorkerDocumentHost {
                    sender: host_sender,
                };
                let _ = result_sender.send(operation(runtime, &worker_host));
            })))
            .map_err(|_| self.worker_failed("document owner is unavailable"))?;
        let deadline = std::time::Instant::now() + WORKER_REPLY_TIMEOUT;

        loop {
            match result_receiver.try_recv() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    return Err(self.worker_failed("document owner exited before replying"));
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(
                    self.worker_failed("document owner exceeded the 30 second reply deadline")
                );
            }
            let poll_interval = BROKER_POLL_INTERVAL.min(deadline.duration_since(now));
            match host_receiver.recv_timeout(poll_interval) {
                Ok(request) => Self::serve_host_request(host, request),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return match result_receiver.recv_timeout(deadline.duration_since(now)) {
                        Ok(result) => result,
                        Err(RecvTimeoutError::Timeout) => Err(self
                            .worker_failed("document owner exceeded the 30 second reply deadline")),
                        Err(RecvTimeoutError::Disconnected) => {
                            Err(self.worker_failed("document owner exited before replying"))
                        }
                    };
                }
            }
        }
    }

    fn serve_host_request(host: &impl DocumentHost, request: HostRequest) {
        match request {
            HostRequest::FetchSubresource {
                requested_url,
                reply,
            } => {
                let _ = reply.send(
                    host.fetch_subresource(&requested_url)
                        .map_err(|error| error.to_string()),
                );
            }
            HostRequest::LoadLocalStorage { origin, reply } => {
                let _ = reply.send(
                    host.load_local_storage(&origin)
                        .map_err(|error| error.to_string()),
                );
            }
            HostRequest::ReplaceLocalStorage {
                origin,
                entries,
                reply,
            } => {
                let _ = reply.send(
                    host.replace_local_storage(&origin, &entries)
                        .map_err(|error| error.to_string()),
                );
            }
            HostRequest::DocumentCookie {
                document_url,
                reply,
            } => {
                let _ = reply.send(
                    host.document_cookie(&document_url)
                        .map_err(|error| error.to_string()),
                );
            }
            HostRequest::StoreDocumentCookie {
                document_url,
                assignment,
                reply,
            } => {
                let _ = reply.send(
                    host.store_document_cookie(&document_url, &assignment)
                        .map_err(|error| error.to_string()),
                );
            }
        }
    }

    pub(crate) fn metadata(&self) -> DocumentMetadata {
        self.transact(|runtime| {
            Ok(DocumentMetadata {
                generation: runtime.document_generation(),
                url: runtime.document_url().unwrap_or("about:blank").to_owned(),
                title: runtime.observe_document_title(),
            })
        })
        .expect("document owner must return metadata")
    }

    pub(crate) fn navigation_target(&self, navigation: WorkerNavigation) -> Option<String> {
        self.transact(move |runtime| {
            Ok(match navigation {
                WorkerNavigation::Open => None,
                WorkerNavigation::Back => runtime.back_target_url(),
                WorkerNavigation::Forward => runtime.forward_target_url(),
                WorkerNavigation::Reload => runtime.reload_target_url(),
            })
        })
        .expect("document owner must return a navigation target")
    }

    pub(crate) fn commit_navigation(
        &mut self,
        host: &impl DocumentHost,
        navigation: WorkerNavigation,
        requested_url: &str,
        response: BrowserResponse,
    ) -> Result<Option<NavigationCommit>, String> {
        let requested_url = requested_url.to_owned();
        self.transact_with_host(host, move |runtime, host| {
            match navigation {
                WorkerNavigation::Open => runtime
                    .commit_brokered_navigation(host, &requested_url, response)
                    .map(Some),
                WorkerNavigation::Back => {
                    runtime.commit_brokered_back(host, &requested_url, response)
                }
                WorkerNavigation::Forward => {
                    runtime.commit_brokered_forward(host, &requested_url, response)
                }
                WorkerNavigation::Reload => {
                    runtime.commit_brokered_reload(host, &requested_url, response)
                }
            }
            .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn restore_history(
        &mut self,
        host: &impl DocumentHost,
        history: Vec<String>,
        cursor: usize,
        response: BrowserResponse,
    ) -> Result<(), String> {
        self.transact_with_host(host, move |runtime, host| {
            runtime
                .restore_brokered_history(host, history, cursor, response)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn navigation_snapshot(&self) -> Option<(Vec<String>, usize)> {
        self.transact(|runtime| Ok(runtime.navigation_snapshot()))
            .expect("document owner must return navigation history")
    }

    pub(crate) fn click(&mut self, target: &ElementTarget) -> Result<bool, String> {
        let target = target.clone();
        self.transact(move |runtime| runtime.click(&target).map_err(|error| error.to_string()))
    }

    pub(crate) fn key(
        &mut self,
        target: &ElementTarget,
        key: &str,
        modifiers: &[String],
    ) -> Result<bool, String> {
        let target = target.clone();
        let key = key.to_owned();
        let modifiers = modifiers.to_vec();
        self.transact(move |runtime| {
            runtime
                .key(&target, &key, &modifiers)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn select(
        &mut self,
        target: &ElementTarget,
        values: &[String],
    ) -> Result<bool, String> {
        let target = target.clone();
        let values = values.to_vec();
        self.transact(move |runtime| {
            runtime
                .select(&target, &values)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn select_files(
        &mut self,
        target: &ElementTarget,
        files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<(), String> {
        let target = target.clone();
        self.transact(move |runtime| {
            runtime
                .select_files(&target, files)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn console_records(&mut self) -> Result<Vec<PageConsoleRecord>, String> {
        self.transact(|runtime| runtime.console_records().map_err(|error| error.to_string()))
    }

    pub(crate) fn network_records(&self) -> Vec<PageNetworkRecord> {
        self.transact(|runtime| Ok(runtime.network_records().to_vec()))
            .expect("document owner must return network records")
    }

    pub(crate) fn submission_after_click(
        &mut self,
        target: &ElementTarget,
    ) -> Result<Option<FormSubmission>, String> {
        let target = target.clone();
        self.transact(move |runtime| {
            runtime
                .submission_after_click(&target)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn resolve_submission_target(
        &self,
        target: &ElementTarget,
    ) -> Result<(ElementTarget, Option<ElementTarget>), String> {
        let target = target.clone();
        self.transact(move |runtime| {
            runtime
                .resolve_submission_target(&target)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn submit(
        &mut self,
        form: &ElementTarget,
        submitter: Option<&ElementTarget>,
    ) -> Result<Option<FormSubmission>, String> {
        let form = form.clone();
        let submitter = submitter.cloned();
        self.transact(move |runtime| {
            runtime
                .submit(&form, submitter.as_ref())
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn prepare_form_submission(
        &self,
        submission: FormSubmission,
    ) -> Result<FormNavigationRequest, String> {
        self.transact(move |runtime| {
            runtime
                .prepare_form_submission(submission)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn commit_brokered_navigation(
        &mut self,
        host: &impl DocumentHost,
        requested_url: &str,
        response: BrowserResponse,
    ) -> Result<NavigationCommit, String> {
        let requested_url = requested_url.to_owned();
        self.transact_with_host(host, move |runtime, host| {
            runtime
                .commit_brokered_navigation(host, &requested_url, response)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn commit_brokered_form_submission(
        &mut self,
        host: &impl DocumentHost,
        requested_url: String,
        response: BrowserResponse,
    ) -> Result<NavigationCommit, String> {
        self.transact_with_host(host, move |runtime, host| {
            runtime
                .commit_brokered_form_submission(host, requested_url, response)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn take_page_fetch_requests(&mut self) -> Result<Vec<PageFetchRequest>, String> {
        self.transact(|runtime| {
            runtime
                .take_page_fetch_requests()
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn prepare_page_fetch(
        &self,
        request: &PageFetchRequest,
    ) -> Result<Option<PageFetchDispatch>, String> {
        let request = request.clone();
        self.transact(move |runtime| {
            runtime
                .prepare_page_fetch(&request)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn complete_page_fetch(
        &mut self,
        request: &PageFetchRequest,
        response: &BrowserResponse,
    ) -> Result<bool, String> {
        let request = request.clone();
        let response = response.clone();
        self.transact(move |runtime| {
            runtime
                .complete_page_fetch(&request, &response)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn reject_page_fetch_response(
        &mut self,
        request: &PageFetchRequest,
        response: &BrowserResponse,
        message: &str,
    ) -> Result<bool, String> {
        let request = request.clone();
        let response = response.clone();
        let message = message.to_owned();
        self.transact(move |runtime| {
            runtime
                .reject_page_fetch_response(&request, &response, &message)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn reject_page_fetch(
        &mut self,
        request: &PageFetchRequest,
        message: &str,
    ) -> Result<bool, String> {
        let request = request.clone();
        let message = message.to_owned();
        self.transact(move |runtime| {
            runtime
                .reject_page_fetch(&request, &message)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn query(&mut self, query: WorkerQuery) -> Result<WorkerReply, String> {
        const WORKER_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

        let (reply, receiver) = mpsc::channel();
        self.sender()?
            .send(RuntimeTask::Query { query, reply })
            .map_err(|_| self.worker_failed("document owner is unavailable"))?;
        match receiver.recv_timeout(WORKER_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err(self.worker_failed("document owner exceeded the 30 second reply deadline"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(self.worker_failed("document owner exited before replying"))
            }
        }
    }

    pub(crate) fn mutate(
        &mut self,
        mutation: WorkerMutation,
    ) -> Result<Option<(i64, i64)>, String> {
        self.transact(move |runtime| match mutation {
            WorkerMutation::Focus(identity) => runtime
                .resolve_element_identity(&identity)
                .and_then(|target| runtime.focus(&target))
                .map(|()| None)
                .map_err(|error| error.to_string()),
            WorkerMutation::Fill { target, value } => runtime
                .resolve_element_identity(&target)
                .and_then(|target| runtime.fill(&target, &value))
                .map(|()| None)
                .map_err(|error| error.to_string()),
            WorkerMutation::TypeText { target, text } => runtime
                .resolve_element_identity(&target)
                .and_then(|target| runtime.type_text(&target, &text))
                .map(|()| None)
                .map_err(|error| error.to_string()),
            WorkerMutation::Check { target, checked } => runtime
                .resolve_element_identity(&target)
                .and_then(|target| runtime.check(&target, checked))
                .map(|_| None)
                .map_err(|error| error.to_string()),
            WorkerMutation::EvaluateScript(source) => runtime
                .evaluate_script(&source)
                .map(|()| None)
                .map_err(|error| error.to_string()),
            WorkerMutation::ScrollBy { delta_x, delta_y } => {
                Ok(Some(runtime.scroll_by(delta_x, delta_y)))
            }
        })
    }

    pub(crate) fn execute(&mut self, command: WorkerCommand) -> Result<RenderedDocument, String> {
        self.transact(move |runtime| {
            let WorkerCommand::RenderDocumentAt { width, height } = command;
            let pixels = runtime
                .render_document_at(width, height)
                .map_err(|error| error.to_string())?;
            Ok(RenderedDocument {
                width: u32::from(pixels.width()),
                height: u32::from(pixels.height()),
                rgba_premultiplied: pixels
                    .data()
                    .iter()
                    .flat_map(|pixel| pixel.to_u8_array())
                    .collect(),
            })
        })
    }
}

impl Drop for DocumentWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join_handle) = self.join_handle.take()
            && join_handle.is_finished()
        {
            let _ = join_handle.join();
        }
    }
}

/// Private worker ownership registry.
///
/// Each tab has exactly one document owner. Replacing an entry drops the prior
/// owner and therefore invalidates every engine-owned document generation.
pub(crate) struct DocumentWorkers {
    workers: BTreeMap<(ProfileId, TabId), u64>,
    next_owner: u64,
}

impl DocumentWorkers {
    pub(crate) fn new() -> Self {
        Self {
            workers: BTreeMap::new(),
            next_owner: 1,
        }
    }

    pub(crate) fn register(&mut self, profile: ProfileId, tab: TabId) -> u64 {
        let owner = self.next_owner;
        self.next_owner = self.next_owner.saturating_add(1);
        self.workers.insert((profile, tab), owner);
        owner
    }

    pub(crate) fn remove(&mut self, profile: &ProfileId, tab: &TabId) -> Option<u64> {
        self.workers.remove(&(profile.clone(), tab.clone()))
    }

    pub(crate) fn owner(&self, profile: &ProfileId, tab: &TabId) -> Option<u64> {
        self.workers.get(&(profile.clone(), tab.clone())).copied()
    }

    pub(crate) fn attach(&mut self, profile: ProfileId, tab: TabId) -> Result<u64, String> {
        if self.owner(&profile, &tab).is_some() {
            return Err(format!(
                "tab {tab} already has a document owner for profile {profile}"
            ));
        }
        Ok(self.register(profile, tab))
    }

    pub(crate) fn detach(&mut self, profile: &ProfileId, tab: &TabId) -> Result<u64, String> {
        self.remove(profile, tab)
            .ok_or_else(|| format!("tab {tab} has no document owner for profile {profile}"))
    }

    pub(crate) fn replace(&mut self, profile: &ProfileId, tab: &TabId) -> Result<u64, String> {
        if self.owner(profile, tab).is_none() {
            return Err(format!(
                "tab {tab} has no document owner for profile {profile}"
            ));
        }
        Ok(self.register(profile.clone(), tab.clone()))
    }

    pub(crate) fn generation(&self, profile: &ProfileId, tab: &TabId) -> Option<u64> {
        self.owner(profile, tab)
    }

    pub(crate) fn is_current(&self, profile: &ProfileId, tab: &TabId, generation: u64) -> bool {
        self.owner(profile, tab) == Some(generation)
    }

    pub(crate) fn envelope(&self, profile: &ProfileId, tab: &TabId) -> Option<u64> {
        self.owner(profile, tab)
    }

    pub(crate) fn is_current_envelope(&self, envelope: &u64) -> bool {
        self.workers.values().any(|owner| owner == envelope)
    }
}
