use std::collections::VecDeque;

use browser_protocol::{
    BrowserEvent, EventBatch, EventCommand, EventCursor, EventKind, ProfileId, ProtocolError,
    ServiceInstanceId,
};

use crate::errors::{event_gap, invalid_request};

const MAX_RETAINED_EVENTS: usize = 256;

struct RetainedEvent {
    profile_id: ProfileId,
    event: BrowserEvent,
}

pub(crate) struct EventLog {
    instance_id: ServiceInstanceId,
    events: VecDeque<RetainedEvent>,
    next_sequence: u64,
}

impl EventLog {
    pub(crate) fn new() -> Self {
        Self {
            instance_id: ServiceInstanceId::parse(format!("svc_{}", std::process::id()))
                .expect("process ID produces a valid service ID"),
            events: VecDeque::new(),
            next_sequence: 1,
        }
    }

    pub(crate) fn instance_id(&self) -> &ServiceInstanceId {
        &self.instance_id
    }

    pub(crate) fn push(&mut self, profile_id: &ProfileId, event: EventKind) {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("event sequence space exhausted");
        self.events.push_back(RetainedEvent {
            profile_id: profile_id.clone(),
            event: BrowserEvent {
                service_instance_id: self.instance_id.clone(),
                sequence,
                event,
            },
        });
        while self.events.len() > MAX_RETAINED_EVENTS {
            self.events.pop_front();
        }
    }

    pub(crate) fn read(
        &self,
        profile_id: &ProfileId,
        command: EventCommand,
    ) -> Result<EventBatch, ProtocolError> {
        if command.max_events == 0 {
            return Err(invalid_request("max_events must be greater than zero"));
        }
        let after = match command.cursor {
            Some(cursor) if cursor.service_instance_id != self.instance_id => {
                return Err(event_gap(self.first_available_sequence()));
            }
            Some(cursor) => cursor.sequence,
            None => 0,
        };
        let first_available = self.first_available_sequence();
        if after.saturating_add(1) < first_available {
            return Err(event_gap(first_available));
        }
        let limit = usize::try_from(command.max_events)
            .map_err(|_| invalid_request("max_events is not representable"))?;
        let events = self
            .events
            .iter()
            .filter(|retained| {
                &retained.profile_id == profile_id && retained.event.sequence > after
            })
            .take(limit)
            .map(|retained| retained.event.clone())
            .collect::<Vec<_>>();
        let latest_sequence = self.next_sequence.saturating_sub(1);
        let next_sequence = events
            .last()
            .map_or(latest_sequence, |event| event.sequence);
        Ok(EventBatch {
            events,
            next_cursor: EventCursor {
                service_instance_id: self.instance_id.clone(),
                sequence: next_sequence,
            },
        })
    }

    fn first_available_sequence(&self) -> u64 {
        self.events
            .front()
            .map_or(self.next_sequence, |retained| retained.event.sequence)
    }
}
