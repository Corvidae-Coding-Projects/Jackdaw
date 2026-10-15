use std::time::Instant;

use browser_protocol::{
    CommandResult, ControlStates, DocumentReadyState, ElementRef, FrameId, ObservationMode,
    ObservedControl, ObservedItem, ObservedLink, ObservedText, ProfileId, ProtocolError, TabId,
    ViewId,
};

fn contains_case_insensitive(value: &str, query: &str) -> bool {
    value.to_lowercase().contains(query)
}

fn item_matches(item: &ObservedItem, query: &str) -> bool {
    match item {
        ObservedItem::Text(text) => contains_case_insensitive(&text.text, query),
        ObservedItem::Control(control) => {
            contains_case_insensitive(&control.role, query)
                || contains_case_insensitive(&control.name, query)
                || control
                    .value
                    .as_deref()
                    .is_some_and(|value| contains_case_insensitive(value, query))
        }
        ObservedItem::Link(link) => {
            contains_case_insensitive(&link.name, query)
                || contains_case_insensitive(&link.url, query)
        }
        ObservedItem::Frame(frame) => {
            frame
                .name
                .as_deref()
                .is_some_and(|name| contains_case_insensitive(name, query))
                || contains_case_insensitive(&frame.url, query)
        }
    }
}

use crate::{
    errors::{internal_error, stale_view, tab_not_found, view_expired},
    observations::ViewSnapshot,
    service::BrowserService,
    workers::{WorkerQuery, WorkerReply},
};

impl BrowserService {
    pub(crate) fn handle_observe(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        observe: browser_protocol::ObserveCommand,
    ) -> Result<CommandResult, ProtocolError> {
        if observe.max_items == 0 {
            return Err(crate::errors::invalid_request(
                "max_items must be greater than zero",
            ));
        }
        if let Some(cursor) = observe.cursor {
            let tab = self
                .tabs
                .get_mut(&tab_id)
                .ok_or_else(|| tab_not_found(&tab_id))?;
            if &tab.profile_id != profile_id {
                return Err(tab_not_found(&tab_id));
            }
            tab.prune_views(self.view_retention, Instant::now());
            let snapshot = tab.observed_views.get(&cursor.view_id).ok_or_else(|| {
                if tab.expired_views.contains(&cursor.view_id) {
                    view_expired(&cursor.view_id)
                } else {
                    stale_view(&cursor.view_id)
                }
            })?;
            return Ok(CommandResult::Observation(
                snapshot.page(cursor.offset, observe.max_items)?,
            ));
        }

        let view_sequence = self.next_view;
        self.next_view = self
            .next_view
            .checked_add(1)
            .ok_or_else(|| internal_error("view identifier space exhausted"))?;
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .ok_or_else(|| tab_not_found(&tab_id))?;
        if &tab.profile_id != profile_id {
            return Err(tab_not_found(&tab_id));
        }
        let metadata = tab.metadata();
        let generation = metadata.generation;
        let view_id = ViewId::parse(format!(
            "v_{}_{}_{view_sequence:020}",
            tab_id.as_str().trim_start_matches("t_"),
            generation
        ))
        .expect("service-generated view ID is valid");
        let frame_id = FrameId::parse(format!("f_{}", tab_id.as_str().trim_start_matches("t_")))
            .expect("service-generated frame ID is valid");
        let mut items = Vec::new();
        if matches!(observe.mode, ObservationMode::Text | ObservationMode::All) {
            let WorkerReply::DocumentText(text) = tab
                .document_worker
                .query(WorkerQuery::DocumentText)
                .map_err(internal_error)?
            else {
                return Err(internal_error(
                    "worker returned an unexpected document-text reply",
                ));
            };
            items.push(ObservedItem::Text(ObservedText { text }));
        }
        if matches!(
            observe.mode,
            ObservationMode::Controls | ObservationMode::All
        ) {
            let WorkerReply::Controls(controls) = tab
                .document_worker
                .query(WorkerQuery::Controls)
                .map_err(internal_error)?
            else {
                return Err(internal_error(
                    "worker returned an unexpected controls reply",
                ));
            };
            for (index, control) in controls.into_iter().enumerate() {
                let target = ElementRef::parse(format!("{}:e_{}", view_id.as_str(), index + 1))
                    .expect("service-generated element reference is valid");
                tab.observed_targets.insert(target.clone(), control.target);
                items.push(ObservedItem::Control(ObservedControl {
                    target,
                    role: control.role,
                    name: control.name,
                    value: control.value,
                    states: ControlStates {
                        disabled: control.disabled,
                        focused: control.focused,
                        checked: control.checked,
                        selected: None,
                        expanded: None,
                        required: control.required,
                        invalid: control.invalid,
                    },
                }));
            }
        }
        if matches!(observe.mode, ObservationMode::Links | ObservationMode::All) {
            let WorkerReply::Links(links) = tab
                .document_worker
                .query(WorkerQuery::Links)
                .map_err(internal_error)?
            else {
                return Err(internal_error("worker returned an unexpected links reply"));
            };
            for (index, link) in links.into_iter().enumerate() {
                let target = ElementRef::parse(format!("{}:e_link{}", view_id.as_str(), index + 1))
                    .expect("service-generated link reference is valid");
                tab.observed_targets.insert(target.clone(), link.target);
                tab.observed_link_urls
                    .insert(target.clone(), link.url.clone());
                items.push(ObservedItem::Link(ObservedLink {
                    target,
                    name: link.name,
                    url: link.url,
                }));
            }
        }
        let mut snapshot = ViewSnapshot {
            view_id: view_id.clone(),
            document_id: tab.document_id(),
            frame_id,
            url: metadata.url,
            title: metadata.title,
            loading: false,
            ready_state: DocumentReadyState::Complete,
            items,
            retained_at: Instant::now(),
            retained_bytes: 0,
        };
        snapshot.retained_bytes = snapshot.measure_retained_bytes();
        tab.observed_views.insert(view_id, snapshot.clone());
        tab.prune_views(self.view_retention, Instant::now());
        Ok(CommandResult::Observation(
            snapshot.page(0, observe.max_items)?,
        ))
    }

