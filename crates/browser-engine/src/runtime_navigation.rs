//! HTTP document navigation and tab-local history at the engine/network ownership boundary.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use browser_net::{BrowserRequest, BrowserResponse};

use crate::{DocumentRuntime, FormSubmission};

/// Narrow host capabilities required while committing a broker-selected document.
///
/// Implementations own network, cookie, and durable-storage authority. The document runtime sees
/// only these origin-scoped operations and cannot select a profile, open arbitrary files, or reach
/// daemon IPC.
pub trait DocumentHost {
    fn fetch_subresource(&self, requested_url: &str) -> Result<BrowserResponse>;

    fn load_local_storage(&self, origin: &str) -> Result<BTreeMap<String, String>>;

    fn replace_local_storage(&self, origin: &str, entries: &BTreeMap<String, String>)
    -> Result<()>;

    fn document_cookie(&self, document_url: &str) -> Result<String>;

    fn store_document_cookie(&self, document_url: &str, assignment: &str) -> Result<()>;
}

/// Owned evidence describing one committed network navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationCommit {
    pub requested_url: String,
    pub final_url: String,
    pub status: u16,
    pub redirect_count: usize,
}

/// An owned form-navigation request prepared by the document worker for broker dispatch.
pub struct FormNavigationRequest {
    pub requested_url: String,
    pub request: BrowserRequest,
}

/// The committed document URLs for one runtime, plus its active entry.
///
/// Entries are changed only after the replacement document has committed. Failed fetches leave
/// both the active document and the traversal position unchanged.
#[derive(Debug, Default)]
pub(crate) struct NavigationHistory {
    entries: Vec<String>,
    current: Option<usize>,
}

impl NavigationHistory {
    fn record_navigation(&mut self, url: String) {
        if let Some(current) = self.current {
            self.entries.truncate(current + 1);
        } else {
            self.entries.clear();
        }
        self.entries.push(url);
        self.current = Some(self.entries.len() - 1);
    }

    fn back_target(&self) -> Option<(usize, String)> {
        let index = self.current?.checked_sub(1)?;
        Some((index, self.entries[index].clone()))
    }

    fn forward_target(&self) -> Option<(usize, String)> {
        let index = self.current?.checked_add(1)?;
        self.entries.get(index).cloned().map(|url| (index, url))
    }

    fn reload_target(&self) -> Option<(usize, String)> {
        let index = self.current?;
        Some((index, self.entries[index].clone()))
    }

    fn commit_traversal(&mut self, index: usize, url: String) {
        self.entries[index] = url;
        self.current = Some(index);
    }
}

impl DocumentRuntime {
    /// Atomically commit a top-level response already selected by the service broker.
    ///
    /// Fetching and redirect selection happen before this method is called, so any broker failure
    /// leaves the active document and history cursor unchanged.
    pub fn commit_brokered_navigation(
        &mut self,
        host: &dyn DocumentHost,
        requested_url: &str,
        response: BrowserResponse,
    ) -> Result<NavigationCommit> {
        let commit = self.commit_http_response(host, requested_url.to_owned(), response)?;
        self.navigation_history
            .record_navigation(commit.final_url.clone());
        Ok(commit)
    }

    /// Return the preceding committed URL without performing network I/O.
    pub fn back_target_url(&self) -> Option<String> {
        self.navigation_history.back_target().map(|(_, url)| url)
    }

    /// Return the following committed URL without performing network I/O.
    pub fn forward_target_url(&self) -> Option<String> {
        self.navigation_history.forward_target().map(|(_, url)| url)
    }

    /// Return the active committed URL without performing network I/O.
    pub fn reload_target_url(&self) -> Option<String> {
        self.navigation_history.reload_target().map(|(_, url)| url)
    }

    /// Commit a broker-selected response at the preceding history position.
    pub fn commit_brokered_back(
        &mut self,
        host: &dyn DocumentHost,
        requested_url: &str,
        response: BrowserResponse,
    ) -> Result<Option<NavigationCommit>> {
        let Some((index, expected_url)) = self.navigation_history.back_target() else {
            return Ok(None);
        };
        ensure!(
            expected_url == requested_url,
            "back navigation target changed before commit"
        );
        let commit = self.commit_http_response(host, requested_url.to_owned(), response)?;
        self.navigation_history
            .commit_traversal(index, commit.final_url.clone());
        Ok(Some(commit))
    }

