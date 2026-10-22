//! Latest-value delivery state. Observing, publishing, applying and acknowledging
//! are separate steps: a failed side effect must not advance the next step.

use crate::clipboard::{Payload, Snapshot};
use std::time::{Duration, Instant};
use wx_ime_sdk::{ClipboardContent, DownloadedImage, ReceivedClipboard, ReceivedId};

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
    pub payload: Payload,
    pub retry: Retry,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceiptPhase {
    Prepare,
    Apply,
    Acknowledge,
}

#[derive(Debug)]
pub(super) struct Incoming {
    pub message: ReceivedClipboard,
    pub payload: Option<Payload>,
    pub phase: ReceiptPhase,
    pub retry: Retry,
}

#[derive(Debug)]
pub(super) struct InvalidIncoming {
    pub id: ReceivedId,
    pub retry: Retry,
}

#[derive(Debug)]
pub(super) struct State {
    // None means the clipboard has not yet been read successfully. A snapshot
    // with no content still observes local changes to unsupported formats.
    pub observed: Option<Snapshot>,
    pub generation: u64,
    pub outgoing: Option<Outgoing>,
    pub incoming: Option<Incoming>,
    pub invalid: Option<InvalidIncoming>,
    pub last_published: Option<(u64, u64)>, // (group, server revision)
    send_current: bool,
    pub send_enabled: bool,
}

impl State {
    pub fn new(send_current: bool) -> Self {
        Self {
            observed: None,
            generation: 0,
            outgoing: None,
            incoming: None,
            invalid: None,
            last_published: None,
            send_current,
            send_enabled: true,
        }
    }

    /// Returns whether an already observed clipboard was changed by its owner.
    /// A new local value supersedes the unsent value, including an in-flight one.
    pub fn observe(&mut self, snapshot: Snapshot, now: Instant) -> bool {
        if self.observed.as_ref() == Some(&snapshot) {
            return false;
        }
        let first = self.observed.is_none();
        let content_changed = self
            .observed
            .as_ref()
            .is_none_or(|previous| previous.content != snapshot.content);
        self.generation = self.generation.wrapping_add(1);
        if self.send_enabled && content_changed && (!first || self.send_current) {
            self.outgoing = snapshot
                .content
                .as_ref()
                .filter(|payload| !matches!(payload, Payload::Text(text) if text.is_empty()))
                .map(|payload| Outgoing {
                    generation: self.generation,
                    payload: payload.clone(),
                    retry: Retry::new(now),
                    paused: false,
                });
        }
        // A Windows sequence change protects a repeated local copy from an
        // older download, but identical content is not uploaded again. Keep a
        // pending send's generation so its completion can still clear it, and
        // keep an uncertain send paused until an explicit retry/new content.
        self.observed = Some(snapshot);
        // A user copy after receipt delivery wins. Explicitly finish the old
        // receipt rather than leaving the SDK's already-emitted pending stuck.
        if !first {
            self.discard_incoming(now);
        }
        !first
    }

