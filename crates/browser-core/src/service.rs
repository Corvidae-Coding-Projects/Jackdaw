use std::{collections::BTreeMap, path::PathBuf};

use browser_net::BrowserNetworkConfig;
use browser_protocol::{DownloadId, ProfileId, TabId};

use crate::{
    broker::BrowserBroker, cancellation::NavigationCancellation, events::EventLog,
    observations::ViewRetentionPolicy, operations::OperationRegistry, tabs::TabState,
    transfers::DownloadState, workers::DocumentWorkers,
};

/// Sequential owner of live tabs for the first persistent daemon slice.
pub struct BrowserService {
    pub(crate) tabs: BTreeMap<TabId, TabState>,
    pub(crate) broker: BrowserBroker,
    pub(crate) active_tabs: BTreeMap<ProfileId, TabId>,
    pub(crate) known_profiles: std::collections::BTreeSet<ProfileId>,
    pub(crate) profile_root: Option<PathBuf>,
    pub(crate) network_config: BrowserNetworkConfig,
    pub(crate) downloads: BTreeMap<DownloadId, DownloadState>,
    pub(crate) operations: OperationRegistry,
    pub(crate) events: EventLog,
    pub(crate) view_retention: ViewRetentionPolicy,
    pub(crate) navigation_cancellation: NavigationCancellation,
    pub(crate) workers: DocumentWorkers,
    pub(crate) next_tab: u64,
    pub(crate) next_view: u64,
    pub(crate) next_download: u64,
}

impl Default for BrowserService {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserService {
    pub fn navigation_cancellation(&self) -> NavigationCancellation {
        self.navigation_cancellation.clone()
    }

    pub fn new() -> Self {
        Self::with_optional_profile_root(
            None,
            ViewRetentionPolicy::default(),
            BrowserNetworkConfig::default(),
        )
    }

    /// Create a service whose persistent browser state is isolated below `profile_root`.
    pub fn with_profile_root(profile_root: impl Into<PathBuf>) -> Self {
        Self::with_profile_root_and_view_retention(profile_root, ViewRetentionPolicy::default())
    }

    pub fn with_profile_root_and_config(
        profile_root: impl Into<PathBuf>,
        network_config: BrowserNetworkConfig,
    ) -> Self {
        Self::with_profile_root_view_retention_and_config(
            profile_root,
            ViewRetentionPolicy::default(),
            network_config,
        )
    }

    pub fn with_profile_root_and_view_retention(
        profile_root: impl Into<PathBuf>,
        view_retention: ViewRetentionPolicy,
    ) -> Self {
        Self::with_profile_root_view_retention_and_config(
            profile_root,
            view_retention,
            BrowserNetworkConfig::default(),
        )
    }

    pub fn with_profile_root_view_retention_and_config(
        profile_root: impl Into<PathBuf>,
        view_retention: ViewRetentionPolicy,
        network_config: BrowserNetworkConfig,
    ) -> Self {
        Self::with_optional_profile_root(Some(profile_root.into()), view_retention, network_config)
    }

    fn with_optional_profile_root(
        profile_root: Option<PathBuf>,
        view_retention: ViewRetentionPolicy,
        network_config: BrowserNetworkConfig,
    ) -> Self {
        Self {
            tabs: BTreeMap::new(),
            broker: BrowserBroker::new(),
            active_tabs: BTreeMap::new(),
            known_profiles: std::collections::BTreeSet::new(),
            profile_root,
            network_config,
            downloads: BTreeMap::new(),
            operations: OperationRegistry::new(),
            events: EventLog::new(),
            view_retention,
            navigation_cancellation: NavigationCancellation::default(),
            workers: DocumentWorkers::new(),
            next_tab: 1,
            next_view: 1,
            next_download: 1,
        }
    }
}
