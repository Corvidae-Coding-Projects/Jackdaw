use std::collections::BTreeMap;

use browser_engine::{DocumentHost, FormNavigationRequest, PageFetchDispatch};

use crate::workers::DocumentWorker;
use browser_net::{BrowserClient, BrowserResponse, RequestControl};
use browser_protocol::ProfileId;

pub(crate) enum BrokerPageFetchOutcome {
    Response(BrowserResponse),
    Rejected {
        response: Option<BrowserResponse>,
        message: String,
    },
}

/// Borrowed, origin-scoped document authority issued by the service broker.
///
/// This adapter deliberately exposes only the narrow `DocumentHost` contract. A live document
/// cannot retain or recover the concrete profile client, select another profile, or reach daemon
/// and filesystem authority through this value.
pub(crate) struct BrokerDocumentHost<'a> {
    client: &'a BrowserClient,
}

impl DocumentHost for BrokerDocumentHost<'_> {
    fn fetch_subresource(&self, requested_url: &str) -> anyhow::Result<BrowserResponse> {
        Ok(self.client.fetch_url(requested_url)?)
    }

    fn load_local_storage(&self, origin: &str) -> anyhow::Result<BTreeMap<String, String>> {
        Ok(self.client.load_local_storage(origin)?)
    }

    fn replace_local_storage(
        &self,
        origin: &str,
        entries: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        Ok(self.client.replace_local_storage(origin, entries)?)
    }

    fn document_cookie(&self, document_url: &str) -> anyhow::Result<String> {
        Ok(self.client.document_cookie(document_url)?)
    }

    fn store_document_cookie(&self, document_url: &str, assignment: &str) -> anyhow::Result<()> {
        Ok(self
            .client
            .store_document_cookie(document_url, assignment)?)
    }
}

/// Service-owned authority for profile-scoped network, storage, and operation effects.
///
/// Live document state receives only a narrow, borrowed host capability while committing brokered
/// response data; tabs never retain a concrete profile client. Keeping the client registry private
/// to this boundary also makes later worker IPC mediation possible without changing document
/// ownership again.
pub(crate) struct BrowserBroker {
    clients: BTreeMap<ProfileId, BrowserClient>,
}

impl BrowserBroker {
    pub(crate) fn new() -> Self {
        Self {
            clients: BTreeMap::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.clients.len()
    }

    pub(crate) fn contains_profile(&self, profile_id: &ProfileId) -> bool {
        self.clients.contains_key(profile_id)
    }

    pub(crate) fn profiles(&self) -> impl Iterator<Item = &ProfileId> {
        self.clients.keys()
    }

    pub(crate) fn insert(
        &mut self,
        profile_id: ProfileId,
        client: BrowserClient,
    ) -> Option<BrowserClient> {
        self.clients.insert(profile_id, client)
    }

    pub(crate) fn remove(&mut self, profile_id: &ProfileId) -> Option<BrowserClient> {
        self.clients.remove(profile_id)
    }

    pub(crate) fn client(&self, profile_id: &ProfileId) -> Option<&BrowserClient> {
        self.clients.get(profile_id)
    }

    /// Borrow the origin-scoped capabilities needed while committing a selected document.
    pub(crate) fn document_host(
        &self,
        profile_id: &ProfileId,
    ) -> Result<BrokerDocumentHost<'_>, String> {
        self.client(profile_id)
            .map(|client| BrokerDocumentHost { client })
            .ok_or_else(|| format!("profile {profile_id} is not open"))
    }

    /// Fetch and select a top-level navigation response under service-owned profile authority.
    ///
    /// The document owner receives only completed response data. It never selects the profile,
    /// redirect chain, or concrete network client used for dispatch.
    pub(crate) fn dispatch_navigation(
        &self,
        profile_id: &ProfileId,
        requested_url: &str,
        control: &RequestControl,
    ) -> Result<BrowserResponse, String> {
        self.client(profile_id)
            .ok_or_else(|| format!("profile {profile_id} is not open"))?
            .fetch_url_with_control(requested_url, control)
            .map_err(|error| error.to_string())
    }

