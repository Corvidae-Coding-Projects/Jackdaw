use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use browser_net::RequestControl;
use browser_protocol::{ProfileId, TabId};

type NavigationKey = (ProfileId, TabId);

#[derive(Default)]
struct CancellationState {
    controls: BTreeMap<NavigationKey, RequestControl>,
    pending: BTreeSet<NavigationKey>,
}

/// Thread-safe cancellation signals for top-level navigations owned by the service thread.
///
/// Socket readers may clone this handle and signal a navigation while the document runtime remains
/// exclusively owned by `BrowserService`.
#[derive(Clone, Default)]
pub struct NavigationCancellation {
    state: Arc<Mutex<CancellationState>>,
}

impl NavigationCancellation {
    pub(crate) fn register(&self, profile_id: ProfileId, tab_id: TabId, control: RequestControl) {
        let key = (profile_id, tab_id);
        let mut state = self
            .state
            .lock()
            .expect("navigation cancellation registry lock poisoned");
        if state.pending.remove(&key) {
            control.cancel();
        }
        state.controls.insert(key, control);
    }

    pub(crate) fn finish(&self, profile_id: &ProfileId, tab_id: &TabId) {
        let key = (profile_id.clone(), tab_id.clone());
        let mut state = self
            .state
            .lock()
            .expect("navigation cancellation registry lock poisoned");
        state.controls.remove(&key);
        state.pending.remove(&key);
    }

    pub(crate) fn clear_pending(&self, profile_id: &ProfileId, tab_id: &TabId) {
        let mut state = self
            .state
            .lock()
            .expect("navigation cancellation registry lock poisoned");
        state.pending.remove(&(profile_id.clone(), tab_id.clone()));
    }

    /// Cancel the selected tab, or every in-flight navigation in the profile when no tab is named.
    pub fn cancel(&self, profile_id: &ProfileId, tab_id: Option<&TabId>) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("navigation cancellation registry lock poisoned");
        let matching_keys = state
            .controls
            .keys()
            .filter(|(candidate_profile, candidate_tab)| {
                candidate_profile == profile_id
                    && tab_id.is_none_or(|selected| selected == candidate_tab)
            })
            .cloned()
            .collect::<Vec<_>>();
        for key in &matching_keys {
            state
                .controls
                .get(key)
                .expect("matching navigation control remains registered")
                .cancel();
        }
        let remembered_pending = if matching_keys.is_empty()
            && let Some(tab_id) = tab_id
        {
            state.pending.insert((profile_id.clone(), tab_id.clone()))
        } else {
            false
        };
        !matching_keys.is_empty() || remembered_pending
    }
}
