use std::time::{Duration, Instant};

use browser_protocol::{
    DocumentId, DocumentReadyState, FrameId, Observation, ObservationCursor, ObservedItem,
    ProtocolError, ViewId,
};

use crate::errors::{internal_error, invalid_request};

const MAX_RETAINED_VIEWS_PER_TAB: usize = 16;
const MAX_RETAINED_VIEW_BYTES_PER_TAB: usize = 1024 * 1024;
const MAX_RETAINED_VIEW_AGE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRetentionPolicy {
    pub max_count: usize,
    pub max_bytes: usize,
    pub max_age: Duration,
}

impl Default for ViewRetentionPolicy {
    fn default() -> Self {
        Self {
            max_count: MAX_RETAINED_VIEWS_PER_TAB,
            max_bytes: MAX_RETAINED_VIEW_BYTES_PER_TAB,
            max_age: MAX_RETAINED_VIEW_AGE,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ViewSnapshot {
    pub(crate) view_id: ViewId,
    pub(crate) document_id: DocumentId,
    pub(crate) frame_id: FrameId,
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) loading: bool,
    pub(crate) ready_state: DocumentReadyState,
    pub(crate) items: Vec<ObservedItem>,
    pub(crate) retained_at: Instant,
    pub(crate) retained_bytes: usize,
}

impl ViewSnapshot {
    pub(crate) fn measure_retained_bytes(&self) -> usize {
        serde_json::to_vec(&(
            &self.view_id,
            &self.document_id,
            &self.frame_id,
            &self.url,
            &self.title,
            self.loading,
            self.ready_state,
            &self.items,
        ))
        .expect("validated retained observation serializes")
        .len()
    }

    pub(crate) fn page(&self, offset: u32, max_items: u32) -> Result<Observation, ProtocolError> {
        let start = usize::try_from(offset)
            .map_err(|_| invalid_request("observation cursor offset is not representable"))?;
        if start >= self.items.len() && !(start == 0 && self.items.is_empty()) {
            return Err(invalid_request(
                "observation cursor offset is outside the retained view",
            ));
        }
        let limit = usize::try_from(max_items)
            .map_err(|_| invalid_request("observation item limit is not representable"))?;
        let end = start.saturating_add(limit).min(self.items.len());
        let truncated = end < self.items.len();
        let next_cursor = if truncated {
            Some(ObservationCursor {
                view_id: self.view_id.clone(),
                offset: u32::try_from(end).map_err(|_| {
                    internal_error("observation cursor offset exceeded protocol range")
                })?,
            })
        } else {
            None
        };
        Ok(Observation {
            view_id: self.view_id.clone(),
            document_id: self.document_id.clone(),
            frame_id: self.frame_id.clone(),
            url: self.url.clone(),
            title: self.title.clone(),
            loading: self.loading,
            ready_state: self.ready_state,
            items: self.items[start..end].to_vec(),
            next_cursor,
            truncated,
        })
    }
}
