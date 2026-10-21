//! Latest-value delivery state. Observing, publishing, applying and acknowledging
//! are separate steps: a failed side effect must not advance the next step.

use std::time::{Duration, Instant};
use wx_ime_sdk::{ClipboardContent, ReceivedClipboard, ReceivedId};

#[derive(Debug)]
pub(super) struct Retry {
    pub failures: u32,
    pub at: Instant,
}

impl Retry {
    pub fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            at: now,
        }
    }

    pub fn reset(&mut self, now: Instant) {
        *self = Self::new(now);
    }

    pub fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let seconds = (1u64 << self.failures.saturating_sub(1).min(5)).min(30);
        self.at = now + Duration::from_secs(seconds);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublishFailure {
    Retry,
    OutcomeUnknown,
    Rejected,
}

#[derive(Debug)]
pub(super) struct Outgoing {
    pub generation: u64,
    pub text: String,
    pub retry: Retry,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceiptPhase {
    Apply,
    Acknowledge,
}

#[derive(Debug)]
pub(super) struct Incoming {
    pub message: ReceivedClipboard,
    pub phase: ReceiptPhase,
    pub retry: Retry,
}

#[derive(Debug)]
pub(super) struct State {
    // The outer None means the clipboard has not yet been read successfully.
    // The inner None is a successfully observed non-text/empty clipboard.
    pub observed: Option<Option<String>>,
    pub generation: u64,
    pub outgoing: Option<Outgoing>,
    pub incoming: Option<Incoming>,
    pub last_published: Option<(u64, u64)>, // (group, server revision)
    send_current: bool,
}

impl State {
    pub fn new(send_current: bool) -> Self {
        Self {
            observed: None,
            generation: 0,
            outgoing: None,
            incoming: None,
            last_published: None,
            send_current,
        }
    }

    /// Returns whether an already observed clipboard was changed by its owner.
    /// A new local value supersedes the unsent value, including an in-flight one.
    pub fn observe(&mut self, text: Option<String>, now: Instant) -> bool {
        if self.observed.as_ref() == Some(&text) {
            return false;
        }
        let first = self.observed.is_none();
        self.generation = self.generation.wrapping_add(1);
        if !first || self.send_current {
            self.outgoing = text
                .as_ref()
                .filter(|text| !text.is_empty())
                .map(|text| Outgoing {
                    generation: self.generation,
                    text: text.clone(),
                    retry: Retry::new(now),
                    paused: false,
                });
        }
        self.observed = Some(text);
        // A user copy after receipt delivery wins. Explicitly finish the old
        // receipt rather than leaving the SDK's already-emitted pending stuck.
        if !first {
            self.discard_incoming(now);
        }
        !first
    }

    pub fn outgoing_due(&self, now: Instant) -> Option<(u64, String)> {
        self.outgoing
            .as_ref()
            .filter(|pending| !pending.paused && pending.retry.at <= now)
            .map(|pending| (pending.generation, pending.text.clone()))
    }

    pub fn sent(&mut self, generation: u64, group: u64, version: u64) {
        self.last_published = Some((group, version));
        if self
            .outgoing
            .as_ref()
            .is_some_and(|pending| pending.generation == generation)
        {
            self.outgoing = None;
        }
    }

    pub fn send_failed(&mut self, generation: u64, failure: PublishFailure, now: Instant) {
        if let Some(pending) = self
            .outgoing
            .as_mut()
            .filter(|p| p.generation == generation)
        {
            pending.retry.failed(now);
            pending.paused = failure != PublishFailure::Retry;
        }
    }

    pub fn force_retry(&mut self, now: Instant) {
        if let Some(pending) = &mut self.outgoing {
            pending.paused = false;
            pending.retry.reset(now);
        }
        if let Some(pending) = &mut self.incoming {
            pending.retry.reset(now);
        }
    }

    /// Retain the entire receipt: the SDK intentionally emits it only once per
    /// process, so polling again is not a retry of a failed host clipboard write.
    pub fn received(
        &mut self,
        message: ReceivedClipboard,
        poll_generation: u64,
        now: Instant,
    ) -> bool {
        let superseded = self.generation != poll_generation
            || self.outgoing.is_some()
            || self.last_published.is_some_and(|(group, version)| {
                group == message.id.group() && message.id.version() <= version
            });
        let text = matches!(&message.content, ClipboardContent::Text(_));
        self.incoming = Some(Incoming {
            message,
            phase: if superseded || !text {
                ReceiptPhase::Acknowledge
            } else {
                ReceiptPhase::Apply
            },
            retry: Retry::new(now),
        });
        superseded
    }

    pub fn apply_due(&self, now: Instant) -> bool {
        self.incoming
            .as_ref()
            .is_some_and(|pending| pending.phase == ReceiptPhase::Apply && pending.retry.at <= now)
    }

    pub fn applied(&mut self, now: Instant) {
        if let Some(pending) = &mut self.incoming {
            if let ClipboardContent::Text(text) = &pending.message.content {
                // Our own write becomes the observation baseline, never an upload.
                self.observed = Some(Some(text.clone()));
            }
            pending.phase = ReceiptPhase::Acknowledge;
            pending.retry.reset(now);
        }
    }

    pub fn discard_incoming(&mut self, now: Instant) {
        if let Some(pending) = &mut self.incoming {
            if pending.phase == ReceiptPhase::Apply {
                pending.phase = ReceiptPhase::Acknowledge;
                pending.retry.reset(now);
            }
        }
    }

    pub fn incoming_failed(&mut self, now: Instant) {
        if let Some(pending) = &mut self.incoming {
            pending.retry.failed(now);
        }
    }

