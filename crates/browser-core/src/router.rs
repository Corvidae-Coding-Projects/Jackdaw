use std::time::Instant;

use browser_protocol::{
    Command, CommandReply, CommandRequest, CommandResult, DocumentReadyState, EffectState,
    ErrorCode, EventKind, Frame, FrameId, FramePixels, OperationReceipt, OperationResult,
    OperationStatus, PROTOCOL_VERSION, PixelFormat, ProfileId, ProtocolError, ReplyTarget,
    ServiceStatus, TabId, ViewId,
};
use browser_store::{OperationPhase, OperationRecord};

use crate::{
    capabilities::{capabilities, completed_effect},
    errors::{internal_error, invalid_request, protocol_error, request_id_conflict},
    observations::ViewSnapshot,
    operations::CompletedOperation,
    service::BrowserService,
};

const MAX_RETAINED_OPERATIONS_PER_PROFILE: usize = 1_024;
const MAX_IN_FLIGHT_OPERATIONS_PER_PROFILE: usize = MAX_RETAINED_OPERATIONS_PER_PROFILE + 1;

impl BrowserService {
    /// Route one validated protocol request without retaining transport borrows.
    pub fn handle(&mut self, request: CommandRequest) -> CommandReply {
        if let Some(completed) = self
            .operations
            .get(&request.profile_id, &request.request_id)
        {
            if completed.request == request {
                return completed.reply.clone();
            }
            return CommandReply {
                protocol_version: PROTOCOL_VERSION,
                request_id: request.request_id,
                success: false,
                target: Some(ReplyTarget {
                    profile_id: request.profile_id,
                    tab_id: request.tab_id,
                    document_id: None,
                }),
                receipt: None,
                result: None,
                error: Some(request_id_conflict()),
            };
        }
        if self
            .operations
            .is_expired(&request.profile_id, &request.request_id)
        {
            return CommandReply {
                protocol_version: PROTOCOL_VERSION,
                request_id: request.request_id.clone(),
                success: false,
                target: Some(ReplyTarget {
                    profile_id: request.profile_id,
                    tab_id: request.tab_id,
                    document_id: None,
                }),
                receipt: None,
                result: None,
                error: Some(protocol_error(
                    ErrorCode::OperationExpired,
                    format!("operation for request {} has expired", request.request_id),
                )),
            };
        }

        let operation_query = match &request.command {
            Command::Operation(browser_protocol::OperationCommand::Get { request_id }) => Some(
                self.operations
                    .get(&request.profile_id, request_id)
                    .cloned()
                    .ok_or_else(|| {
                        if self.operations.is_expired(&request.profile_id, request_id) {
                            protocol_error(
                                ErrorCode::OperationExpired,
                                format!("operation for request {request_id} has expired"),
                            )
                        } else {
                            invalid_request(format!(
                                "operation for request {request_id} does not exist"
                            ))
                        }
                    }),
            ),
            _ => None,
        };
        let request_id = request.request_id.clone();
        let profile_id = request.profile_id.clone();
        let requested_tab = request.tab_id.clone();
        let success_effect = completed_effect(&request.command);
        let operation_id = self.operations.next_id();
        let sequence = operation_id
            .as_str()
            .strip_prefix("op_")
            .and_then(|value| value.parse::<u64>().ok())
            .expect("service-generated operation receipt has a numeric sequence");
        let request_json =
            serde_json::to_string(&request).expect("validated command request serializes");

        let persistence_failure = |message: String, effect: EffectState| {
            let mut error = internal_error(message);
            error.effect = effect;
            CommandReply {
                protocol_version: PROTOCOL_VERSION,
                request_id: request_id.clone(),
                success: false,
                target: Some(ReplyTarget {
                    profile_id: profile_id.clone(),
                    tab_id: requested_tab.clone(),
                    document_id: None,
                }),
                receipt: Some(OperationReceipt {
                    id: operation_id.clone(),
                    status: OperationStatus::Failed,
                    effect,
                }),
                result: None,
                error: Some(error),
            }
        };

        if let Some(client) = self.broker.client(&profile_id) {
            let accepted = OperationRecord {
                sequence,
                request_id: request_id.as_str().to_owned(),
                request_json: request_json.clone(),
                phase: OperationPhase::Accepted,
                reply_json: None,
            };
            let evicted = match client
                .put_operation_record(&accepted, MAX_IN_FLIGHT_OPERATIONS_PER_PROFILE)
            {
                Ok(evicted) => evicted,
                Err(error) => {
                    return persistence_failure(
                        format!("could not persist accepted operation: {error}"),
                        EffectState::None,
                    );
                }
            };
            self.operations.mark_evicted(&profile_id, &evicted);
        }

        let outcome = if request.timeout_ms == 0 {
            Err(protocol_error(
                ErrorCode::Timeout,
                "request deadline elapsed before dispatch",
            ))
        } else {
            if let Some(client) = self.broker.client(&profile_id) {
                let dispatched = OperationRecord {
                    sequence,
                    request_id: request_id.as_str().to_owned(),
                    request_json: request_json.clone(),
                    phase: OperationPhase::Dispatched,
                    reply_json: None,
                };
                let evicted = match client
                    .put_operation_record(&dispatched, MAX_IN_FLIGHT_OPERATIONS_PER_PROFILE)
                {
                    Ok(evicted) => evicted,
                    Err(error) => {
                        return persistence_failure(
                            format!("could not persist operation dispatch boundary: {error}"),
                            EffectState::None,
                        );
                    }
                };
                self.operations.mark_evicted(&profile_id, &evicted);
            }
            match operation_query {
                Some(query) => query.and_then(|completed| {
                    let Command::Operation(browser_protocol::OperationCommand::Get { request_id }) =
                        &request.command
                    else {
                        unreachable!("operation query snapshot requires an operation command");
                    };
                    let receipt = completed.reply.receipt.clone().ok_or_else(|| {
                        internal_error("retained operation reply omitted its receipt")
                    })?;
                    Ok((
                        completed.request.tab_id.clone(),
                        CommandResult::Operation(OperationResult {
                            request_id: request_id.clone(),
                            receipt,
                        }),
                    ))
                }),
                None => self.dispatch(
                    &profile_id,
                    requested_tab.as_ref(),
                    request.command.clone(),
                    request.timeout_ms,
                ),
            }
        };
        let reply = match outcome {
            Ok((tab_id, result)) => CommandReply {
                protocol_version: PROTOCOL_VERSION,
                request_id: request_id.clone(),
                success: true,
                target: Some(ReplyTarget {
                    profile_id: profile_id.clone(),
                    document_id: tab_id
                        .as_ref()
                        .and_then(|id| self.tabs.get(id))
                        .map(crate::tabs::TabState::document_id),
                    tab_id,
                }),
                receipt: Some(OperationReceipt {
                    id: operation_id,
                    status: OperationStatus::Completed,
                    effect: success_effect,
                }),
                result: Some(result),
                error: None,
            },
            Err(error) => CommandReply {
                protocol_version: PROTOCOL_VERSION,
                request_id: request_id.clone(),
                success: false,
                target: Some(ReplyTarget {
                    profile_id: profile_id.clone(),
                    tab_id: requested_tab,
                    document_id: None,
                }),
                receipt: Some(OperationReceipt {
                    id: operation_id,
                    status: if error.code == ErrorCode::Timeout {
                        OperationStatus::TimedOut
                    } else {
                        OperationStatus::Failed
                    },
                    effect: error.effect,
                }),
                result: None,
                error: Some(error),
            },
        };
        self.emit_result_event(&profile_id, &reply);

        let completed = CompletedOperation {
            request,
            reply: reply.clone(),
        };
        if let Some(client) = self.broker.client(&profile_id) {
            let record = OperationRecord {
                sequence,
                request_id: request_id.as_str().to_owned(),
                request_json,
                phase: OperationPhase::Terminal,
                reply_json: Some(
                    serde_json::to_string(&completed.reply)
                        .expect("validated command reply serializes"),
                ),
            };
            let evicted = match client
                .put_operation_record(&record, MAX_RETAINED_OPERATIONS_PER_PROFILE)
            {
                Ok(evicted) => evicted,
                Err(error) => {
                    let mut persistence_error =
                        internal_error(format!("could not persist completed operation: {error}"));
                    persistence_error.effect = EffectState::Unknown;
                    return CommandReply {
                        protocol_version: PROTOCOL_VERSION,
                        request_id,
                        success: false,
                        target: completed.reply.target.clone(),
                        receipt: Some(OperationReceipt {
                            id: completed
                                .reply
                                .receipt
                                .as_ref()
                                .expect("completed operation has a receipt")
                                .id
                                .clone(),
                            status: OperationStatus::Failed,
                            effect: EffectState::Unknown,
                        }),
                        result: None,
                        error: Some(persistence_error),
                    };
                }
            };
            self.operations.mark_evicted(&profile_id, &evicted);
        }
        self.operations.insert(profile_id, completed);
        reply
    }

