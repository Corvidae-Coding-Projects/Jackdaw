use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use browser_net::BrowserClient;
use browser_protocol::{
    CommandReply, CommandRequest, EffectState, OperationId, OperationReceipt, OperationStatus,
    PROTOCOL_VERSION, ProfileCommand, ProfileId, ProtocolError, ReplyTarget, RequestId,
};
use browser_store::{OperationPhase, TabSessionRecord};

use crate::{
    errors::{internal_error, invalid_request, network_error, profile_not_found},
    operations::CompletedOperation,
    service::BrowserService,
    tabs::TabState,
};

impl BrowserService {
    pub(crate) fn ensure_profile_root(&self) -> Result<Option<&Path>, ProtocolError> {
        let Some(root) = self.profile_root.as_deref() else {
            return Ok(None);
        };
        fs::create_dir_all(root)
            .map_err(|error| internal_error(format!("could not create profile root: {error}")))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|error| internal_error(format!("could not protect profile root: {error}")))?;
        Ok(Some(root))
    }

    fn profile_path(&self, profile_id: &ProfileId) -> Result<Option<PathBuf>, ProtocolError> {
        Ok(self
            .ensure_profile_root()?
            .map(|root| root.join(format!("{}.redb", profile_id.as_str()))))
    }

    fn profile_exists(&self, profile_id: &ProfileId) -> Result<bool, ProtocolError> {
        if self.known_profiles.contains(profile_id) || self.broker.contains_profile(profile_id) {
            return Ok(true);
        }
        Ok(self
            .profile_path(profile_id)?
            .is_some_and(|path| path.is_file()))
    }

    fn list_profiles(&self) -> Result<Vec<ProfileId>, ProtocolError> {
        let mut profiles = self.known_profiles.clone();
        profiles.extend(self.broker.profiles().cloned());
        if let Some(root) = self.ensure_profile_root()? {
            for entry in fs::read_dir(root).map_err(internal_error)? {
                let entry = entry.map_err(internal_error)?;
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("redb") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|value| value.to_str())
                    && let Ok(profile) = ProfileId::parse(stem)
                {
                    profiles.insert(profile);
                }
            }
        }
        Ok(profiles.into_iter().collect())
    }

    pub(crate) fn open_profile(
        &mut self,
        profile_id: &ProfileId,
        create: bool,
    ) -> Result<BrowserClient, ProtocolError> {
        if let Some(client) = self.broker.client(profile_id) {
            return Ok(client.clone());
        }
        if !create && !self.profile_exists(profile_id)? {
            return Err(profile_not_found(profile_id));
        }
        let client = match self.profile_path(profile_id)? {
            Some(path) => {
                BrowserClient::open_profile_with_config(path, self.network_config.clone())
                    .map_err(network_error)?
            }
            None => {
                BrowserClient::with_config(self.network_config.clone()).map_err(network_error)?
            }
        };

        let operation_records = client.load_operation_records().map_err(network_error)?;
        let operation_tombstones = client.load_operation_tombstones().map_err(network_error)?;
        let mut restored_tombstones = Vec::with_capacity(operation_tombstones.len());
        for request_id in operation_tombstones {
            let request_id = RequestId::parse(request_id).map_err(|error| {
                internal_error(format!("stored operation tombstone is invalid: {error}"))
            })?;
            if restored_tombstones.contains(&request_id) {
                return Err(internal_error(format!(
                    "stored operation tombstone {request_id} is duplicated"
                )));
            }
            restored_tombstones.push(request_id);
        }
        let mut restored_operations = Vec::with_capacity(operation_records.len());
        for mut record in operation_records {
            let request =
                serde_json::from_str::<CommandRequest>(&record.request_json).map_err(|error| {
                    internal_error(format!("stored operation request is invalid: {error}"))
                })?;
            if request.request_id.as_str() != record.request_id || &request.profile_id != profile_id
            {
                return Err(internal_error(
                    "stored operation identity does not match its profile",
                ));
            }

            let reply = match record.phase {
                OperationPhase::Terminal => {
                    let reply_json = record.reply_json.as_deref().ok_or_else(|| {
                        internal_error("terminal operation is missing its durable reply")
                    })?;
                    serde_json::from_str::<CommandReply>(reply_json).map_err(|error| {
                        internal_error(format!("stored operation reply is invalid: {error}"))
                    })?
                }
                OperationPhase::Accepted | OperationPhase::Dispatched => {
                    let effect = if record.phase == OperationPhase::Accepted {
                        EffectState::None
                    } else {
                        EffectState::Unknown
                    };
                    let mut error = internal_error(match record.phase {
                        OperationPhase::Accepted => {
                            "operation was interrupted before dispatch and was not replayed"
                        }
                        OperationPhase::Dispatched => {
                            "operation was interrupted after dispatch and was not replayed"
                        }
                        OperationPhase::Terminal => unreachable!(),
                    });
                    error.effect = effect;
                    let operation_id = OperationId::parse(format!("op_{}", record.sequence))
                        .map_err(|error| {
                            internal_error(format!("stored operation sequence is invalid: {error}"))
                        })?;
                    let reply = CommandReply {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: request.request_id.clone(),
                        success: false,
                        target: Some(ReplyTarget {
                            profile_id: profile_id.clone(),
                            tab_id: request.tab_id.clone(),
                            document_id: None,
                        }),
                        receipt: Some(OperationReceipt {
                            id: operation_id,
                            status: OperationStatus::Failed,
                            effect,
                        }),
                        result: None,
                        error: Some(error),
                    };
                    record.phase = OperationPhase::Terminal;
                    record.reply_json = Some(
                        serde_json::to_string(&reply)
                            .expect("recovered operation reply serializes"),
                    );
                    client
                        .put_operation_record(&record, 1_024)
                        .map_err(network_error)?;
                    reply
                }
            };
            if reply.request_id != request.request_id {
                return Err(internal_error(
                    "stored operation reply identity does not match its request",
                ));
            }
            let operation = CompletedOperation { request, reply };
            if restored_tombstones.contains(&operation.request.request_id) {
                return Err(internal_error(format!(
                    "stored operation request ID {} is both retained and expired",
                    operation.request.request_id
                )));
            }
            if let Some(existing) = self
                .operations
                .get(profile_id, &operation.request.request_id)
            {
                if existing == &operation {
                    continue;
                }
                return Err(internal_error(format!(
                    "stored operation request ID {} conflicts with the in-memory operation",
                    operation.request.request_id
                )));
            }
            if restored_operations
                .iter()
                .any(|candidate: &CompletedOperation| {
                    candidate.request.request_id == operation.request.request_id
                })
            {
                return Err(internal_error(format!(
                    "stored operation request ID {} is duplicated",
                    operation.request.request_id
                )));
            }
            restored_operations.push(operation);
        }

        let sessions = client.load_tab_sessions().map_err(network_error)?;
        let mut restored_tabs = Vec::with_capacity(sessions.len());
        let mut restored_active = None;
        for session in sessions {
            let active = session.active;
            let tab_id = browser_protocol::TabId::parse(session.tab_id).map_err(|error| {
                internal_error(format!("stored profile tab ID is invalid: {error}"))
            })?;
            if self.tabs.contains_key(&tab_id) {
                return Err(internal_error(format!(
                    "stored profile tab ID {tab_id} conflicts with an open tab"
                )));
            }
            let mut tab =
                TabState::new(tab_id.clone(), profile_id.clone()).map_err(internal_error)?;
            self.broker
                .restore_profile_tab(
                    &client,
                    &mut tab.document_worker,
                    session.history,
                    session.cursor,
                )
                .map_err(network_error)?;
            if let Some(sequence) = tab_id
                .as_str()
                .strip_prefix("t_")
                .and_then(|value| value.parse::<u64>().ok())
            {
                self.next_tab = self.next_tab.max(sequence.saturating_add(1));
            }
            if active && restored_active.replace(tab_id.clone()).is_some() {
                return Err(internal_error(
                    "stored profile has more than one active tab",
                ));
            }
            restored_tabs.push((tab_id, tab));
        }
        let fallback_active = restored_tabs.first().map(|(tab_id, _)| tab_id.clone());
        let mut attached_tabs = Vec::with_capacity(restored_tabs.len());
        for (tab_id, _) in &restored_tabs {
            if let Err(error) = self.workers.attach(profile_id.clone(), tab_id.clone()) {
                for attached_tab in attached_tabs {
                    let _ = self.workers.detach(profile_id, &attached_tab);
                }
                return Err(internal_error(error));
            }
            attached_tabs.push(tab_id.clone());
        }
        self.known_profiles.insert(profile_id.clone());
        self.broker.insert(profile_id.clone(), client.clone());
        self.tabs.extend(restored_tabs);
        if let Some(active) = restored_active.or(fallback_active) {
            self.active_tabs.insert(profile_id.clone(), active);
        }
        for request_id in restored_tombstones {
            self.operations
                .restore_expired(profile_id.clone(), request_id);
        }
        for operation in restored_operations {
            self.operations.restore(profile_id.clone(), operation);
        }
        Ok(client)
    }

    pub(crate) fn persist_profile_tabs(&self, profile_id: &ProfileId) -> Result<(), ProtocolError> {
        let Some(client) = self.broker.client(profile_id) else {
            return Ok(());
        };
        let sessions = self
            .tabs
            .values()
            .filter(|tab| &tab.profile_id == profile_id)
            .filter_map(|tab| {
                tab.document_worker
                    .navigation_snapshot()
                    .map(|(history, cursor)| TabSessionRecord {
                        tab_id: tab.id.as_str().to_owned(),
                        history,
                        cursor,
                        active: self.active_tabs.get(profile_id) == Some(&tab.id),
                    })
            })
            .collect::<Vec<_>>();
        client
            .replace_tab_sessions(&sessions)
            .map_err(network_error)
    }

    fn close_profile(&mut self, profile_id: &ProfileId) -> Result<(), ProtocolError> {
        if self.tabs.values().any(|tab| &tab.profile_id == profile_id) {
            return Err(invalid_request("profile still owns open tabs"));
        }
        if self.broker.remove(profile_id).is_none() {
            return Err(profile_not_found(profile_id));
        }
        self.active_tabs.remove(profile_id);
        Ok(())
    }

    fn remove_profile(&mut self, profile_id: &ProfileId) -> Result<(), ProtocolError> {
        if self.tabs.values().any(|tab| &tab.profile_id == profile_id) {
            return Err(invalid_request("profile still owns open tabs"));
        }
        if !self.profile_exists(profile_id)? {
            return Err(profile_not_found(profile_id));
        }
        self.broker.remove(profile_id);
        self.active_tabs.remove(profile_id);
        if let Some(path) = self.profile_path(profile_id)?
            && path.exists()
        {
            fs::remove_file(&path).map_err(|error| {
                internal_error(format!(
                    "could not remove profile {}: {error}",
                    path.display()
                ))
            })?;
        }
        self.known_profiles.remove(profile_id);
        Ok(())
    }

    pub(crate) fn handle_profile_command(
        &mut self,
        _profile_id: &ProfileId,
        command: ProfileCommand,
    ) -> Result<Vec<ProfileId>, ProtocolError> {
        match command {
            ProfileCommand::Create { profile } => {
                if self.profile_exists(&profile)? {
                    return Err(invalid_request(format!("profile {profile} already exists")));
                }
                self.open_profile(&profile, true)?;
                Ok(vec![profile])
            }
            ProfileCommand::List => self.list_profiles(),
            ProfileCommand::Open { profile } => {
                self.open_profile(&profile, false)?;
                Ok(vec![profile])
            }
            ProfileCommand::Close { profile } => {
                self.close_profile(&profile)?;
                Ok(vec![profile])
            }
            ProfileCommand::Remove { profile } => {
                self.remove_profile(&profile)?;
                Ok(vec![profile])
            }
        }
    }
}
