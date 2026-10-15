//! Page-initiated Fetch requests at the engine/network ownership boundary.

use anyhow::{Context as _, Result, ensure};
use boa_engine::Source;
use browser_net::{BrowserRequest, BrowserResponse};
use serde::{Deserialize, Serialize};

use crate::{DocumentRuntime, PageNetworkRecord};

const MAX_NETWORK_RECORDS: usize = 256;

/// An owned page request captured from one document generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFetchRequest {
    pub document_generation: u64,
    pub request_id: String,
    pub requested_url: String,
    pub url: String,
}

/// An owned, policy-relevant network request prepared by a document worker.
///
/// The service broker consumes this value; the document runtime never receives profile-scoped
/// network authority.
pub struct PageFetchDispatch {
    pub request: BrowserRequest,
    pub document_origin: String,
    pub cross_origin: bool,
}

#[derive(Debug, Deserialize)]
struct QueuedFetchRequest {
    generation: u64,
    request_id: String,
    url: String,
}

#[derive(Serialize)]
struct FetchResponsePayload<'a> {
    status: u16,
    url: &'a str,
    body: &'a str,
}

impl DocumentRuntime {
    /// Drain page requests queued by `fetch()`, resolving relative URLs against the committed page.
    pub fn take_page_fetch_requests(&mut self) -> Result<Vec<PageFetchRequest>> {
        // Drain the realm queue before validating its base URL. Blank documents normally have no
        // Fetch work, and must become quiescent instead of failing every background-service tick.
        // If a blank document did queue a request, draining first also makes that failure terminal
        // for the work item rather than reprocessing it indefinitely.
        let value = self
            .context
            .eval(Source::from_bytes(
                "JSON.stringify(globalThis.__rwbTakeFetchRequests())",
            ))
            .map_err(|error| anyhow::anyhow!("could not collect page Fetch requests: {error}"))?;
        let json = value
            .to_string(&mut self.context)
            .map_err(|error| anyhow::anyhow!("could not stringify page Fetch requests: {error}"))?
            .to_std_string_escaped();
        let queued: Vec<QueuedFetchRequest> =
            serde_json::from_str(&json).context("page Fetch queue produced invalid JSON")?;
        if queued.is_empty() {
            return Ok(Vec::new());
        }

        let document_url = self
            .document_url
            .as_deref()
            .context("page Fetch requires a committed network document URL")?;
        let base = BrowserRequest::parse(document_url)
            .with_context(|| format!("committed document URL is invalid: {document_url}"))?;
        let requests = queued
            .into_iter()
            .map(|request| {
                ensure!(
                    request.generation == self.document_generation,
                    "page Fetch request belongs to obsolete document generation {}",
                    request.generation
                );
                ensure!(
                    !request.request_id.is_empty(),
                    "page Fetch request ID must not be empty"
                );
                let url = base
                    .url
                    .join(&request.url)
                    .with_context(|| format!("page Fetch URL is invalid: {:?}", request.url))?;
                Ok(PageFetchRequest {
                    document_generation: request.generation,
                    request_id: request.request_id,
                    requested_url: request.url,
                    url: url.to_string(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        for request in &requests {
            self.push_network_record(request);
        }
        Ok(requests)
    }

    /// Prepare one queued request for service-owned broker dispatch.
    ///
    /// A stale request is discarded before any network effect. The returned value contains only
    /// owned request bytes and policy context, so it is safe to carry across a worker boundary.
    pub fn prepare_page_fetch(
        &self,
        request: &PageFetchRequest,
    ) -> Result<Option<PageFetchDispatch>> {
        if request.document_generation != self.document_generation {
            return Ok(None);
        }
        let document_url = self
            .document_url
            .as_deref()
            .context("page Fetch requires a committed network document URL")?;
        let document = BrowserRequest::parse(document_url)?;
        let mut target = BrowserRequest::parse(&request.url)?;
        let document_origin = document.url.origin().ascii_serialization();
        let cross_origin = document.url.origin() != target.url.origin();
        if cross_origin {
            target.origin = Some(document_origin.clone());
        }
        Ok(Some(PageFetchDispatch {
            request: target,
            document_origin,
            cross_origin,
        }))
    }

    /// Reject a brokered response while retaining its final URL and status in diagnostics.
    pub fn reject_page_fetch_response(
        &mut self,
        request: &PageFetchRequest,
        response: &BrowserResponse,
        message: &str,
    ) -> Result<bool> {
        if request.document_generation != self.document_generation {
            return Ok(false);
        }
        self.finish_network_record(
            request,
            Some(response.url.to_string()),
            Some(response.status),
            Some(message.to_owned()),
        );
        self.reject_page_fetch_callback(request, message)
    }

    /// Return the bounded network diagnostic records for this document generation.
    pub fn network_records(&self) -> &[PageNetworkRecord] {
        &self.network_records
    }

    fn push_network_record(&mut self, request: &PageFetchRequest) {
        let sequence = self
            .network_records
            .last()
            .map_or(1, |record| record.sequence.saturating_add(1));
        self.network_records.push(PageNetworkRecord {
            sequence,
            request_id: request.request_id.clone(),
            requested_url: request.requested_url.clone(),
            final_url: None,
            status: None,
            error: None,
        });
        if self.network_records.len() > MAX_NETWORK_RECORDS {
            self.network_records.remove(0);
        }
    }

    fn finish_network_record(
        &mut self,
        request: &PageFetchRequest,
        final_url: Option<String>,
        status: Option<u16>,
        error: Option<String>,
    ) {
        if let Some(record) = self
            .network_records
            .iter_mut()
            .rev()
            .find(|record| record.request_id == request.request_id)
        {
            record.final_url = final_url;
            record.status = status;
            record.error = error;
        }
    }

    /// Settle a queued request with an owned network response.
    pub fn complete_page_fetch(
        &mut self,
        request: &PageFetchRequest,
        response: &BrowserResponse,
    ) -> Result<bool> {
        if request.document_generation != self.document_generation {
            return Ok(false);
        }
        let body = String::from_utf8(response.body.clone())
            .context("page Fetch response body is not valid UTF-8")?;
        let payload = serde_json::to_string(&FetchResponsePayload {
            status: response.status,
            url: response.url.as_str(),
            body: &body,
        })
        .context("could not serialize page Fetch response")?;
        self.finish_network_record(
            request,
            Some(response.url.to_string()),
            Some(response.status),
            None,
        );
        self.complete_host_request(request.document_generation, &request.request_id, &payload)
    }

    /// Reject a queued request with a page-visible `TypeError`.
    pub fn reject_page_fetch(&mut self, request: &PageFetchRequest, message: &str) -> Result<bool> {
        if request.document_generation != self.document_generation {
            return Ok(false);
        }
        self.finish_network_record(request, None, None, Some(message.to_owned()));
        self.reject_page_fetch_callback(request, message)
    }

    fn reject_page_fetch_callback(
        &mut self,
        request: &PageFetchRequest,
        message: &str,
    ) -> Result<bool> {
        let request_id = serde_json::to_string(&request.request_id)
            .context("could not encode Fetch request ID")?;
        let message = serde_json::to_string(message).context("could not encode Fetch rejection")?;
        let source = format!("globalThis.__rwbRejectHostRequest({request_id}, {message})");
        let value = self
            .context
            .eval(Source::from_bytes(&source))
            .map_err(|error| anyhow::anyhow!("Fetch rejection callback failed: {error}"))?;
        let rejected = value.as_boolean().unwrap_or(false);
        self.checkpoint()?;
        Ok(rejected)
    }
}