    pub fn outgoing_due(&self, now: Instant) -> Option<(u64, Payload)> {
        if !self.send_enabled {
            return None;
        }
        self.outgoing
            .as_ref()
            .filter(|pending| !pending.paused && pending.retry.at <= now)
            .map(|pending| (pending.generation, pending.payload.clone()))
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
        if let Some(pending) = &mut self.invalid {
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
        let (phase, payload) = if superseded {
            (ReceiptPhase::Acknowledge, None)
        } else {
            match &message.content {
                ClipboardContent::Text(text) => {
                    (ReceiptPhase::Apply, Some(Payload::Text(text.clone())))
                }
                ClipboardContent::Image(_) => (ReceiptPhase::Prepare, None),
            }
        };
        self.incoming = Some(Incoming {
            message,
            payload,
            phase,
            retry: Retry::new(now),
        });
        superseded
    }

    pub fn prepare_due(&self, now: Instant) -> Option<(ReceivedId, DownloadedImage)> {
        let pending = self
            .incoming
            .as_ref()
            .filter(|pending| pending.phase == ReceiptPhase::Prepare && pending.retry.at <= now)?;
        match &pending.message.content {
            ClipboardContent::Image(image) => Some((pending.message.id.clone(), image.clone())),
            ClipboardContent::Text(_) => None,
        }
    }

    pub fn prepared(&mut self, id: &ReceivedId, payload: Payload, now: Instant) -> bool {
        let Some(pending) = self
            .incoming
            .as_mut()
            .filter(|pending| &pending.message.id == id && pending.phase == ReceiptPhase::Prepare)
        else {
            return false;
        };
        pending.payload = Some(payload);
        pending.phase = ReceiptPhase::Apply;
        pending.retry.reset(now);
        true
    }

    pub fn preparation_failed(&mut self, id: &ReceivedId, now: Instant) -> bool {
        let Some(pending) = self
            .incoming
            .as_mut()
            .filter(|pending| &pending.message.id == id && pending.phase == ReceiptPhase::Prepare)
        else {
            return false;
        };
        pending.retry.failed(now);
        true
    }

    pub fn invalid_image(&mut self, id: &ReceivedId, now: Instant) -> bool {
        if self.incoming.as_ref().is_some_and(|pending| {
            &pending.message.id == id && pending.phase == ReceiptPhase::Prepare
        }) {
            self.discard_incoming(now);
            true
        } else {
            false
        }
    }

    pub fn apply_due(&self, now: Instant) -> bool {
        self.incoming
            .as_ref()
            .is_some_and(|pending| pending.phase == ReceiptPhase::Apply && pending.retry.at <= now)
    }

    pub fn applied(&mut self, snapshot: Snapshot, now: Instant) {
        if let Some(pending) = &mut self.incoming {
            if pending.phase != ReceiptPhase::Apply {
                return;
            }
            // The actual post-write revision/content becomes the observation
            // baseline. Image encoding changes and our own sequence increment
            // must not produce an upload or look like a local conflict.
            self.observed = Some(snapshot);
            pending.payload = None;
            pending.phase = ReceiptPhase::Acknowledge;
            pending.retry.reset(now);
        }
    }

    pub fn discard_incoming(&mut self, now: Instant) {
        if let Some(pending) = &mut self.incoming {
            if pending.phase != ReceiptPhase::Acknowledge {
                pending.payload = None;
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

    /// Invalid encrypted references can fail before the SDK emits a receipt.
    /// Keep that exact version until the SDK persists its explicit rejection.
    pub fn received_invalid(&mut self, id: ReceivedId, now: Instant) {
        self.invalid = Some(InvalidIncoming {
            id,
            retry: Retry::new(now),
        });
    }

    pub fn discard_due(&self, now: Instant) -> Option<ReceivedId> {
        self.invalid
            .as_ref()
            .filter(|pending| pending.retry.at <= now)
            .map(|pending| pending.id.clone())
    }

    pub fn discard_failed(&mut self, id: &ReceivedId, now: Instant) {
        if let Some(pending) = self.invalid.as_mut().filter(|pending| &pending.id == id) {
            pending.retry.failed(now);
        }
    }

    pub fn discarded(&mut self, id: &ReceivedId) {
        if self
            .invalid
            .as_ref()
            .is_some_and(|pending| &pending.id == id)
        {
            self.invalid = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const RED_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn image() -> Payload {
        Payload::Image(Arc::new(
            crate::image::ClipboardImage::from_encoded(RED_PNG).unwrap(),
        ))
    }

    fn snapshot(content: Option<Payload>, revision: Option<u32>) -> Snapshot {
        Snapshot { content, revision }
    }

    fn text(value: &str) -> Snapshot {
        snapshot(Some(Payload::Text(value.into())), None)
    }

    fn remote_content(content: serde_json::Value, version: u64) -> ReceivedClipboard {
        serde_json::from_value(serde_json::json!({
            "id": { "group": 10, "version": version, "from_uin": 2, "trace_id": format!("remote-{version}") },
            "source": { "uin": 2, "name": "phone", "platform": 1, "supports_file_transfer": false },
            "content": content,
            "expire_at": 4_000_000_000u64
        })).unwrap()
    }

    fn remote(text: &str, version: u64) -> ReceivedClipboard {
        remote_content(serde_json::json!({ "Text": text }), version)
    }

    fn remote_image(version: u64) -> ReceivedClipboard {
        remote_content(
            serde_json::json!({ "Image": {
            "path": "cached-image-for-state-test.png", "bytes": RED_PNG.len(), "sha256": "fixture"
        } }),
            version,
        )
    }

    fn running(now: Instant) -> State {
        let mut state = State::new(false);
        state.observe(text("before startup"), now);
        assert!(state.outgoing_due(now).is_none());
        state
    }

    fn finish_receipt(state: &mut State, now: Instant) {
        let id = state.ack_due(now).expect("receipt is safe to acknowledge");
        state.acknowledged(&id);
        assert!(state.incoming.is_none());
    }

    #[test]
    fn clipboard_busy_retains_text_until_write_and_ack_both_succeed() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote("from phone", 20), state.generation, now);
        state.incoming_failed(now); // Simulated OpenClipboard failure.
        assert!(state.ack_due(now).is_none());
        assert!(!state.apply_due(now));
        let retry = now + Duration::from_secs(1);
        assert!(state.apply_due(retry)); // No second SDK emission is needed.
        state.applied(text("from phone"), retry);
        let id = state.ack_due(retry).unwrap();
        state.incoming_failed(retry); // Simulated state-file save failure in ack.
        assert!(!state.apply_due(retry + Duration::from_secs(30)));
        assert!(state.incoming.is_some());
        let retry = retry + Duration::from_secs(1);
        assert_eq!(state.ack_due(retry), Some(id.clone()));
        state.acknowledged(&id);
        state.observe(text("from phone"), retry);
        assert!(state.outgoing.is_none());
    }

    #[test]
    fn image_write_retries_then_ack_retries_without_rewrite_or_echo() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote_image(20), state.generation, now);
        assert!(!state.apply_due(now));
        assert!(state.ack_due(now).is_none());
        let (id, _) = state.prepare_due(now).unwrap();
        let pixels = image();
        assert!(state.prepared(&id, pixels.clone(), now));
        state.incoming_failed(now); // Clipboard is temporarily owned by another app.
        assert!(state.ack_due(now).is_none());
        let retry = now + Duration::from_secs(1);
        assert!(state.apply_due(retry));
        let written = snapshot(Some(pixels), Some(14));
        state.applied(written.clone(), retry);
        state.incoming_failed(retry); // The durable acknowledgement fails once.
        assert!(!state.apply_due(retry + Duration::from_secs(60)));
        assert!(state.prepare_due(retry + Duration::from_secs(60)).is_none());
        assert!(!state.observe(written, retry));
        assert!(state.outgoing.is_none());
        finish_receipt(&mut state, retry + Duration::from_secs(1));
    }

    #[test]
    fn alternating_remote_text_and_image_updates_the_baseline_without_echo() {
        let now = Instant::now();
        let mut state = running(now);
        for (version, content) in [
            (20, image()),
            (21, Payload::Text("after image".into())),
            (22, image()),
        ] {
            let receipt = match &content {
                Payload::Text(value) => remote(value, version),
                Payload::Image(_) => remote_image(version),
            };
            state.received(receipt, state.generation, now);
            if let Some((id, _)) = state.prepare_due(now) {
                state.prepared(&id, content.clone(), now);
            }
            assert!(state.apply_due(now));
            let written = snapshot(Some(content), Some(version as u32));
            state.applied(written.clone(), now);
            finish_receipt(&mut state, now);
            assert!(!state.observe(written, now));
            assert!(state.outgoing.is_none());
        }
    }

    #[test]
    fn copy_during_slow_download_preserves_new_local_image() {
        let now = Instant::now();
        let mut state = running(now);
        let poll_generation = state.generation;
        let local = image();
        state.observe(snapshot(Some(local.clone()), Some(8)), now);
        assert!(state.received(remote_image(20), poll_generation, now));
        assert!(state.prepare_due(now).is_none());
        assert!(!state.apply_due(now));
        finish_receipt(&mut state, now);
        assert_eq!(state.outgoing_due(now).unwrap().1, local);
    }

    #[test]
    fn copy_during_image_decode_makes_its_late_result_harmless() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote_image(20), state.generation, now);
        let (id, _) = state.prepare_due(now).unwrap();
        state.observe(text("new local text"), now);
        assert!(!state.prepared(&id, image(), now));
        assert!(!state.preparation_failed(&id, now));
        assert!(!state.invalid_image(&id, now));
        assert!(!state.apply_due(now));
        finish_receipt(&mut state, now);
        assert_eq!(
            state.outgoing_due(now).unwrap().1,
            Payload::Text("new local text".into())
        );
    }

