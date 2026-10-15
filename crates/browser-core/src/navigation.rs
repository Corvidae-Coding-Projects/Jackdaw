use std::time::Duration;

use browser_engine::NavigationCommit;
use browser_net::RequestControl;
use browser_protocol::{
    CommandResult, EventKind, NavigationCommand, NavigationResult, ProfileId, ProtocolError,
    TabCommand, TabId,
};

use crate::{
    errors::{internal_error, invalid_request, network_error, tab_not_found},
    service::BrowserService,
    tabs::TabState,
    workers::WorkerNavigation,
};

impl BrowserService {
    pub(crate) fn handle_tab_command(
        &mut self,
        profile_id: &ProfileId,
        command: TabCommand,
    ) -> Result<(Option<TabId>, CommandResult), ProtocolError> {
        match command {
            TabCommand::List => {
                self.open_profile(profile_id, true)?;
                Ok((
                    None,
                    CommandResult::Tabs(
                        self.tabs
                            .values()
                            .filter(|tab| &tab.profile_id == profile_id)
                            .map(TabState::summary)
                            .collect(),
                    ),
                ))
            }
            TabCommand::New { url } => {
                self.open_profile(profile_id, true)?;
                let id = TabId::parse(format!("t_{}", self.next_tab))
                    .expect("service-generated tab ID is valid");
                self.next_tab = self
                    .next_tab
                    .checked_add(1)
                    .ok_or_else(|| internal_error("tab identifier space exhausted"))?;
                let mut tab =
                    TabState::new(id.clone(), profile_id.clone()).map_err(internal_error)?;
                if let Some(url) = url
                    && url != "about:blank"
                {
                    let response = self
                        .broker
                        .dispatch_initial_navigation(profile_id, &url)
                        .map_err(network_error)?;
                    let host = self
                        .broker
                        .document_host(profile_id)
                        .map_err(network_error)?;
                    tab.document_worker
                        .commit_navigation(&host, WorkerNavigation::Open, &url, response)
                        .map_err(network_error)?;
                }
                let summary = tab.summary();
                self.workers
                    .attach(profile_id.clone(), id.clone())
                    .map_err(internal_error)?;
                self.tabs.insert(id.clone(), tab);
                self.active_tabs.insert(profile_id.clone(), id.clone());
                self.persist_profile_tabs(profile_id)?;
                Ok((Some(id), CommandResult::Tabs(vec![summary])))
            }
            TabCommand::Close { tab } => {
                let owned = self
                    .tabs
                    .get(&tab)
                    .is_some_and(|state| &state.profile_id == profile_id);
                if !owned {
                    return Err(tab_not_found(&tab));
                }
                let was_active = self.active_tabs.get(profile_id) == Some(&tab);
                self.workers
                    .detach(profile_id, &tab)
                    .map_err(internal_error)?;
                self.tabs.remove(&tab);
                if was_active {
                    if let Some(replacement) = self
                        .tabs
                        .values()
                        .find(|state| &state.profile_id == profile_id)
                        .map(|state| state.id.clone())
                    {
                        self.active_tabs.insert(profile_id.clone(), replacement);
                    } else {
                        self.active_tabs.remove(profile_id);
                    }
                }
                self.persist_profile_tabs(profile_id)?;
                Ok((None, CommandResult::Empty))
            }
            TabCommand::Activate { tab } => {
                let owned = self
                    .tabs
                    .get(&tab)
                    .is_some_and(|state| &state.profile_id == profile_id);
                if !owned {
                    return Err(tab_not_found(&tab));
                }
                self.active_tabs.insert(profile_id.clone(), tab.clone());
                self.persist_profile_tabs(profile_id)?;
                Ok((Some(tab), CommandResult::Empty))
            }
        }
    }