    pub fn ack_due(&self, now: Instant) -> Option<ReceivedId> {
        self.incoming
            .as_ref()
            .filter(|pending| pending.phase == ReceiptPhase::Acknowledge && pending.retry.at <= now)
            .map(|pending| pending.message.id.clone())
    }

    pub fn acknowledged(&mut self, id: &ReceivedId) {
        if self
            .incoming
            .as_ref()
            .is_some_and(|pending| &pending.message.id == id)
        {
            self.incoming = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(text: &str, version: u64) -> ReceivedClipboard {
        serde_json::from_value(serde_json::json!({
            "id": { "group": 10, "version": version, "from_uin": 2, "trace_id": format!("remote-{version}") },
            "source": { "uin": 2, "name": "phone", "platform": 1, "supports_file_transfer": false },
            "content": { "Text": text },
            "expire_at": 4_000_000_000u64
        })).unwrap()
    }

    fn running(now: Instant) -> State {
        let mut state = State::new(false);
        state.observe(Some("before startup".into()), now);
        assert!(state.outgoing_due(now).is_none());
        state
    }

    #[test]
    fn clipboard_busy_retains_receipt_until_write_and_ack_both_succeed() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote("from phone", 20), state.generation, now);
        state.incoming_failed(now); // Simulated OpenClipboard failure.
        assert!(state.ack_due(now).is_none());
        assert!(!state.apply_due(now));
        let retry = now + Duration::from_secs(1);
        assert!(state.apply_due(retry)); // No second SDK emission is needed.
        state.applied(retry);
        let id = state.ack_due(retry).unwrap();
        state.incoming_failed(retry); // Simulated state-file save failure in ack.
        assert!(!state.apply_due(retry + Duration::from_secs(30)));
        assert!(state.incoming.is_some());
        let retry = retry + Duration::from_secs(1);
        assert_eq!(state.ack_due(retry), Some(id.clone()));
        state.acknowledged(&id);
        assert!(state.incoming.is_none());
        state.observe(Some("from phone".into()), retry);
        assert!(state.outgoing.is_none()); // No clipboard echo loop.
    }

    #[test]
    fn copy_during_slow_poll_is_uploaded_and_old_receipt_is_explicitly_finished() {
        let now = Instant::now();
        let mut state = running(now);
        let poll_generation = state.generation;
        state.observe(Some("new local copy".into()), now);
        assert!(state.received(remote("old phone value", 20), poll_generation, now));
        assert!(!state.apply_due(now));
        let id = state.ack_due(now).unwrap();
        state.acknowledged(&id);
        assert!(state.incoming.is_none());
        assert_eq!(state.outgoing_due(now).unwrap().1, "new local copy");
    }

    #[test]
    fn copy_while_retrying_a_busy_clipboard_cancels_old_application() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote("old phone value", 20), state.generation, now);
        state.incoming_failed(now);
        state.observe(Some("new local copy".into()), now);
        assert!(!state.apply_due(now + Duration::from_secs(30)));
        assert!(state.ack_due(now).is_some());
        assert_eq!(state.observed, Some(Some("new local copy".into())));
    }

    #[test]
    fn disconnected_proxy_retries_only_the_latest_local_copy() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(Some("A".into()), now);
        let (a, _) = state.outgoing_due(now).unwrap();
        state.send_failed(a, PublishFailure::Retry, now);
        assert!(state.outgoing_due(now).is_none());
        state.observe(Some("B".into()), now);
        let (b, text) = state.outgoing_due(now).unwrap();
        assert_eq!(text, "B");
        state.sent(a, 10, 20); // An older in-flight success cannot clear B.
        assert_eq!(state.outgoing_due(now).unwrap().0, b);
        state.sent(b, 10, 21);
        assert!(state.outgoing.is_none());
        assert_eq!(state.last_published, Some((10, 21)));
    }

    #[test]
    fn definitive_send_failure_does_not_turn_observation_into_success() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(Some("retry me".into()), now);
        let (generation, _) = state.outgoing_due(now).unwrap();
        state.send_failed(generation, PublishFailure::Retry, now);
        state.observe(Some("retry me".into()), now + Duration::from_secs(1));
        assert_eq!(
            state.outgoing_due(now + Duration::from_secs(1)).unwrap().1,
            "retry me"
        );
        assert!(state.last_published.is_none());
    }

    #[test]
    fn uncertain_publication_waits_for_explicit_retry_or_a_new_copy() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(Some("A".into()), now);
        let (generation, _) = state.outgoing_due(now).unwrap();
        state.send_failed(generation, PublishFailure::OutcomeUnknown, now);
        assert!(state.outgoing_due(now + Duration::from_secs(600)).is_none());
        state.force_retry(now);
        assert_eq!(state.outgoing_due(now).unwrap().1, "A");
        state.send_failed(generation, PublishFailure::OutcomeUnknown, now);
        state.observe(Some("B".into()), now);
        assert_eq!(state.outgoing_due(now).unwrap().1, "B");
    }

    #[test]
    fn a_new_non_text_selection_stops_retrying_stale_text() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(Some("old text".into()), now);
        state.observe(None, now);
        assert!(state.outgoing.is_none());
        assert_eq!(state.observed, Some(None));
    }

    #[test]
    fn recovered_old_receipt_cannot_replace_a_newer_published_version() {
        let now = Instant::now();
        let mut state = running(now);
        state.sent(state.generation, 10, 30);
        assert!(state.received(remote("older", 29), state.generation, now));
        assert!(!state.apply_due(now));
        assert!(state.ack_due(now).is_some());
    }

    #[test]
    fn startup_sends_existing_text_only_when_requested() {
        let now = Instant::now();
        let mut state = State::new(true);
        state.observe(Some("send at startup".into()), now);
        assert_eq!(state.outgoing_due(now).unwrap().1, "send at startup");
    }
}