    /// Commit a broker-selected response at the following history position.
    pub fn commit_brokered_forward(
        &mut self,
        host: &dyn DocumentHost,
        requested_url: &str,
        response: BrowserResponse,
    ) -> Result<Option<NavigationCommit>> {
        let Some((index, expected_url)) = self.navigation_history.forward_target() else {
            return Ok(None);
        };
        ensure!(
            expected_url == requested_url,
            "forward navigation target changed before commit"
        );
        let commit = self.commit_http_response(host, requested_url.to_owned(), response)?;
        self.navigation_history
            .commit_traversal(index, commit.final_url.clone());
        Ok(Some(commit))
    }

    /// Commit a broker-selected response in place at the active history position.
    pub fn commit_brokered_reload(
        &mut self,
        host: &dyn DocumentHost,
        requested_url: &str,
        response: BrowserResponse,
    ) -> Result<Option<NavigationCommit>> {
        let Some((index, expected_url)) = self.navigation_history.reload_target() else {
            return Ok(None);
        };
        ensure!(
            expected_url == requested_url,
            "reload navigation target changed before commit"
        );
        let commit = self.commit_http_response(host, requested_url.to_owned(), response)?;
        self.navigation_history
            .commit_traversal(index, commit.final_url.clone());
        Ok(Some(commit))
    }

    /// Prepare an already validated form submission for service-owned broker dispatch.
    ///
    /// The document runtime owns form semantics and encoding, but it does not perform the network
    /// effect. The returned request contains only owned bytes and may cross a worker boundary.
    pub fn prepare_form_submission(
        &self,
        submission: FormSubmission,
    ) -> Result<FormNavigationRequest> {
        let base = self
            .document_url
            .as_deref()
            .context("form submission requires a committed document URL")?;
        let action = if submission.action.is_empty() {
            base.to_owned()
        } else {
            BrowserRequest::parse(base)?
                .url
                .join(&submission.action)?
                .to_string()
        };
        let request = if submission.enctype == "multipart/form-data" {
            BrowserRequest::multipart(
                &action,
                &submission.method,
                submission
                    .fields
                    .into_iter()
                    .map(|field| (field.name, field.value))
                    .collect(),
                submission
                    .files
                    .into_iter()
                    .map(|file| (file.name, file.file_name, file.media_type, file.bytes))
                    .collect(),
            )?
        } else {
            BrowserRequest::form(
                &action,
                &submission.method,
                submission
                    .fields
                    .iter()
                    .map(|field| (field.name.as_str(), field.value.as_str())),
            )?
        };
        Ok(FormNavigationRequest {
            requested_url: action,
            request,
        })
    }

    /// Commit a form response selected by the service-owned broker.
    pub fn commit_brokered_form_submission(
        &mut self,
        host: &dyn DocumentHost,
        requested_url: String,
        response: BrowserResponse,
    ) -> Result<NavigationCommit> {
        let commit = self.commit_http_response(host, requested_url, response)?;
        self.navigation_history
            .record_navigation(commit.final_url.clone());
        Ok(commit)
    }

    /// Return the committed tab history and active cursor as owned persistence data.
    pub fn navigation_snapshot(&self) -> Option<(Vec<String>, usize)> {
        self.navigation_history
            .current
            .map(|cursor| (self.navigation_history.entries.clone(), cursor))
    }

    /// Reconstruct a committed tab from a response selected by the service broker.
    ///
    /// Fetching happens before this method is called, so a broker failure leaves the caller free
    /// to discard the temporary runtime without granting it network authority.
    pub fn restore_brokered_history(
        &mut self,
        host: &dyn DocumentHost,
        history: Vec<String>,
        cursor: usize,
        response: BrowserResponse,
    ) -> Result<NavigationCommit> {
        ensure!(
            !history.is_empty(),
            "restored tab history must not be empty"
        );
        ensure!(
            cursor < history.len(),
            "restored tab history cursor is outside its entries"
        );
        let requested_url = history[cursor].clone();
        let commit = self.commit_http_response(host, requested_url, response)?;
        let mut restored = history;
        restored[cursor] = commit.final_url.clone();
        self.navigation_history = NavigationHistory {
            entries: restored,
            current: Some(cursor),
        };
        Ok(commit)
    }

    fn commit_http_response(
        &mut self,
        host: &dyn DocumentHost,
        requested_url: String,
        response: BrowserResponse,
    ) -> Result<NavigationCommit> {
        ensure!(
            (200..=299).contains(&response.status),
            "document navigation returned HTTP status {} for {}",
            response.status,
            response.url
        );
        let html = String::from_utf8(response.body)
            .context("document response body is not valid UTF-8")?;
        let commit = NavigationCommit {
            requested_url,
            final_url: response.url.to_string(),
            status: response.status,
            redirect_count: response.redirects.len(),
        };
        self.navigate_network(&html, &commit.final_url, host)?;
        Ok(commit)
    }
}