    pub(crate) fn handle_read(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        read: browser_protocol::ReadCommand,
    ) -> Result<CommandResult, ProtocolError> {
        self.handle_observe(
            profile_id,
            tab_id,
            browser_protocol::ObserveCommand {
                mode: ObservationMode::Text,
                frame_id: read.frame_id,
                cursor: read.cursor,
                max_items: read.max_items,
            },
        )
    }

    pub(crate) fn handle_find(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        find: browser_protocol::FindCommand,
    ) -> Result<CommandResult, ProtocolError> {
        let query = find.query.trim().to_lowercase();
        if query.is_empty() {
            return Err(crate::errors::invalid_request(
                "find query must not be empty",
            ));
        }
        if find.max_items == 0 {
            return Err(crate::errors::invalid_request(
                "max_items must be greater than zero",
            ));
        }

        let result = self.handle_observe(
            profile_id,
            tab_id.clone(),
            browser_protocol::ObserveCommand {
                mode: ObservationMode::All,
                frame_id: find.frame_id,
                cursor: None,
                max_items: u32::MAX,
            },
        )?;
        let CommandResult::Observation(observation) = result else {
            return Err(internal_error(
                "find observation returned an unexpected result kind",
            ));
        };
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .ok_or_else(|| tab_not_found(&tab_id))?;
        let snapshot = tab
            .observed_views
            .get_mut(&observation.view_id)
            .ok_or_else(|| internal_error("find observation was not retained"))?;
        snapshot.items.retain(|item| item_matches(item, &query));
        snapshot.retained_bytes = snapshot.measure_retained_bytes();
        Ok(CommandResult::Observation(
            snapshot.page(0, find.max_items)?,
        ))
    }
}
