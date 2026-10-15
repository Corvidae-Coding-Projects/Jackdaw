use std::collections::BTreeMap;

use redb::{Database, ReadableDatabase, TableDefinition};

use crate::{ProfileStore, StoreError};

const LOCAL_STORAGE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("profile_local_storage_v1");

pub(crate) fn initialize(database: &Database) -> Result<(), StoreError> {
    let transaction = database.begin_write()?;
    {
        transaction.open_table(LOCAL_STORAGE)?;
    }
    transaction.commit()?;
    Ok(())
}

impl ProfileStore {
    /// Atomically replace the complete local-storage map for one serialized origin.
    pub fn replace_local_storage(
        &self,
        origin: &str,
        entries: &BTreeMap<String, String>,
    ) -> Result<(), StoreError> {
        if origin.is_empty() {
            return Err(StoreError::InvalidInput(
                "local-storage origin must not be empty",
            ));
        }
        let encoded = serde_json::to_vec(entries)?;
        let transaction = self.database.begin_write()?;
        {
            let mut table = transaction.open_table(LOCAL_STORAGE)?;
            table.insert(origin, encoded.as_slice())?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Load an origin's local-storage map, returning an empty map for a new origin.
    pub fn load_local_storage(&self, origin: &str) -> Result<BTreeMap<String, String>, StoreError> {
        if origin.is_empty() {
            return Err(StoreError::InvalidInput(
                "local-storage origin must not be empty",
            ));
        }
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(LOCAL_STORAGE)?;
        let Some(encoded) = table.get(origin)? else {
            return Ok(BTreeMap::new());
        };
        Ok(serde_json::from_slice(encoded.value())?)
    }
}
