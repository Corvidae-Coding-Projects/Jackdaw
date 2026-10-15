use std::{fmt, path::Path};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const METADATA: TableDefinition<&str, u64> = TableDefinition::new("profile_metadata_v1");
const COOKIES: TableDefinition<&str, &[u8]> = TableDefinition::new("profile_cookies_v1");
const TAB_SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("profile_tab_sessions_v1");
const SCHEMA_KEY: &str = "cookie_schema_version";

pub const COOKIE_SCHEMA_VERSION: u64 = 1;

/// One persistent cookie detached from transport and database implementation handles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CookieRecord {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub expires_unix_seconds: u64,
    pub creation: u64,
}

/// Durable navigation state for one profile-owned tab.
///
/// The cursor identifies the committed entry within `history`; page heaps and form drafts are
/// intentionally not persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSessionRecord {
    pub tab_id: String,
    pub history: Vec<String>,
    pub cursor: usize,
    #[serde(default)]
    pub active: bool,
}

#[derive(Debug)]
pub enum StoreError {
    Database(redb::DatabaseError),
    Transaction(redb::TransactionError),
    Table(redb::TableError),
    Storage(redb::StorageError),
    Commit(redb::CommitError),
    Encode(serde_json::Error),
    UnsupportedSchema(u64),
    InvalidInput(&'static str),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "profile database failed: {error}"),
            Self::Transaction(error) => write!(formatter, "profile transaction failed: {error}"),
            Self::Table(error) => write!(formatter, "profile table failed: {error}"),
            Self::Storage(error) => write!(formatter, "profile storage failed: {error}"),
            Self::Commit(error) => write!(formatter, "profile commit failed: {error}"),
            Self::Encode(error) => write!(formatter, "profile cookie encoding failed: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported cookie schema version {version}")
            }
            Self::InvalidInput(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<redb::DatabaseError> for StoreError {
    fn from(error: redb::DatabaseError) -> Self {
        Self::Database(error)
    }
}
impl From<redb::TransactionError> for StoreError {
    fn from(error: redb::TransactionError) -> Self {
        Self::Transaction(error)
    }
}
impl From<redb::TableError> for StoreError {
    fn from(error: redb::TableError) -> Self {
        Self::Table(error)
    }
}
impl From<redb::StorageError> for StoreError {
    fn from(error: redb::StorageError) -> Self {
        Self::Storage(error)
    }
}
impl From<redb::CommitError> for StoreError {
    fn from(error: redb::CommitError) -> Self {
        Self::Commit(error)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error)
    }
}

/// An exclusive redb handle for one profile's durable browser-owned state.
pub struct ProfileStore {
    pub(crate) database: Database,
}

impl fmt::Debug for ProfileStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileStore")
            .finish_non_exhaustive()
    }
}

/// Compatibility name retained while callers migrate to the unified profile store.
pub type ProfileCookieStore = ProfileStore;

impl ProfileStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let database = Database::create(path)?;
        {
            let transaction = database.begin_write()?;
            {
                let mut metadata = transaction.open_table(METADATA)?;
                let stored_version = metadata.get(SCHEMA_KEY)?.map(|version| version.value());
                match stored_version {
                    Some(version) if version != COOKIE_SCHEMA_VERSION => {
                        return Err(StoreError::UnsupportedSchema(version));
                    }
                    Some(_) => {}
                    None => {
                        metadata.insert(SCHEMA_KEY, COOKIE_SCHEMA_VERSION)?;
                    }
                }
                transaction.open_table(COOKIES)?;
                transaction.open_table(TAB_SESSIONS)?;
            }
            transaction.commit()?;
        }
        crate::operations::initialize(&database)?;
        crate::storage::initialize(&database)?;
        Ok(Self { database })
    }

    pub fn replace_cookies(&self, cookies: &[CookieRecord]) -> Result<(), StoreError> {
        let transaction = self.database.begin_write()?;
        {
            let mut table = transaction.open_table(COOKIES)?;
            let keys = table
                .iter()?
                .map(|entry| entry.map(|(key, _)| key.value().to_owned()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key.as_str())?;
            }
            for cookie in cookies {
                let key = format!("{}\u{0}{}\u{0}{}", cookie.domain, cookie.path, cookie.name);
                let encoded = serde_json::to_vec(cookie)?;
                table.insert(key.as_str(), encoded.as_slice())?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_cookies(&self) -> Result<Vec<CookieRecord>, StoreError> {
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(COOKIES)?;
        let mut cookies = table
            .iter()?
            .map(|entry| {
                let (_, value) = entry?;
                serde_json::from_slice::<CookieRecord>(value.value()).map_err(StoreError::from)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        cookies.sort_by_key(|cookie| cookie.creation);
        Ok(cookies)
    }

    /// Atomically replace the durable tab snapshot for this profile.
    pub fn replace_tab_sessions(&self, tabs: &[TabSessionRecord]) -> Result<(), StoreError> {
        let mut active_tabs = 0_u8;
        for tab in tabs {
            active_tabs = active_tabs.saturating_add(u8::from(tab.active));
            if active_tabs > 1 {
                return Err(StoreError::InvalidInput(
                    "tab session snapshot has more than one active tab",
                ));
            }
            if tab.tab_id.is_empty() {
                return Err(StoreError::InvalidInput("tab session ID must not be empty"));
            }
            if tab.history.is_empty() {
                return Err(StoreError::InvalidInput(
                    "tab session history must not be empty",
                ));
            }
            if tab.cursor >= tab.history.len() {
                return Err(StoreError::InvalidInput(
                    "tab session cursor is outside history",
                ));
            }
        }

        let transaction = self.database.begin_write()?;
        {
            let mut table = transaction.open_table(TAB_SESSIONS)?;
            let keys = table
                .iter()?
                .map(|entry| entry.map(|(key, _)| key.value().to_owned()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key.as_str())?;
            }
            for tab in tabs {
                let encoded = serde_json::to_vec(tab)?;
                table.insert(tab.tab_id.as_str(), encoded.as_slice())?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Load the profile's tab snapshot in stable tab-ID order.
    pub fn load_tab_sessions(&self) -> Result<Vec<TabSessionRecord>, StoreError> {
        let transaction = self.database.begin_read()?;
        let table = transaction.open_table(TAB_SESSIONS)?;
        let mut tabs = table
            .iter()?
            .map(|entry| {
                let (_, value) = entry?;
                let tab = serde_json::from_slice::<TabSessionRecord>(value.value())?;
                if tab.tab_id.is_empty()
                    || tab.history.is_empty()
                    || tab.cursor >= tab.history.len()
                {
                    return Err(StoreError::InvalidInput("stored tab session is invalid"));
                }
                Ok(tab)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        tabs.sort_by(|left, right| left.tab_id.cmp(&right.tab_id));
        if tabs.iter().filter(|tab| tab.active).count() > 1 {
            return Err(StoreError::InvalidInput(
                "stored tab sessions have more than one active tab",
            ));
        }
        Ok(tabs)
    }
}