    pub(crate) fn handle_navigation_command(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        command: NavigationCommand,
        timeout_ms: u64,
    ) -> Result<CommandResult, ProtocolError> {
        let owned = self
            .tabs
            .get(&tab_id)
            .is_some_and(|tab| &tab.profile_id == profile_id);
        if !owned {
            return Err(tab_not_found(&tab_id));
        }
        let owner_generation = self
            .workers
            .generation(profile_id, &tab_id)
            .ok_or_else(|| tab_not_found(&tab_id))?;
        if matches!(command, NavigationCommand::Stop) {
            self.navigation_cancellation
                .clear_pending(profile_id, &tab_id);
            return Ok(CommandResult::Empty);
        }

        let requested_url = {
            let tab = self
                .tabs
                .get(&tab_id)
                .expect("owned tab remains present during navigation selection");
            match &command {
                NavigationCommand::Open { url } => url.clone(),
                NavigationCommand::Back => tab
                    .document_worker
                    .navigation_target(WorkerNavigation::Back)
                    .ok_or_else(|| invalid_request("tab has no preceding history entry"))?,
                NavigationCommand::Forward => tab
                    .document_worker
                    .navigation_target(WorkerNavigation::Forward)
                    .ok_or_else(|| invalid_request("tab has no following history entry"))?,
                NavigationCommand::Reload => tab
                    .document_worker
                    .navigation_target(WorkerNavigation::Reload)
                    .ok_or_else(|| invalid_request("tab has no committed document"))?,
                NavigationCommand::Stop => unreachable!("stop returned before network dispatch"),
            }
        };
        let control = RequestControl::for_timeout(Duration::from_millis(timeout_ms))
            .map_err(network_error)?;
        self.navigation_cancellation
            .register(profile_id.clone(), tab_id.clone(), control.clone());
        let brokered = self
            .broker
            .dispatch_navigation(profile_id, &requested_url, &control)
            .map_err(network_error);
        let host = self
            .broker
            .document_host(profile_id)
            .map_err(network_error)?;
        let outcome = {
            let tab = self
                .tabs
                .get_mut(&tab_id)
                .expect("owned tab remains present during navigation commit");
            brokered.and_then(|response| {
                let (navigation, missing_message) = match command {
                    NavigationCommand::Open { .. } => (WorkerNavigation::Open, None),
                    NavigationCommand::Back => (
                        WorkerNavigation::Back,
                        Some("tab has no preceding history entry"),
                    ),
                    NavigationCommand::Forward => (
                        WorkerNavigation::Forward,
                        Some("tab has no following history entry"),
                    ),
                    NavigationCommand::Reload => (
                        WorkerNavigation::Reload,
                        Some("tab has no committed document"),
                    ),
                    NavigationCommand::Stop => {
                        unreachable!("stop returned before network dispatch")
                    }
                };
                tab.document_worker
                    .commit_navigation(&host, navigation, &requested_url, response)
                    .map_err(network_error)
                    .and_then(|commit| match (commit, missing_message) {
                        (Some(commit), _) => Ok(commit),
                        (None, Some(message)) => Err(invalid_request(message)),
                        (None, None) => Err(internal_error(
                            "document worker omitted an open-navigation commit",
                        )),
                    })
            })
        };
        self.navigation_cancellation.finish(profile_id, &tab_id);
        if !self
            .workers
            .is_current(profile_id, &tab_id, owner_generation)
        {
            return Err(internal_error(format!(
                "discarded navigation completion from obsolete document owner for tab {tab_id}"
            )));
        }
        let commit = match outcome {
            Ok(commit) => commit,
            Err(error) => {
                let owner_failed = self
                    .tabs
                    .get(&tab_id)
                    .is_some_and(|tab| !tab.document_worker.is_healthy());
                if owner_failed {
                    let tab = self
                        .tabs
                        .get_mut(&tab_id)
                        .expect("failed tab remains present during owner replacement");
                    tab.replace_failed_document_owner()
                        .map_err(internal_error)?;
                    self.workers
                        .replace(profile_id, &tab_id)
                        .map_err(internal_error)?;
                    self.events.push(
                        profile_id,
                        EventKind::WorkerCrashed {
                            tab_id: tab_id.clone(),
                        },
                    );
                }
                return Err(error);
            }
        };
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .expect("navigated tab remains present after synchronous commit");
        tab.invalidate_views();
        let result = navigation_result(tab, commit);
        self.persist_profile_tabs(profile_id)?;
        Ok(CommandResult::Navigation(result))
    }
}

pub(crate) fn navigation_result(tab: &TabState, commit: NavigationCommit) -> NavigationResult {
    NavigationResult {
        url: commit.final_url,
        title: tab.metadata().title,
        loading: false,
        document_id: tab.document_id(),
    }
}