    /// Fetch the initial document for a newly created tab under service-owned profile authority.
    ///
    /// Tab creation has no independently cancellable operation yet, so this uses the network
    /// client's configured deadline while preserving the same broker/commit ownership split as
    /// explicit navigation commands.
    pub(crate) fn dispatch_initial_navigation(
        &self,
        profile_id: &ProfileId,
        requested_url: &str,
    ) -> Result<BrowserResponse, String> {
        self.client(profile_id)
            .ok_or_else(|| format!("profile {profile_id} is not open"))?
            .fetch_url(requested_url)
            .map_err(|error| error.to_string())
    }

    /// Restore one persisted tab before its profile client enters the live registry.
    ///
    /// Profile opening is transactional: the candidate client is not published until every stored
    /// tab has been reconstructed. Keeping response selection here preserves that transaction while
    /// ensuring the document runtime receives only completed response data.
    pub(crate) fn restore_profile_tab(
        &self,
        client: &BrowserClient,
        runtime: &mut DocumentWorker,
        history: Vec<String>,
        cursor: usize,
    ) -> Result<(), String> {
        let requested_url = history
            .get(cursor)
            .ok_or_else(|| "stored tab history cursor is outside its entries".to_owned())?
            .clone();
        let response = client
            .fetch_url(&requested_url)
            .map_err(|error| error.to_string())?;
        let host = BrokerDocumentHost { client };
        runtime.restore_history(&host, history, cursor, response)
    }

    /// Fetch a download under service-owned profile authority.
    ///
    /// Transfer handling receives only the completed response bytes and metadata; it never holds
    /// or selects a profile-scoped network client.
    pub(crate) fn dispatch_download(
        &self,
        profile_id: &ProfileId,
        requested_url: &str,
    ) -> Result<BrowserResponse, String> {
        self.client(profile_id)
            .ok_or_else(|| format!("profile {profile_id} is not open"))?
            .fetch_url(requested_url)
            .map_err(|error| error.to_string())
    }

    /// Dispatch a prepared form request under service-owned profile authority.
    ///
    /// The document owner supplies only serialized form semantics. The broker selects the
    /// profile client, performs the network effect, and returns owned response data for commit.
    pub(crate) fn dispatch_form_submission(
        &self,
        profile_id: &ProfileId,
        prepared: FormNavigationRequest,
    ) -> Result<(String, BrowserResponse), String> {
        let response = self
            .client(profile_id)
            .ok_or_else(|| format!("profile {profile_id} is not open"))?
            .fetch(prepared.request)
            .map_err(|error| error.to_string())?;
        Ok((prepared.requested_url, response))
    }

    /// Dispatch page Fetch through service-owned profile authority and enforce CORS before the
    /// response can re-enter a document worker.
    pub(crate) fn dispatch_page_fetch(
        &self,
        profile_id: &ProfileId,
        dispatch: PageFetchDispatch,
    ) -> BrokerPageFetchOutcome {
        let Some(client) = self.client(profile_id) else {
            return BrokerPageFetchOutcome::Rejected {
                response: None,
                message: format!("profile {profile_id} is not open"),
            };
        };
        match client.fetch(dispatch.request) {
            Ok(response) if dispatch.cross_origin => {
                let allowed = response
                    .headers
                    .get("access-control-allow-origin")
                    .is_some_and(|value| value == "*" || value == &dispatch.document_origin);
                if allowed {
                    BrokerPageFetchOutcome::Response(response)
                } else {
                    BrokerPageFetchOutcome::Rejected {
                        message: format!(
                            "cross-origin Fetch denied: Access-Control-Allow-Origin does not allow {}",
                            dispatch.document_origin
                        ),
                        response: Some(response),
                    }
                }
            }
            Ok(response) => BrokerPageFetchOutcome::Response(response),
            Err(error) => BrokerPageFetchOutcome::Rejected {
                response: None,
                message: error.to_string(),
            },
        }
    }
}
