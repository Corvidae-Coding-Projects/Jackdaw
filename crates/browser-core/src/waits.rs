use std::{
    thread,
    time::{Duration, Instant},
};

use browser_protocol::{
    CommandResult, ErrorCode, ProfileId, ProtocolError, TabId, TargetState, WaitCommand,
};

use crate::{
    broker::BrokerPageFetchOutcome,
    errors::{internal_error, protocol_error, stale_target, tab_not_found},
    service::BrowserService,
    workers::{WorkerQuery, WorkerReply},
};

impl BrowserService {
    /// Advance one ready task for every live tab without requiring a client wait command.
    ///
    /// The daemon calls this on the service-owning thread, preserving Boa's thread affinity while
    /// allowing timers and page Fetch completions to update documents between CLI requests.
    pub fn advance_background_work(&mut self) -> Result<bool, ProtocolError> {
        let tab_ids = self.tabs.keys().cloned().collect::<Vec<_>>();
        let mut advanced = false;
        for tab_id in tab_ids {
            let profile_id = self
                .tabs
                .get(&tab_id)
                .ok_or_else(|| tab_not_found(&tab_id))?
                .profile_id
                .clone();
            let WorkerReply::Advanced(ran_timer) = self
                .tabs
                .get_mut(&tab_id)
                .ok_or_else(|| tab_not_found(&tab_id))?
                .document_worker
                .query(WorkerQuery::RunNextTimer)
                .map_err(internal_error)?
            else {
                return Err(internal_error(
                    "worker returned an unexpected timer-advance reply",
                ));
            };
            let ran_fetch = self.advance_page_fetches(&profile_id, &tab_id)?;
            if ran_timer || ran_fetch {
                self.tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| tab_not_found(&tab_id))?
                    .invalidate_views();
                advanced = true;
            }
        }
        Ok(advanced)
    }

    pub(crate) fn handle_wait(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        command: WaitCommand,
        timeout_ms: u64,
    ) -> Result<CommandResult, ProtocolError> {
        let started = Instant::now();
        let deadline = started
            .checked_add(Duration::from_millis(timeout_ms))
            .ok_or_else(|| crate::errors::invalid_request("wait timeout is too large"))?;
        loop {
            if matches!(command, WaitCommand::Idle) {
                let WorkerReply::Advanced(ran_timer) = self
                    .tab_for_wait(profile_id, &tab_id)?
                    .document_worker
                    .query(WorkerQuery::RunNextTimer)
                    .map_err(internal_error)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected timer-advance reply",
                    ));
                };
                let ran_fetch = self.advance_page_fetches(profile_id, &tab_id)?;
                if !ran_timer && !ran_fetch {
                    return Ok(CommandResult::Empty);
                }
            } else {
                if self.wait_condition_met(profile_id, &tab_id, &command)? {
                    return Ok(CommandResult::Empty);
                }
                let WorkerReply::Advanced(ran_timer) = self
                    .tab_for_wait(profile_id, &tab_id)?
                    .document_worker
                    .query(WorkerQuery::RunNextTimer)
                    .map_err(internal_error)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected timer-advance reply",
                    ));
                };
                let ran_fetch = self.advance_page_fetches(profile_id, &tab_id)?;
                if ran_timer || ran_fetch {
                    continue;
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(protocol_error(
                    ErrorCode::Timeout,
                    format!("wait condition was not met within {timeout_ms} ms"),
                ));
            }
            thread::sleep((deadline - now).min(Duration::from_millis(5)));
        }
    }

    fn tab_for_wait(
        &mut self,
        profile_id: &ProfileId,
        tab_id: &TabId,
    ) -> Result<&mut crate::tabs::TabState, ProtocolError> {
        let tab = self
            .tabs
            .get_mut(tab_id)
            .ok_or_else(|| tab_not_found(tab_id))?;
        if &tab.profile_id != profile_id {
            return Err(tab_not_found(tab_id));
        }
        Ok(tab)
    }

    fn wait_condition_met(
        &mut self,
        profile_id: &ProfileId,
        tab_id: &TabId,
        command: &WaitCommand,
    ) -> Result<bool, ProtocolError> {
        match command {
            WaitCommand::Idle => Ok(false),
            WaitCommand::Url { url } => {
                let tab = self.tab_for_wait(profile_id, tab_id)?;
                Ok(tab.metadata().url == *url)
            }
            WaitCommand::Text { value } => {
                let tab = self.tab_for_wait(profile_id, tab_id)?;
                let WorkerReply::DocumentText(text) = tab
                    .document_worker
                    .query(WorkerQuery::DocumentText)
                    .map_err(internal_error)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected document-text reply",
                    ));
                };
                Ok(text.contains(value))
            }
            WaitCommand::RoleName { role, name } => {
                let tab = self.tab_for_wait(profile_id, tab_id)?;
                if role == "link" {
                    let WorkerReply::Links(links) = tab
                        .document_worker
                        .query(WorkerQuery::Links)
                        .map_err(internal_error)?
                    else {
                        return Err(internal_error("worker returned an unexpected links reply"));
                    };
                    return Ok(links.iter().any(|link| link.name == *name));
                }
                let WorkerReply::Controls(controls) = tab
                    .document_worker
                    .query(WorkerQuery::Controls)
                    .map_err(internal_error)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected controls reply",
                    ));
                };
                Ok(controls
                    .iter()
                    .any(|control| control.role == *role && control.name == *name))
            }
            WaitCommand::TargetState { target, state } => {
                let tab = self.tab_for_wait(profile_id, tab_id)?;
                let engine_target = tab
                    .observed_targets
                    .get(target)
                    .cloned()
                    .ok_or_else(|| stale_target(target))?;
                let attached = match tab
                    .document_worker
                    .query(WorkerQuery::ValidateTarget(engine_target.identity()))
                {
                    Ok(WorkerReply::TargetValidated) => true,
                    Err(_) => false,
                    Ok(_) => {
                        return Err(internal_error(
                            "worker returned an unexpected target-validation reply",
                        ));
                    }
                };
                match state {
                    TargetState::Attached => Ok(attached),
                    TargetState::Detached => Ok(!attached),
                    TargetState::Enabled
                    | TargetState::Disabled
                    | TargetState::Focused
                    | TargetState::Checked
                    | TargetState::Unchecked
                        if !attached =>
                    {
                        Ok(false)
                    }
                    TargetState::Enabled
                    | TargetState::Disabled
                    | TargetState::Focused
                    | TargetState::Checked
                    | TargetState::Unchecked => {
                        let WorkerReply::Controls(controls) = tab
                            .document_worker
                            .query(WorkerQuery::Controls)
                            .map_err(internal_error)?
                        else {
                            return Err(internal_error(
                                "worker returned an unexpected controls reply",
                            ));
                        };
                        let control = controls
                            .into_iter()
                            .find(|control| control.target == engine_target);
                        Ok(control.is_some_and(|control| match state {
                            TargetState::Enabled => !control.disabled,
                            TargetState::Disabled => control.disabled,
                            TargetState::Focused => control.focused,
                            TargetState::Checked => control.checked == Some(true),
                            TargetState::Unchecked => control.checked == Some(false),
                            TargetState::Attached | TargetState::Detached => unreachable!(),
                        }))
                    }
                }
            }
            WaitCommand::Download { download_id } => {
                Ok(self.downloads.get(download_id).is_some_and(|download| {
                    &download.profile_id == profile_id && &download.tab_id == tab_id
                }))
            }
        }
    }
    fn advance_page_fetches(
        &mut self,
        profile_id: &ProfileId,
        tab_id: &TabId,
    ) -> Result<bool, ProtocolError> {
        let requests = self
            .tabs
            .get_mut(tab_id)
            .ok_or_else(|| tab_not_found(tab_id))?
            .document_worker
            .take_page_fetch_requests()
            .map_err(internal_error)?;
        let advanced = !requests.is_empty();
        for request in requests {
            let dispatch = self
                .tabs
                .get(tab_id)
                .ok_or_else(|| tab_not_found(tab_id))?
                .document_worker
                .prepare_page_fetch(&request)
                .map_err(internal_error)?;
            let Some(dispatch) = dispatch else {
                continue;
            };
            let outcome = self.broker.dispatch_page_fetch(profile_id, dispatch);
            let runtime = &mut self
                .tabs
                .get_mut(tab_id)
                .ok_or_else(|| tab_not_found(tab_id))?
                .document_worker;
            match outcome {
                BrokerPageFetchOutcome::Response(response) => {
                    runtime
                        .complete_page_fetch(&request, &response)
                        .map_err(internal_error)?;
                }
                BrokerPageFetchOutcome::Rejected {
                    response: Some(response),
                    message,
                } => {
                    runtime
                        .reject_page_fetch_response(&request, &response, &message)
                        .map_err(internal_error)?;
                }
                BrokerPageFetchOutcome::Rejected {
                    response: None,
                    message,
                } => {
                    runtime
                        .reject_page_fetch(&request, &message)
                        .map_err(internal_error)?;
                }
            }
        }
        Ok(advanced)
    }
}
