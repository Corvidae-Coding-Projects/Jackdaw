use std::collections::{BTreeMap, BTreeSet};

use browser_protocol::{CommandReply, CommandRequest, OperationId, ProfileId, RequestId};

#[derive(Clone, PartialEq)]
pub(crate) struct CompletedOperation {
    pub(crate) request: CommandRequest,
    pub(crate) reply: CommandReply,
}

pub(crate) struct OperationRegistry {
    completed: BTreeMap<(ProfileId, RequestId), CompletedOperation>,
    expired: BTreeSet<(ProfileId, RequestId)>,
    next_sequence: u64,
}

impl OperationRegistry {
    pub(crate) fn new() -> Self {
        Self {
            completed: BTreeMap::new(),
            expired: BTreeSet::new(),
            next_sequence: 1,
        }
    }

    pub(crate) fn get(
        &self,
        profile_id: &ProfileId,
        request_id: &RequestId,
    ) -> Option<&CompletedOperation> {
        self.completed
            .get(&(profile_id.clone(), request_id.clone()))
    }

    pub(crate) fn is_expired(&self, profile_id: &ProfileId, request_id: &RequestId) -> bool {
        self.expired
            .contains(&(profile_id.clone(), request_id.clone()))
    }

    pub(crate) fn next_id(&mut self) -> OperationId {
        let id = OperationId::parse(format!("op_{}", self.next_sequence))
            .expect("service-generated operation ID is valid");
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("operation identifier space exhausted");
        id
    }

    pub(crate) fn restore(&mut self, profile_id: ProfileId, operation: CompletedOperation) {
        if let Some(sequence) = operation
            .reply
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.id.as_str().strip_prefix("op_"))
            .and_then(|value| value.parse::<u64>().ok())
        {
            self.next_sequence = self.next_sequence.max(sequence.saturating_add(1));
        }
        let key = (profile_id, operation.request.request_id.clone());
        self.expired.remove(&key);
        self.completed.insert(key, operation);
    }

    pub(crate) fn restore_expired(&mut self, profile_id: ProfileId, request_id: RequestId) {
        self.expired.insert((profile_id, request_id));
    }

    pub(crate) fn insert(&mut self, profile_id: ProfileId, operation: CompletedOperation) {
        let key = (profile_id, operation.request.request_id.clone());
        self.expired.remove(&key);
        self.completed.insert(key, operation);
    }

    pub(crate) fn mark_evicted(&mut self, profile_id: &ProfileId, evicted: &[String]) {
        for request_id in evicted {
            let request_id = RequestId::parse(request_id.clone())
                .expect("persisted operation originated from a validated request ID");
            let key = (profile_id.clone(), request_id);
            self.completed.remove(&key);
            self.expired.insert(key);
        }
    }
}