    #[test]
    fn copy_while_retrying_a_busy_clipboard_cancels_old_application() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote("old phone value", 20), state.generation, now);
        state.incoming_failed(now);
        let local = snapshot(Some(image()), Some(7));
        state.observe(local.clone(), now);
        assert!(!state.apply_due(now + Duration::from_secs(30)));
        assert!(state.ack_due(now).is_some());
        assert_eq!(state.observed, Some(local));
    }

    #[test]
    fn sequence_only_local_copy_protects_against_slow_poll_without_upload() {
        let now = Instant::now();
        let mut state = State::new(false);
        let pixels = image();
        state.observe(snapshot(Some(pixels.clone()), Some(8)), now);
        let poll_generation = state.generation;
        state.observe(snapshot(Some(pixels), Some(9)), now);
        assert!(state.outgoing.is_none());
        assert!(state.received(remote_image(20), poll_generation, now));
        assert!(!state.apply_due(now));
        finish_receipt(&mut state, now);
    }

    #[test]
    fn identical_copy_does_not_duplicate_an_in_flight_send() {
        let now = Instant::now();
        let mut state = running(now);
        let pixels = image();
        state.observe(snapshot(Some(pixels.clone()), Some(8)), now);
        let (sending, _) = state.outgoing_due(now).unwrap();
        state.observe(snapshot(Some(pixels), Some(9)), now);
        assert_ne!(state.generation, sending);
        assert_eq!(state.outgoing_due(now).unwrap().0, sending);
        state.sent(sending, 10, 20);
        assert!(state.outgoing.is_none());
    }

    #[test]
    fn disconnected_proxy_retries_only_latest_content_across_types() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(text("A"), now);
        let (a, _) = state.outgoing_due(now).unwrap();
        state.send_failed(a, PublishFailure::Retry, now);
        assert!(state.outgoing_due(now).is_none());
        let pixels = image();
        state.observe(snapshot(Some(pixels.clone()), Some(10)), now);
        let (b, payload) = state.outgoing_due(now).unwrap();
        assert_eq!(payload, pixels);
        state.sent(a, 10, 20); // An older in-flight success cannot clear the image.
        assert_eq!(state.outgoing_due(now).unwrap().0, b);
        state.send_failed(b, PublishFailure::Retry, now);
        state.observe(text("C"), now);
        let (c, payload) = state.outgoing_due(now).unwrap();
        assert_eq!(payload, Payload::Text("C".into()));
        state.sent(b, 10, 21); // The old image must not clear the newer text.
        assert_eq!(state.outgoing_due(now).unwrap().0, c);
        state.sent(c, 10, 22);
        assert!(state.outgoing.is_none());
        assert_eq!(state.last_published, Some((10, 22)));
    }

    #[test]
    fn definitive_send_failure_does_not_turn_observation_into_success() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(text("retry me"), now);
        let (generation, _) = state.outgoing_due(now).unwrap();
        state.send_failed(generation, PublishFailure::Retry, now);
        state.observe(text("retry me"), now + Duration::from_secs(1));
        assert_eq!(
            state.outgoing_due(now + Duration::from_secs(1)).unwrap().1,
            Payload::Text("retry me".into())
        );
        assert!(state.last_published.is_none());
    }

    #[test]
    fn uncertain_image_publication_requires_explicit_retry_or_new_content() {
        let now = Instant::now();
        let mut state = running(now);
        let pixels = image();
        state.observe(snapshot(Some(pixels.clone()), Some(8)), now);
        let (generation, _) = state.outgoing_due(now).unwrap();
        state.send_failed(generation, PublishFailure::OutcomeUnknown, now);
        state.observe(snapshot(Some(pixels.clone()), Some(9)), now);
        assert!(state.outgoing_due(now + Duration::from_secs(600)).is_none());
        state.force_retry(now);
        assert_eq!(state.outgoing_due(now).unwrap().1, pixels);
        state.send_failed(generation, PublishFailure::OutcomeUnknown, now);
        state.observe(text("new content"), now);
        assert_eq!(
            state.outgoing_due(now).unwrap().1,
            Payload::Text("new content".into())
        );
    }

    #[test]
    fn temporarily_unreadable_image_retries_before_invalid_image_is_consumed() {
        let now = Instant::now();
        let mut state = running(now);
        state.received(remote_image(20), state.generation, now);
        let (id, _) = state.prepare_due(now).unwrap();
        assert!(state.preparation_failed(&id, now)); // Temporary cache file read failure.
        assert!(state.prepare_due(now).is_none());
        assert!(state.ack_due(now).is_none());
        let retry = now + Duration::from_secs(1);
        assert_eq!(state.prepare_due(retry).unwrap().0, id);
        assert!(state.invalid_image(&id, retry)); // Deterministically oversized/invalid decode.
        assert!(!state.apply_due(retry));
        finish_receipt(&mut state, retry);
        state.received(remote("next usable item", 21), state.generation, retry);
        assert!(state.apply_due(retry));
        state.applied(text("next usable item"), retry);
        finish_receipt(&mut state, retry);
        assert_eq!(state.observed, Some(text("next usable item")));
    }

    #[test]
    fn invalid_download_retries_exact_discard_without_losing_latest_outbox() {
        let now = Instant::now();
        let mut state = running(now);
        let id = remote_image(20).id;
        state.received_invalid(id.clone(), now);
        assert_eq!(state.discard_due(now), Some(id.clone()));
        state.discard_failed(&id, now);
        state.observe(snapshot(Some(image()), Some(9)), now);
        assert!(state.discard_due(now).is_none());
        let retry = now + Duration::from_secs(1);
        assert_eq!(state.discard_due(retry), Some(id.clone()));
        state.discarded(&remote_image(19).id);
        assert!(state.invalid.is_some()); // A different version cannot clear it.
        state.discarded(&id);
        assert!(state.invalid.is_none());
        let (generation, _) = state.outgoing_due(retry).unwrap();
        state.sent(generation, 10, 21);
        state.received(remote("next remote", 22), state.generation, retry);
        assert!(state.apply_due(retry));
    }

    #[test]
    fn unsupported_local_selection_stops_retrying_stale_text_or_image() {
        let now = Instant::now();
        let mut state = running(now);
        state.observe(snapshot(Some(image()), Some(8)), now);
        let empty = snapshot(None, Some(9));
        state.observe(empty.clone(), now);
        assert!(state.outgoing.is_none());
        assert_eq!(state.observed, Some(empty));
    }

    #[test]
    fn recovered_old_image_cannot_replace_newer_published_content() {
        let now = Instant::now();
        let mut state = running(now);
        state.sent(state.generation, 10, 30);
        assert!(state.received(remote_image(29), state.generation, now));
        assert!(state.prepare_due(now).is_none());
        assert!(!state.apply_due(now));
        assert!(state.ack_due(now).is_some());
    }

    #[test]
    fn startup_sends_existing_text_or_image_only_when_requested() {
        let now = Instant::now();
        for content in [Payload::Text("send at startup".into()), image()] {
            let initial = snapshot(Some(content.clone()), None);
            let mut quiet = State::new(false);
            quiet.observe(initial.clone(), now);
            assert!(quiet.outgoing.is_none());
            let mut sending = State::new(true);
            sending.observe(initial, now);
            assert_eq!(sending.outgoing_due(now).unwrap().1, content);
        }
    }
}
