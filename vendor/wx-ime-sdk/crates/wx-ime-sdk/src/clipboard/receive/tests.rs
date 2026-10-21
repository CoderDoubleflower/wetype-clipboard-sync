use super::*;
use crate::{client::Inner, session::Session, storage::Store};
use std::sync::{Arc, atomic::AtomicBool};
use tokio::sync::Mutex;

fn offline(directory: &std::path::Path) -> Client {
    let store = Store::open(directory.to_owned(), "test".into()).unwrap();
    Client {
        inner: Arc::new(Inner {
            session: Mutex::new(Session::offline_for_test(store).unwrap()),
            closed: AtomicBool::new(false),
            lifetime: tokio_util::sync::CancellationToken::new(),
            pairing: AtomicBool::new(false),
            cos: crate::clipboard::cos::Cos::new(None, None).unwrap(),
            proxy: None,
            receive: Mutex::new(ReceiveState::default()),
        }),
    }
}

fn id(version: u64) -> ReceivedId {
    ReceivedId {
        group: 10,
        version,
        from_uin: 2,
        trace_id: format!("image-{version}"),
    }
}

#[test]
fn only_content_failures_are_eligible_for_explicit_discard() {
    let mut receive = ReceiveState::default();
    for error in [
        Error::CosTransport,
        Error::CosHttp(403),
        Error::CosHttp(500),
        Error::Proxy("authentication rejected"),
        Error::Timeout("COS"),
        Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
    ] {
        assert!(!matches!(
            receive.image_error(&id(20), error),
            Error::InvalidClipboard { .. }
        ));
        assert!(receive.rejected.is_none());
    }
    for error in [
        Error::Invalid("COS download exceeds limit"),
        Error::Invalid("empty COS object"),
        Error::Protocol(wx_ime_protocol::Error::Invalid(
            "image plaintext MD5 mismatch",
        )),
        Error::Protocol(ImageReference::decode(b"not JSON").unwrap_err()),
    ] {
        let rejected = receive.image_error(&id(20), error);
        assert!(
            matches!(rejected, Error::InvalidClipboard { id: received, .. } if received == id(20))
        );
        assert_eq!(receive.rejected, Some(id(20)));
    }
}

#[tokio::test]
async fn discard_requires_the_exact_observed_rejection_and_retries_after_save_failure() {
    let directory = tempfile::tempdir().unwrap();
    let client = offline(directory.path());
    assert!(client.discard_clipboard(id(20)).await.is_err());
    client
        .inner
        .receive
        .lock()
        .await
        .reject_image(&id(21), "invalid image reference");
    assert!(client.discard_clipboard(id(20)).await.is_err());
    let state_path = directory.path().join("identity.json");
    std::fs::remove_file(&state_path).unwrap();
    std::fs::create_dir(&state_path).unwrap();
    assert!(client.discard_clipboard(id(21)).await.is_err());
    assert_eq!(client.inner.receive.lock().await.rejected, Some(id(21)));
    assert!(
        client
            .session()
            .await
            .unwrap()
            .store
            .state
            .consumed
            .is_empty()
    );
    std::fs::remove_dir(&state_path).unwrap();
    client.discard_clipboard(id(21)).await.unwrap();
    assert!(client.inner.receive.lock().await.rejected.is_none());
    assert_eq!(
        client
            .session()
            .await
            .unwrap()
            .store
            .state
            .consumed
            .get(&10),
        Some(&21)
    );
}

#[tokio::test]
async fn prune_waits_for_receive_commit_and_ack_still_reclaims_the_image() {
    let directory = tempfile::tempdir().unwrap();
    let client = offline(directory.path());
    let receiving = client.inner.receive.lock().await;
    let cache = client
        .session()
        .await
        .unwrap()
        .store
        .directory
        .join("cache");
    prepare_image_cache(&cache).unwrap();
    let plain = b"received encoded image";
    let digest = sha256(plain);
    let path = cache.join(format!("10-20-{digest}.image"));
    std::fs::write(&path, plain).unwrap();
    // This is the interval between writing the downloaded object and committing
    // the receipt: prune must not see the new object as an orphan.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), client.prune_image_cache())
            .await
            .is_err()
    );
    assert!(path.is_file());
    {
        let mut session = client.session().await.unwrap();
        session.store.state.pending = Some(ReceivedClipboard {
            id: id(20),
            source: crate::Device {
                uin: 2,
                name: "phone".into(),
                platform: 1,
                supports_file_transfer: false,
            },
            content: ClipboardContent::Image(DownloadedImage {
                path: path.clone(),
                bytes: plain.len(),
                sha256: digest,
            }),
            expire_at: 4_000_000_000,
        });
        session.store.save().unwrap();
    }
    drop(receiving);
    client.prune_image_cache().await.unwrap();
    assert!(path.is_file());
    client.acknowledge(id(20)).await.unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn image_bytes_validate_size_before_network_or_session_use() {
    let directory = tempfile::tempdir().unwrap();
    let client = offline(directory.path());
    client.shutdown().await.unwrap();
    assert!(matches!(
        client.send_image_bytes(&[]).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        client.send_image_bytes(&vec![0; MAX_IMAGE_BYTES + 1]).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        client.send_image_bytes(b"encoded image").await,
        Err(Error::Closed)
    ));
}
