//! Minimal versioned persistence boundary used by the component qualification gate.
//!
//! The production profile store will move to `browser-store`; this module exists to prove that
//! the selected `redb` version can durably round-trip a versioned record before that crate is
//! introduced.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use redb::{Database, ReadableDatabase, TableDefinition};

const COMPONENT_RECORDS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("component_records_v1");

/// Schema version written by the component gate.
pub const COMPONENT_SCHEMA_VERSION: u32 = 1;

/// A small owned record representing the persistence contract exercised by the spike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRecord {
    pub schema_version: u32,
    pub value: Vec<u8>,
}

/// Write a record, commit it, close the database, reopen it, and return the durable value.
///
/// Reopening is intentional: a read from the original handle would not demonstrate that the
/// committed bytes survive the lifecycle boundary required by persistent browser profiles.
pub fn round_trip_component_record(
    path: &Path,
    key: &str,
    value: &[u8],
) -> Result<ComponentRecord> {
    ensure!(!key.is_empty(), "component record key must not be empty");

    {
        let database = Database::create(path).with_context(|| {
            format!("failed to create component database at {}", path.display())
        })?;
        let transaction = database
            .begin_write()
            .context("failed to begin component database write")?;
        {
            let mut table = transaction
                .open_table(COMPONENT_RECORDS)
                .context("failed to open component record table")?;
            table
                .insert(key, value)
                .context("failed to write component record")?;
        }
        transaction
            .commit()
            .context("failed to commit component record")?;
    }

    let database = Database::open(path)
        .with_context(|| format!("failed to reopen component database at {}", path.display()))?;
    let transaction = database
        .begin_read()
        .context("failed to begin component database read")?;
    let table = transaction
        .open_table(COMPONENT_RECORDS)
        .context("failed to reopen component record table")?;
    let stored = table
        .get(key)
        .context("failed to read component record")?
        .with_context(|| format!("component record {key:?} was not persisted"))?;

    Ok(ComponentRecord {
        schema_version: COMPONENT_SCHEMA_VERSION,
        value: stored.value().to_vec(),
    })
}
