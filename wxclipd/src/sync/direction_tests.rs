use super::*;
use crate::clipboard::Snapshot;

fn payloads() -> [Payload; 2] {
    [
        Payload::Text("local clipboard".into()),
        Payload::Image(Arc::new(
            crate::image::ClipboardImage::from_rgba(1, 1, vec![12, 34, 56, 255]).unwrap(),
        )),
    ]
}

#[test]
fn all_four_modes_gate_text_images_and_manual_retries() {
    for send in [false, true] {
        for receive in [false, true] {
            for payload in payloads() {
                let now = Instant::now();
                let mut state = State::new(true);
                state.send_enabled = send;
                state.observe(
                    Snapshot {
                        content: Some(payload.clone()),
                        revision: Some(1),
                    },
                    now,
                );
                state.force_retry(now);
                let mut requested = true;
                let command = next_network_command(
                    &state,
                    send,
                    receive,
                    &mut requested,
                    &Retry::new(now),
                    now,
                );
                match (send, receive, command) {
                    (
                        true,
                        _,
                        Some(NetworkCommand::Send {
                            payload: actual, ..
                        }),
                    ) => assert_eq!(actual, payload),
                    (false, true, Some(NetworkCommand::Poll { .. })) => assert!(!requested),
                    (false, false, None) => {}
                    _ => panic!("direction policy was bypassed"),
                }
                if !send {
                    assert!(state.outgoing.is_none());
                    state.observe(
                        Snapshot {
                            content: Some(Payload::Text("next local copy".into())),
                            revision: Some(2),
                        },
                        now,
                    );
                    state.force_retry(now + Duration::from_secs(60));
                    assert!(
                        state.outgoing.is_none(),
                        "receive-only must not build an outbox or starve polling"
                    );
                }
            }
        }
    }
}

#[test]
fn send_only_never_follows_publication_with_a_remote_poll() {
    let now = Instant::now();
    let mut state = State::new(true);
    state.observe(
        Snapshot {
            content: Some(Payload::Text("text".into())),
            revision: Some(1),
        },
        now,
    );
    state.sent(state.generation, 10, 1);
    assert!(next_network_command(&state, true, false, &mut true, &Retry::new(now), now).is_none());
}

#[test]
fn receiving_only_keeps_local_conflict_protection_without_uploading() {
    let now = Instant::now();
    let mut state = State::new(false);
    state.send_enabled = false;
    state.observe(
        Snapshot {
            content: None,
            revision: Some(1),
        },
        now,
    );
    let message: ReceivedClipboard = serde_json::from_value(serde_json::json!({
        "id": {"group":10,"version":1,"from_uin":2,"trace_id":"direction-test"},
        "source": {"uin":2,"name":"phone","platform":1,"supports_file_transfer":false},
        "content": {"Text":"remote"}, "expire_at":4_000_000_000u64
    }))
    .unwrap();
    assert!(!state.received(message, state.generation, now));
    assert!(state.apply_due(now));
    state.observe(
        Snapshot {
            content: Some(Payload::Text("new local copy".into())),
            revision: Some(2),
        },
        now,
    );
    assert!(!state.apply_due(now));
    assert!(state.outgoing.is_none());
    assert!(state.ack_due(now).is_some());
}

#[test]
fn restarting_to_enable_sending_does_not_publish_the_disabled_period() {
    let now = Instant::now();
    let baseline = Snapshot {
        content: Some(Payload::Text("copied while disabled".into())),
        revision: Some(1),
    };
    let mut enabled = State::new(false);
    enabled.observe(baseline, now);
    enabled.force_retry(now);
    assert!(enabled.outgoing_due(now).is_none());
    enabled.observe(
        Snapshot {
            content: Some(Payload::Text("new copy after enabling".into())),
            revision: Some(2),
        },
        now,
    );
    assert!(enabled.outgoing_due(now).is_some());
}
