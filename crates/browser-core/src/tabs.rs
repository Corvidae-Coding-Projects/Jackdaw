use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use browser_engine::ElementTarget;
use browser_protocol::{DocumentId, ElementRef, ProfileId, TabId, TabSummary, ViewId};

use crate::{
    observations::{ViewRetentionPolicy, ViewSnapshot},
    workers::{DocumentMetadata, DocumentWorker, RenderedDocument, WorkerCommand},
};

const MAX_RETAINED_EXPIRED_VIEWS_PER_TAB: usize = 16;
const BLANK_DOCUMENT: &str = "<!doctype html><html><body><main id='content'></main></body></html>";

pub(crate) struct TabState {
    pub(crate) id: TabId,
    pub(crate) profile_id: ProfileId,
    pub(crate) document_worker: DocumentWorker,
    document_owner_epoch: u64,
    pub(crate) observed_targets: BTreeMap<ElementRef, ElementTarget>,
    pub(crate) observed_link_urls: BTreeMap<ElementRef, String>,
    pub(crate) observed_views: BTreeMap<ViewId, ViewSnapshot>,
    pub(crate) expired_views: BTreeSet<ViewId>,
    pub(crate) key_input_view: Option<(ViewId, DocumentId)>,
    pub(crate) pointer_press: Option<ElementTarget>,
}

impl TabState {
    pub(crate) fn new(id: TabId, profile_id: ProfileId) -> Result<Self, String> {
        Ok(Self {
            id,
            profile_id,
            document_worker: DocumentWorker::from_html(BLANK_DOCUMENT)?,
            document_owner_epoch: 1,
            observed_targets: BTreeMap::new(),
            observed_link_urls: BTreeMap::new(),
            observed_views: BTreeMap::new(),
            expired_views: BTreeSet::new(),
            key_input_view: None,
            pointer_press: None,
        })
    }

    pub(crate) fn metadata(&self) -> DocumentMetadata {
        self.document_worker.metadata()
    }

    pub(crate) fn render_document_at(
        &mut self,
        width: u16,
        height: u16,
    ) -> Result<RenderedDocument, String> {
        self.document_worker
            .execute(WorkerCommand::RenderDocumentAt { width, height })
    }

    pub(crate) fn document_id(&self) -> DocumentId {
        let metadata = self.metadata();
        DocumentId::parse(format!(
            "doc_{}_{}_{}",
            self.id.as_str().trim_start_matches("t_"),
            self.document_owner_epoch,
            metadata.generation
        ))
        .expect("service-generated document ID is valid")
    }

    pub(crate) fn summary(&self) -> TabSummary {
        let metadata = self.metadata();
        TabSummary {
            tab_id: self.id.clone(),
            url: metadata.url,
            title: metadata.title,
            loading: false,
        }
    }

    pub(crate) fn replace_failed_document_owner(&mut self) -> Result<(), String> {
        let replacement = DocumentWorker::from_html(BLANK_DOCUMENT)?;
        self.document_worker = replacement;
        self.document_owner_epoch = self
            .document_owner_epoch
            .checked_add(1)
            .ok_or_else(|| "document owner epoch space exhausted".to_owned())?;
        self.invalidate_views();
        Ok(())
    }

    pub(crate) fn invalidate_views(&mut self) {
        self.observed_targets.clear();
        self.observed_link_urls.clear();
        self.observed_views.clear();
        self.expired_views.clear();
        self.key_input_view = None;
        self.pointer_press = None;
    }

    fn expire_view(&mut self, view_id: ViewId) {
        self.observed_views.remove(&view_id);
        self.expired_views.insert(view_id.clone());
        while self.expired_views.len() > MAX_RETAINED_EXPIRED_VIEWS_PER_TAB {
            self.expired_views.pop_first();
        }
        self.observed_targets
            .retain(|target, _| target.view_id() != view_id);
        self.observed_link_urls
            .retain(|target, _| target.view_id() != view_id);
    }

    pub(crate) fn prune_views(&mut self, policy: ViewRetentionPolicy, now: Instant) {
        let aged = self
            .observed_views
            .iter()
            .filter(|(_, snapshot)| {
                now.saturating_duration_since(snapshot.retained_at) >= policy.max_age
            })
            .map(|(view_id, _)| view_id.clone())
            .collect::<Vec<_>>();
        for view_id in aged {
            self.expire_view(view_id);
        }

        loop {
            let retained_bytes = self
                .observed_views
                .values()
                .map(|snapshot| snapshot.retained_bytes)
                .sum::<usize>();
            if self.observed_views.len() <= policy.max_count && retained_bytes <= policy.max_bytes {
                break;
            }
            let oldest = self
                .observed_views
                .first_key_value()
                .map(|(view_id, _)| view_id.clone())
                .expect("an over-limit retained-view map is nonempty");
            self.expire_view(oldest);
        }
    }
}
