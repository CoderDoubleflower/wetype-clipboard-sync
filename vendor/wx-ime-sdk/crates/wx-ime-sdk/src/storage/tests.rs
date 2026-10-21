use super::*;
use crate::{ClipboardContent, Device, DownloadedImage, ReceivedClipboard, ReceivedId};

fn image_receipt(store: &Store, version: u64) -> ReceivedClipboard {
    let digest = wx_ime_protocol::crypto::sha256(b"cached image bytes");
    ReceivedClipboard {
        id: ReceivedId {
            group: 10,
            version,
            from_uin: 2,
            trace_id: format!("image-{version}"),
        },
        source: Device {
            uin: 2,
            name: "phone".into(),
            platform: 1,
            supports_file_transfer: false,
        },
        content: ClipboardContent::Image(DownloadedImage {
            path: store
                .directory
                .join("cache")
                .join(format!("10-{version}-{digest}.image")),
            bytes: 18,
            sha256: digest,
        }),
        expire_at: 4_000_000_000,
    }
}

fn cache_file(store: &Store, version: u64) -> PathBuf {
    let receipt = image_receipt(store, version);
    let ClipboardContent::Image(image) = receipt.content else {
        unreachable!()
    };
    write_new(&image.path, b"cached image bytes").unwrap();
    image.path
}

#[test]
fn cleanup_retains_pending_and_foreign_files_and_directories() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    let cache = store.directory.join("cache");
    prepare_image_cache(&cache).unwrap();
    let retained = cache_file(&store, 20);
    let orphan = cache_file(&store, 19);
    let foreign = cache.join("personal.image");
    write_new(&foreign, b"keep me").unwrap();
    let noncanonical = cache.join(format!("010-1-{}.image", "a".repeat(64)));
    write_new(&noncanonical, b"keep me too").unwrap();
    let folder = cache.join(format!("10-18-{}.image", "a".repeat(64)));
    fs::create_dir(&folder).unwrap();
    store.state.pending = Some(image_receipt(&store, 20));
    store.save().unwrap();
    store.clean_cache().unwrap();
    assert!(retained.is_file());
    assert!(!orphan.exists());
    assert!(foreign.is_file());
    assert!(noncanonical.is_file());
    assert!(folder.is_dir());
    drop(store);
    let reopened = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    assert!(reopened.state.pending.is_some());
    assert!(retained.is_file());
}

#[test]
fn unsafe_pending_path_prevents_all_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    prepare_image_cache(&store.directory.join("cache")).unwrap();
    let orphan = cache_file(&store, 19);
    let mut pending = image_receipt(&store, 20);
    let ClipboardContent::Image(image) = &mut pending.content else {
        unreachable!()
    };
    image.path = store.directory.join("outside.image");
    store.state.pending = Some(pending);
    assert!(store.clean_cache().is_err());
    assert!(orphan.is_file());
}

#[cfg(unix)]
#[test]
fn cleanup_does_not_follow_cache_directory_or_object_symlinks() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    let cache = store.directory.join("cache");
    let outside = tempfile::tempdir().unwrap();
    let name = format!("10-19-{}.image", "a".repeat(64));
    let outside_image = outside.path().join(&name);
    write_new(&outside_image, b"must remain").unwrap();
    symlink(outside.path(), &cache).unwrap();
    assert!(store.clean_cache().is_err());
    assert!(prepare_image_cache(&cache).is_err());
    assert!(outside_image.is_file());
    fs::remove_file(&cache).unwrap();
    prepare_image_cache(&cache).unwrap();
    let link = cache.join(name);
    symlink(&outside_image, &link).unwrap();
    store.clean_cache().unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(outside_image).unwrap(), b"must remain");
}

#[test]
fn rejected_revision_never_replaces_newer_pending_or_moves_cursor_backward() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    let rejected = image_receipt(&store, 20).id;
    let newer = image_receipt(&store, 21);
    store.state.pending = Some(newer.clone());
    assert!(store.discard_clipboard(&rejected).is_err());
    assert_eq!(store.state.pending.as_ref().unwrap().id, newer.id);
    assert!(store.state.consumed.is_empty());
    store.state.pending = None;
    store.state.consumed.insert(10, 30);
    store.discard_clipboard(&rejected).unwrap();
    assert_eq!(store.state.consumed.get(&10), Some(&30));
    drop(store);
    let reopened = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    assert_eq!(reopened.state.consumed.get(&10), Some(&30));
}

#[test]
fn failed_discard_save_restores_pending_and_cursor_for_exact_retry() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned(), "test".into()).unwrap();
    let pending = image_receipt(&store, 20);
    store.state.pending = Some(pending.clone());
    store.state.consumed.insert(10, 19);
    let state_path = store.directory.join("identity.json");
    fs::remove_file(&state_path).unwrap();
    fs::create_dir(&state_path).unwrap();
    assert!(store.discard_clipboard(&pending.id).is_err());
    assert_eq!(store.state.pending.as_ref().unwrap().id, pending.id);
    assert_eq!(store.state.consumed.get(&10), Some(&19));
    fs::remove_dir(&state_path).unwrap();
    store.discard_clipboard(&pending.id).unwrap();
    assert!(store.state.pending.is_none());
    assert_eq!(store.state.consumed.get(&10), Some(&20));
}