    fn emit_result_event(&mut self, profile_id: &ProfileId, reply: &CommandReply) {
        let (Some(target), Some(result)) = (reply.target.as_ref(), reply.result.as_ref()) else {
            return;
        };
        match result {
            CommandResult::Navigation(navigation) => {
                if let Some(tab_id) = target.tab_id.clone() {
                    self.events.push(
                        profile_id,
                        EventKind::NavigationCommitted {
                            tab_id,
                            document_id: navigation.document_id.clone(),
                            url: navigation.url.clone(),
                        },
                    );
                }
            }
            CommandResult::Download(download) => self.events.push(
                profile_id,
                EventKind::DownloadChanged {
                    download_id: download.download_id.clone(),
                    status: download.status,
                    received_bytes: download.bytes,
                },
            ),
            _ => {}
        }
    }

    fn required_tab(
        &self,
        profile_id: &ProfileId,
        requested_tab: Option<&TabId>,
    ) -> Result<TabId, ProtocolError> {
        requested_tab
            .cloned()
            .or_else(|| self.active_tabs.get(profile_id).cloned())
            .ok_or_else(|| invalid_request("command requires tab_id or an active tab"))
    }

    fn dispatch(
        &mut self,
        profile_id: &ProfileId,
        requested_tab: Option<&TabId>,
        command: Command,
        timeout_ms: u64,
    ) -> Result<(Option<TabId>, CommandResult), ProtocolError> {
        match command {
            Command::Status => Ok((
                None,
                CommandResult::Status(ServiceStatus {
                    running: true,
                    service_instance_id: self.events.instance_id().clone(),
                    profiles_open: u32::try_from(self.broker.len()).unwrap_or(u32::MAX),
                }),
            )),
            Command::Shutdown => Ok((None, CommandResult::Empty)),
            Command::Version => Ok((
                None,
                CommandResult::Version(browser_protocol::VersionResult {
                    product_version: env!("CARGO_PKG_VERSION").to_owned(),
                    protocol_version: browser_protocol::PROTOCOL_VERSION,
                }),
            )),
            Command::Capabilities => Ok((None, CommandResult::Capabilities(capabilities()))),
            Command::Events(command) => Ok((
                None,
                CommandResult::Events(self.events.read(profile_id, command)?),
            )),
            Command::Operation(command) => {
                let browser_protocol::OperationCommand::Get { request_id } = command;
                let completed = self
                    .operations
                    .get(profile_id, &request_id)
                    .ok_or_else(|| {
                        if self.operations.is_expired(profile_id, &request_id) {
                            protocol_error(
                                ErrorCode::OperationExpired,
                                format!("operation for request {request_id} has expired"),
                            )
                        } else {
                            invalid_request(format!(
                                "operation for request {request_id} does not exist"
                            ))
                        }
                    })?;
                let receipt = completed.reply.receipt.clone().ok_or_else(|| {
                    internal_error("retained operation reply omitted its receipt")
                })?;
                Ok((
                    completed.request.tab_id.clone(),
                    CommandResult::Operation(OperationResult {
                        request_id,
                        receipt,
                    }),
                ))
            }
            Command::Profile(command) => Ok((
                None,
                CommandResult::Profiles(self.handle_profile_command(profile_id, command)?),
            )),
            Command::Tab(command) => self.handle_tab_command(profile_id, command),
            Command::Navigate(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_navigation_command(
                    profile_id,
                    tab_id.clone(),
                    command,
                    timeout_ms,
                )?;
                Ok((Some(tab_id), result))
            }
            Command::Observe(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_observe(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Read(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_read(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Find(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_find(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Screenshot(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_screenshot(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Wait(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_wait(profile_id, tab_id.clone(), command, timeout_ms)?;
                Ok((Some(tab_id), result))
            }
            Command::Upload(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_upload(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Download(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_download(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::FrameInput(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_frame_input(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Interact(command) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let result = self.handle_interaction(profile_id, tab_id.clone(), command)?;
                Ok((Some(tab_id), result))
            }
            Command::Diagnostic(
                command @ (browser_protocol::DiagnosticCommand::RenderFrame
                | browser_protocol::DiagnosticCommand::RenderFrameViewport { .. }),
            ) => {
                let (viewport_width, viewport_height) = match command {
                    browser_protocol::DiagnosticCommand::RenderFrame => (800_u16, 600_u16),
                    browser_protocol::DiagnosticCommand::RenderFrameViewport { width, height } => {
                        if width == 0 || height == 0 {
                            return Err(invalid_request(
                                "frame viewport dimensions must both be nonzero",
                            ));
                        }
                        let width = u16::try_from(width).map_err(|_| {
                            invalid_request("frame viewport width exceeds the renderer limit")
                        })?;
                        let height = u16::try_from(height).map_err(|_| {
                            invalid_request("frame viewport height exceeds the renderer limit")
                        })?;
                        (width, height)
                    }
                    _ => unreachable!("render-frame dispatch matched another diagnostic"),
                };
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let view_sequence = self.next_view;
                self.next_view = self
                    .next_view
                    .checked_add(1)
                    .ok_or_else(|| internal_error("view identifier space exhausted"))?;
                let owner = self
                    .workers
                    .envelope(profile_id, &tab_id)
                    .ok_or_else(|| internal_error(format!("tab {tab_id} has no document owner")))?;
                let tab = self.tabs.get_mut(&tab_id).ok_or_else(|| {
                    protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not exist"),
                    )
                })?;
                if &tab.profile_id != profile_id {
                    return Err(protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not belong to profile {profile_id}"),
                    ));
                }
                let rendered = tab
                    .render_document_at(viewport_width, viewport_height)
                    .map_err(internal_error)?;
                let metadata = tab.metadata();
                let generation = metadata.generation;
                let document_id = tab.document_id();
                if !self.workers.is_current_envelope(&owner) {
                    return Err(internal_error(format!(
                        "discarded frame from obsolete document owner for tab {tab_id}"
                    )));
                }
                let width = rendered.width;
                let height = rendered.height;
                let stride = width
                    .checked_mul(4)
                    .ok_or_else(|| internal_error("frame stride overflow"))?;
                let bytes = rendered.rgba_premultiplied;
                let frame_id =
                    FrameId::parse(format!("f_{}", tab_id.as_str().trim_start_matches("t_")))
                        .expect("service-generated frame ID is valid");
                let view_id = ViewId::parse(format!(
                    "v_{}_{}_{view_sequence:020}",
                    tab_id.as_str().trim_start_matches("t_"),
                    generation
                ))
                .expect("service-generated view ID is valid");
                let mut snapshot = ViewSnapshot {
                    view_id: view_id.clone(),
                    document_id: document_id.clone(),
                    frame_id: frame_id.clone(),
                    url: metadata.url,
                    title: metadata.title,
                    loading: false,
                    ready_state: DocumentReadyState::Complete,
                    items: Vec::new(),
                    retained_at: Instant::now(),
                    retained_bytes: 0,
                };
                snapshot.retained_bytes = snapshot.measure_retained_bytes();
                tab.observed_views.insert(view_id.clone(), snapshot);
                tab.prune_views(self.view_retention, Instant::now());
                Ok((
                    Some(tab_id.clone()),
                    CommandResult::Frame(Frame {
                        tab_id,
                        frame_id,
                        document_id,
                        view_id,
                        width,
                        height,
                        pixel_scale: 1.0,
                        pixels: FramePixels::Owned {
                            format: PixelFormat::Rgba8Premultiplied,
                            stride,
                            bytes,
                        },
                    }),
                ))
            }
            Command::Diagnostic(browser_protocol::DiagnosticCommand::Evaluate { source }) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let tab = self.tabs.get_mut(&tab_id).ok_or_else(|| {
                    protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not exist"),
                    )
                })?;
                if &tab.profile_id != profile_id {
                    return Err(protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not belong to profile {profile_id}"),
                    ));
                }
                tab.document_worker
                    .mutate(crate::workers::WorkerMutation::EvaluateScript(source))
                    .map_err(|error| {
                        let mut error =
                            internal_error(format!("page script evaluation failed: {error}"));
                        error.effect = EffectState::Unknown;
                        error
                    })?;
                tab.invalidate_views();
                Ok((Some(tab_id), CommandResult::Empty))
            }
            Command::Diagnostic(browser_protocol::DiagnosticCommand::Console) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let tab = self.tabs.get_mut(&tab_id).ok_or_else(|| {
                    protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not exist"),
                    )
                })?;
                if &tab.profile_id != profile_id {
                    return Err(protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not belong to profile {profile_id}"),
                    ));
                }
                let records = tab
                    .document_worker
                    .console_records()
                    .map_err(internal_error)?
                    .into_iter()
                    .map(|record| browser_protocol::ConsoleRecord {
                        sequence: record.sequence,
                        level: match record.level {
                            browser_engine::PageConsoleLevel::Debug => {
                                browser_protocol::ConsoleLevel::Debug
                            }
                            browser_engine::PageConsoleLevel::Log => {
                                browser_protocol::ConsoleLevel::Log
                            }
                            browser_engine::PageConsoleLevel::Info => {
                                browser_protocol::ConsoleLevel::Info
                            }
                            browser_engine::PageConsoleLevel::Warn => {
                                browser_protocol::ConsoleLevel::Warn
                            }
                            browser_engine::PageConsoleLevel::Error => {
                                browser_protocol::ConsoleLevel::Error
                            }
                        },
                        text: record.text,
                    })
                    .collect();
                Ok((Some(tab_id), CommandResult::Console(records)))
            }
            Command::Diagnostic(browser_protocol::DiagnosticCommand::Network) => {
                let tab_id = self.required_tab(profile_id, requested_tab)?;
                let tab = self.tabs.get(&tab_id).ok_or_else(|| {
                    protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not exist"),
                    )
                })?;
                if &tab.profile_id != profile_id {
                    return Err(protocol_error(
                        ErrorCode::TabNotFound,
                        format!("tab {tab_id} does not belong to profile {profile_id}"),
                    ));
                }
                let records = tab
                    .document_worker
                    .network_records()
                    .iter()
                    .map(|record| browser_protocol::NetworkRecord {
                        sequence: record.sequence,
                        request_id: record.request_id.clone(),
                        requested_url: record.requested_url.clone(),
                        final_url: record.final_url.clone(),
                        status: record.status,
                        error: record.error.clone(),
                    })
                    .collect();
                Ok((Some(tab_id), CommandResult::Network(records)))
            }
        }
    }
}
