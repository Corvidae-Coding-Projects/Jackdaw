use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{ProfileStore, StoreError};

const OPERATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("profile_operations_v1");
const OPERATION_TOMBSTONES: TableDefinition<&str, u64> =
    TableDefinition::new("profile_operation_tombstones_v1");

/// Durable dispatch boundary for one protocol operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Accepted,
    Dispatched,
    Terminal,
}

fn terminal_phase() -> OperationPhase {
    OperationPhase::Terminal
}

/// One protocol operation retained inside its owning profile database.
///
/// The store deliberately treats request and reply JSON as opaque protocol payloads so the
/// persistence crate does not depend on browser-protocol or interpret frontend semantics. Older
/// records deserialize as terminal records because they were written only after completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    /// Monotonic service-assigned operation sequence used for deterministic retention.
    #[serde(default)]
    pub sequence: u64,
    pub request_id: String,
    pub request_json: String,
    #[serde(default = "terminal_phase")]
    pub phase: OperationPhase,
    #[serde(default)]
    pub reply_json: Option<String>,
}

pub(crate) fn initialize(database: &Database) -> Result<(), StoreError> {
    let transaction = database.begin_write()?;
    {
        transaction.open_table(OPERATIONS)?;
        transaction.open_table(OPERATION_TOMBSTONES)?;
    }
    transaction.commit()?;
    Ok(())
}

impl ProfileStore {
    /// Atomically retain one completed operation and tombstone the oldest records by sequence.
    ///
    /// The returned request IDs were replaced by durable tombstones in the committed transaction
    /// and may therefore move from an in-memory replay index to an expiry index without exposing a
    /// split-brain retention state. Tombstones use the same bound as full retained replies.
    pub fn put_operation(
        &self,
        operation: &OperationRecord,
        max_retained: usize,
    ) -> Result<Vec<String>, StoreError> {
        let reply_is_valid = match operation.phase {
            OperationPhase::Terminal => operation
                .reply_json
                .as_ref()
                .is_some_and(|reply| !reply.is_empty()),
            OperationPhase::Accepted | OperationPhase::Dispatched => operation.reply_json.is_none(),
        };
        if operation.sequence == 0
            || operation.request_id.is_empty()
            || operation.request_json.is_empty()
            || !reply_is_valid
            || max_retained == 0
        {
            return Err(StoreError::InvalidInput(
                "operation record fields, phase, and retention limit are inconsistent",
            ));
        }
        let encoded = serde_json::to_vec(operation)?;
        let transaction = self.database.begin_write()?;
        let evicted = {
            let mut table = transaction.open_table(OPERATIONS)?;
            table.insert(operation.request_id.as_str(), encoded.as_slice())?;

            let mut retained = table
                .iter()?
                .map(|entry| {
                    let (key, value) = entry?;
                    let stored = serde_json::from_slice::<OperationRecord>(value.value())?;
                    Ok((stored.sequence, key.value().to_owned()))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            retained.sort();
            let evicted_ids = retained
                .into_iter()
                .take(table.len()?.saturating_sub(max_retained as u64) as usize)
                .map(|(_, request_id)| request_id)
                .collect::<Vec<_>>();
            let mut evicted = Vec::with_capacity(evicted_ids.len());
            for request_id in evicted_ids {
                let removed =
                    table
                        .remove(request_id.as_str())?
                        .ok_or(StoreError::InvalidInput(
                            "operation selected for eviction disappeared",
                        ))?;
                evicted.push(serde_json::from_slice::<OperationRecord>(removed.value())?);
            }
            evicted
        };
        {
            let mut tombstones = transaction.open_table(OPERATION_TOMBSTONES)?;
            tombstones.remove(operation.request_id.as_str())?;
            for record in &evicted {
                tombstones.insert(record.request_id.as_str(), record.sequence)?;
            }
            let mut retained = tombstones
                .iter()?
                .map(|entry| {
                    let (key, value) = entry?;
                    Ok((value.value(), key.value().to_owned()))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            retained.sort();
            let expired = retained
                .into_iter()
                .take(tombstones.len()?.saturating_sub(max_retained as u64) as usize)
                .map(|(_, request_id)| request_id)
                .collect::<Vec<_>>();
            for request_id in expired {
                tombstones.remove(request_id.as_str())?;
            }
        }
        transaction.commit()?;
        Ok(evicted
            .into_iter()
            .map(|record| record.request_id)
            .collect())
    }

    /// Load completed operations in stable durable-sequence order.
    pub fn load_operations(&self) -> Result<Vec<OperationRecord>, StoreError> {
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(OPERATIONS)?;
        let mut operations = table
            .iter()?
            .map(|entry| {
                let (key, value) = entry?;
                let operation = serde_json::from_slice::<OperationRecord>(value.value())?;
                let reply_is_valid = match operation.phase {
                    OperationPhase::Terminal => operation
                        .reply_json
                        .as_ref()
                        .is_some_and(|reply| !reply.is_empty()),
                    OperationPhase::Accepted | OperationPhase::Dispatched => {
                        operation.reply_json.is_none()
                    }
                };
                if operation.request_id.is_empty()
                    || operation.request_id != key.value()
                    || operation.request_json.is_empty()
                    || !reply_is_valid
                {
                    return Err(StoreError::InvalidInput(
                        "stored operation record is invalid",
                    ));
                }
                Ok(operation)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        operations.sort_by(|left, right| {
            left.sequence
                .cmp(&right.sequence)
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        Ok(operations)
    }

    /// Load bounded durable markers for operation replies that aged out of replay retention.
    pub fn load_operation_tombstones(&self) -> Result<Vec<String>, StoreError> {
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(OPERATION_TOMBSTONES)?;
        let mut tombstones = table
            .iter()?
            .map(|entry| {
                let (key, value) = entry?;
                if key.value().is_empty() || value.value() == 0 {
                    return Err(StoreError::InvalidInput(
                        "stored operation tombstone is invalid",
                    ));
                }
                Ok((value.value(), key.value().to_owned()))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        tombstones.sort();
        Ok(tombstones
            .into_iter()
            .map(|(_, request_id)| request_id)
            .collect())
    }
}
