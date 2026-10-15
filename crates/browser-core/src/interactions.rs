use std::time::Instant;

use browser_protocol::{
    CommandResult, FrameInput, FrameInputCommand, InteractionCommand, KeyPhase, PointerButton,
    PointerPhase, ProfileId, ProtocolError, ScrollResult, TabId,
};

use crate::{
    errors::{
        internal_error, invalid_request, network_error, stale_target, stale_view, tab_not_found,
        target_not_interactable, view_expired,
    },
    navigation::navigation_result,
    service::BrowserService,
    workers::{WorkerMutation, WorkerQuery, WorkerReply},
};

impl BrowserService {
    pub(crate) fn handle_interaction(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        interaction: InteractionCommand,
    ) -> Result<CommandResult, ProtocolError> {
        let owned = self
            .tabs
            .get(&tab_id)
            .is_some_and(|tab| &tab.profile_id == profile_id)
            && self.workers.generation(profile_id, &tab_id).is_some();
        if !owned {
            return Err(tab_not_found(&tab_id));
        }
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .expect("owned tab remains present during interaction dispatch");
        match interaction {
            InteractionCommand::Click { target } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                let default_enabled = tab
                    .document_worker
                    .click(&engine_target)
                    .map_err(target_not_interactable)?;
                if default_enabled {
                    if let Some(url) = tab.observed_link_urls.get(&target).cloned() {
                        let control = browser_net::RequestControl::for_timeout(
                            std::time::Duration::from_secs(30),
                        )
                        .map_err(network_error)?;
                        let response = self
                            .broker
                            .dispatch_navigation(profile_id, &url, &control)
                            .map_err(network_error)?;
                        let host = self
                            .broker
                            .document_host(profile_id)
                            .map_err(network_error)?;
                        let commit = tab
                            .document_worker
                            .commit_brokered_navigation(&host, &url, response)
                            .map_err(network_error)?;
                        tab.invalidate_views();
                        let result = navigation_result(tab, commit);
                        self.persist_profile_tabs(profile_id)?;
                        return Ok(CommandResult::Navigation(result));
                    }
                    if let Some(submission) = tab
                        .document_worker
                        .submission_after_click(&engine_target)
                        .map_err(target_not_interactable)?
                    {
                        let prepared = tab
                            .document_worker
                            .prepare_form_submission(submission)
                            .map_err(network_error)?;
                        let (requested_url, response) = self
                            .broker
                            .dispatch_form_submission(profile_id, prepared)
                            .map_err(network_error)?;
                        let host = self
                            .broker
                            .document_host(profile_id)
                            .map_err(network_error)?;
                        let commit = tab
                            .document_worker
                            .commit_brokered_form_submission(&host, requested_url, response)
                            .map_err(network_error)?;
                        tab.invalidate_views();
                        let result = navigation_result(tab, commit);
                        self.persist_profile_tabs(profile_id)?;
                        return Ok(CommandResult::Navigation(result));
                    }
                }
            }
            InteractionCommand::ClickAt { x, y } => {
                let WorkerReply::Target(Some(engine_target)) = tab
                    .document_worker
                    .query(WorkerQuery::HitTest { x, y })
                    .map_err(target_not_interactable)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected hit-test reply",
                    ));
                };
                let default_enabled = tab
                    .document_worker
                    .click(&engine_target)
                    .map_err(target_not_interactable)?;
                if default_enabled {
                    let WorkerReply::LinkUrl(link_url) = tab
                        .document_worker
                        .query(WorkerQuery::LinkUrl(engine_target.identity()))
                        .map_err(target_not_interactable)?
                    else {
                        return Err(internal_error(
                            "worker returned an unexpected link-URL reply",
                        ));
                    };
                    if let Some(url) = link_url {
                        let control = browser_net::RequestControl::for_timeout(
                            std::time::Duration::from_secs(30),
                        )
                        .map_err(network_error)?;
                        let response = self
                            .broker
                            .dispatch_navigation(profile_id, &url, &control)
                            .map_err(network_error)?;
                        let host = self
                            .broker
                            .document_host(profile_id)
                            .map_err(network_error)?;
                        let commit = tab
                            .document_worker
                            .commit_brokered_navigation(&host, &url, response)
                            .map_err(network_error)?;
                        tab.invalidate_views();
                        let result = navigation_result(tab, commit);
                        self.persist_profile_tabs(profile_id)?;
                        return Ok(CommandResult::Navigation(result));
                    }
                    if let Some(submission) = tab
                        .document_worker
                        .submission_after_click(&engine_target)
                        .map_err(target_not_interactable)?
                    {
                        let prepared = tab
                            .document_worker
                            .prepare_form_submission(submission)
                            .map_err(network_error)?;
                        let (requested_url, response) = self
                            .broker
                            .dispatch_form_submission(profile_id, prepared)
                            .map_err(network_error)?;
                        let host = self
                            .broker
                            .document_host(profile_id)
                            .map_err(network_error)?;
                        let commit = tab
                            .document_worker
                            .commit_brokered_form_submission(&host, requested_url, response)
                            .map_err(network_error)?;
                        tab.invalidate_views();
                        let result = navigation_result(tab, commit);
                        self.persist_profile_tabs(profile_id)?;
                        return Ok(CommandResult::Navigation(result));
                    }
                }
            }
            InteractionCommand::Focus { target } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                tab.document_worker
                    .mutate(WorkerMutation::Focus(engine_target.identity()))
                    .map_err(target_not_interactable)?;
            }
            InteractionCommand::Fill { target, value } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                tab.document_worker
                    .mutate(WorkerMutation::Fill {
                        target: engine_target.identity(),
                        value,
                    })
                    .map_err(target_not_interactable)?;
            }
            InteractionCommand::Type { target, text } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                tab.document_worker
                    .mutate(WorkerMutation::TypeText {
                        target: engine_target.identity(),
                        text,
                    })
                    .map_err(target_not_interactable)?;
            }
            InteractionCommand::Key {
                target: Some(target),
                key,
                modifiers,
            } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                tab.document_worker
                    .key(&engine_target, &key, &modifiers)
                    .map_err(target_not_interactable)?;
            }
            InteractionCommand::Select { target, values } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                tab.document_worker
                    .select(&engine_target, &values)
                    .map_err(target_not_interactable)?;
            }
            InteractionCommand::Check { target, checked } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                tab.document_worker
                    .mutate(WorkerMutation::Check {
                        target: engine_target.identity(),
                        checked,
                    })
                    .map_err(target_not_interactable)?;
            }
            InteractionCommand::Submit { target } => {
                let engine_target = tab
                    .observed_targets
                    .get(&target)
                    .cloned()
                    .ok_or_else(|| stale_target(&target))?;
                let (form, submitter) = tab
                    .document_worker
                    .resolve_submission_target(&engine_target)
                    .map_err(target_not_interactable)?;
                let Some(submission) = tab
                    .document_worker
                    .submit(&form, submitter.as_ref())
                    .map_err(target_not_interactable)?
                else {
                    return Ok(CommandResult::Empty);
                };
                let prepared = tab
                    .document_worker
                    .prepare_form_submission(submission)
                    .map_err(network_error)?;
                let (requested_url, response) = self
                    .broker
                    .dispatch_form_submission(profile_id, prepared)
                    .map_err(network_error)?;
                let host = self
                    .broker
                    .document_host(profile_id)
                    .map_err(network_error)?;
                let commit = tab
                    .document_worker
                    .commit_brokered_form_submission(&host, requested_url, response)
                    .map_err(network_error)?;
                tab.invalidate_views();
                let result = navigation_result(tab, commit);
                self.persist_profile_tabs(profile_id)?;
                return Ok(CommandResult::Navigation(result));
            }
            InteractionCommand::Key { target: None, .. } => {
                return Err(invalid_request("key requires an explicit target"));
            }
            InteractionCommand::Scroll {
                target,
                delta_x,
                delta_y,
            } => {
                if let Some(target) = target {
                    let engine_target = tab
                        .observed_targets
                        .get(&target)
                        .cloned()
                        .ok_or_else(|| stale_target(&target))?;
                    let WorkerReply::TargetValidated = tab
                        .document_worker
                        .query(WorkerQuery::ValidateTarget(engine_target.identity()))
                        .map_err(target_not_interactable)?
                    else {
                        return Err(internal_error(
                            "worker returned an unexpected target-validation reply",
                        ));
                    };
                }
                let Some((offset_x, offset_y)) = tab
                    .document_worker
                    .mutate(WorkerMutation::ScrollBy { delta_x, delta_y })
                    .map_err(target_not_interactable)?
                else {
                    return Err(internal_error(
                        "worker returned no scroll offset for a scroll mutation",
                    ));
                };
                tab.invalidate_views();
                return Ok(CommandResult::Scroll(ScrollResult { offset_x, offset_y }));
            }
        }
        Ok(CommandResult::Empty)
    }

    pub(crate) fn handle_frame_input(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        command: FrameInputCommand,
    ) -> Result<CommandResult, ProtocolError> {
        let input_view_id = command.view_id.clone();
        {
            let tab = self
                .tabs
                .get_mut(&tab_id)
                .ok_or_else(|| tab_not_found(&tab_id))?;
            if &tab.profile_id != profile_id {
                return Err(tab_not_found(&tab_id));
            }
            tab.prune_views(self.view_retention, Instant::now());
            let document_id = tab.document_id();
            let retained_view = tab
                .observed_views
                .get(&command.view_id)
                .is_some_and(|snapshot| snapshot.document_id == document_id);
            let key_input_lease = matches!(&command.input, FrameInput::Key { .. })
                && tab
                    .key_input_view
                    .as_ref()
                    .is_some_and(|(view_id, lease_document_id)| {
                        view_id == &command.view_id && lease_document_id == &document_id
                    });
            if !retained_view && !key_input_lease {
                return Err(if tab.expired_views.contains(&command.view_id) {
                    view_expired(&command.view_id)
                } else {
                    stale_view(&command.view_id)
                });
            }
        }

        match command.input {
            FrameInput::Pointer {
                phase: PointerPhase::Down,
                x,
                y,
                button: Some(PointerButton::Primary),
            } => {
                let x = frame_coordinate(x, "pointer x")?;
                let y = frame_coordinate(y, "pointer y")?;
                let tab = self
                    .tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| tab_not_found(&tab_id))?;
                let WorkerReply::Target(Some(target)) = tab
                    .document_worker
                    .query(WorkerQuery::HitTest { x, y })
                    .map_err(target_not_interactable)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected pointer hit-test reply",
                    ));
                };
                tab.pointer_press = Some(target);
                Ok(CommandResult::Empty)
            }
            FrameInput::Pointer {
                phase: PointerPhase::Up,
                x,
                y,
                button: Some(PointerButton::Primary),
            } => {
                let x = frame_coordinate(x, "pointer x")?;
                let y = frame_coordinate(y, "pointer y")?;
                let matches_press = {
                    let tab = self
                        .tabs
                        .get_mut(&tab_id)
                        .ok_or_else(|| tab_not_found(&tab_id))?;
                    let released = match tab.document_worker.query(WorkerQuery::HitTest { x, y }) {
                        Ok(WorkerReply::Target(target)) => target,
                        Ok(_) | Err(_) => None,
                    };
                    let pressed = tab.pointer_press.take();
                    pressed.as_ref() == released.as_ref() && pressed.is_some()
                };
                if !matches_press {
                    return Ok(CommandResult::Empty);
                }
                let result = self.handle_interaction(
                    profile_id,
                    tab_id.clone(),
                    InteractionCommand::ClickAt { x, y },
                )?;
                let tab = self
                    .tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| tab_not_found(&tab_id))?;
                tab.invalidate_views();
                tab.key_input_view = Some((input_view_id, tab.document_id()));
                Ok(result)
            }
            FrameInput::Pointer {
                phase: PointerPhase::Up,
                ..
            } => {
                self.tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| tab_not_found(&tab_id))?
                    .pointer_press = None;
                Ok(CommandResult::Empty)
            }
            FrameInput::Pointer { .. }
            | FrameInput::Key {
                phase: KeyPhase::Up,
                ..
            } => Ok(CommandResult::Empty),
            FrameInput::Key {
                phase: KeyPhase::Down,
                key,
                text,
            } => {
                let tab = self
                    .tabs
                    .get_mut(&tab_id)
                    .ok_or_else(|| tab_not_found(&tab_id))?;
                let WorkerReply::Target(target) = tab
                    .document_worker
                    .query(WorkerQuery::FocusedControl)
                    .map_err(target_not_interactable)?
                else {
                    return Err(internal_error(
                        "worker returned an unexpected focused-control reply",
                    ));
                };
                let target = target
                    .ok_or_else(|| invalid_request("frame key input requires a focused control"))?;
                let key = text
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or(&key);
                tab.document_worker
                    .key(&target, key, &[])
                    .map_err(target_not_interactable)?;
                tab.invalidate_views();
                tab.key_input_view = Some((input_view_id, tab.document_id()));
                Ok(CommandResult::Empty)
            }
            FrameInput::Scroll {
                delta_x, delta_y, ..
            } => self.handle_interaction(
                profile_id,
                tab_id,
                InteractionCommand::Scroll {
                    target: None,
                    delta_x: frame_delta(delta_x, "scroll delta_x")?,
                    delta_y: frame_delta(delta_y, "scroll delta_y")?,
                },
            ),
        }
    }
}

fn frame_coordinate(value: f32, field: &str) -> Result<i32, ProtocolError> {
    if !value.is_finite() || value < 0.0 || value > i32::MAX as f32 {
        return Err(invalid_request(format!(
            "{field} is outside the viewport coordinate range"
        )));
    }
    Ok(value.round() as i32)
}

fn frame_delta(value: f32, field: &str) -> Result<i32, ProtocolError> {
    if !value.is_finite() || value < i32::MIN as f32 || value > i32::MAX as f32 {
        return Err(invalid_request(format!(
            "{field} is outside the supported range"
        )));
    }
    Ok(value.round() as i32)
}
