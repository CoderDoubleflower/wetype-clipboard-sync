use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, MutexGuard};

pub(super) static IMAGE_DECODES: AtomicUsize = AtomicUsize::new(0);
static NATIVE_TEST_ACCESS: Mutex<()> = Mutex::new(());

// Serialize whole native test cases, not just individual API calls, and restore
// a supported clipboard value even when an assertion unwinds the test.
struct RestoreClipboard {
    _exclusive: MutexGuard<'static, ()>,
    original: Option<Payload>,
}

impl RestoreClipboard {
    async fn capture() -> Self {
        let exclusive = NATIVE_TEST_ACCESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = read_snapshot().await.unwrap().content;
        Self {
            _exclusive: exclusive,
            original,
        }
    }
}

impl Drop for RestoreClipboard {
    fn drop(&mut self) {
        let restore = || -> Result<()> {
            let mut prepared = self
                .original
                .as_ref()
                .map(PreparedPayload::new)
                .transpose()?;
            let owner = ClipboardOwner::new()?;
            let mut cache = CLIPBOARD_ACCESS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.snapshot = None;
            retry_occupied(|| {
                if unsafe { OpenClipboard(owner.0) } == 0 {
                    return Err(ClipboardError::ClipboardOccupied);
                }
                let _open = OpenClipboardGuard;
                if let Some(prepared) = &mut prepared {
                    unsafe { prepared.publish() }?;
                } else if unsafe { EmptyClipboard() } == 0 {
                    return Err(native_error("restore EmptyClipboard"));
                }
                Ok(())
            })
        };
        if let Err(error) = restore() {
            eprintln!("native clipboard test could not restore its original value: {error}");
        }
    }
}

fn clear_cache() {
    CLIPBOARD_ACCESS.lock().unwrap().snapshot = None;
}

fn fixture_image() -> Arc<ClipboardImage> {
    Arc::new(
        ClipboardImage::from_rgba(
            2,
            2,
            vec![255, 0, 0, 255, 0, 255, 0, 128, 9, 8, 7, 0, 0, 0, 255, 255],
        )
        .unwrap(),
    )
}

fn image(snapshot: &Snapshot) -> &Arc<ClipboardImage> {
    match &snapshot.content {
        Some(Payload::Image(image)) => image,
        _ => panic!("expected image snapshot, got {snapshot:?}"),
    }
}

fn text_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn publish_formats(formats: Vec<(u32, Vec<u8>)>) {
    let owner = ClipboardOwner::new().unwrap();
    let mut formats: Vec<_> = formats
        .iter()
        .map(|(format, bytes)| (*format, GlobalMemory::from_bytes(bytes).unwrap()))
        .collect();
    let mut cache = CLIPBOARD_ACCESS.lock().unwrap();
    cache.snapshot = None;
    retry_occupied(|| {
        if unsafe { OpenClipboard(owner.0) } == 0 {
            return Err(ClipboardError::ClipboardOccupied);
        }
        let _open = OpenClipboardGuard;
        if unsafe { EmptyClipboard() } == 0 {
            return Err(native_error("fixture EmptyClipboard"));
        }
        for (format, memory) in &mut formats {
            unsafe { memory.publish(*format) }?;
        }
        Ok(())
    })
    .unwrap();
}

fn native_bytes(format: u32) -> Vec<u8> {
    let _access = CLIPBOARD_ACCESS.lock().unwrap();
    retry_occupied(|| {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            return Err(ClipboardError::ClipboardOccupied);
        }
        let _open = OpenClipboardGuard;
        match unsafe { read_locked_blob(format, MAX_DIB_BYTES) }? {
            Blob::Bytes(bytes) => Ok(bytes),
            Blob::Unsupported(reason) => panic!("unexpected fixture limit: {reason}"),
        }
    })
    .unwrap()
}

#[tokio::test]
async fn native_unicode_roundtrip_and_compare_exchange_preserve_local_copy() {
    let _restore = RestoreClipboard::capture().await;
    let first = "wxclipd Windows 测试 🙂\r\n第二行";
    write_text(first).await.unwrap();
    assert_eq!(read_text().await.unwrap().as_deref(), Some(first));
    assert!(
        write_text_if_unchanged(&Some(first.to_owned()), "远端文本 ✅")
            .await
            .unwrap()
    );
    assert_eq!(read_text().await.unwrap().as_deref(), Some("远端文本 ✅"));
    write_text("用户刚复制的内容").await.unwrap();
    assert!(
        !write_text_if_unchanged(&Some(first.to_owned()), "过期远端值")
            .await
            .unwrap()
    );
    assert_eq!(
        read_text().await.unwrap().as_deref(),
        Some("用户刚复制的内容")
    );
}

#[tokio::test]
async fn native_image_roundtrip_preserves_transparency_and_populates_write_cache() {
    let _restore = RestoreClipboard::capture().await;
    let pixels = fixture_image();
    let expected = read_snapshot().await.unwrap();
    let written = write_if_unchanged(&expected, &Payload::Image(pixels.clone()))
        .await
        .unwrap()
        .unwrap();
    assert!(written.revision.is_some());
    // CloseClipboard may synthesize bitmap formats. That one-time revision
    // change is resolved before returning; subsequent polls must reuse pixels.
    let decodes = IMAGE_DECODES.load(Ordering::Relaxed);
    let cached = read_snapshot().await.unwrap();
    assert_eq!(cached, written);
    assert!(Arc::ptr_eq(image(&cached), &pixels));
    assert_eq!(IMAGE_DECODES.load(Ordering::Relaxed), decodes);

    let png = native_bytes(png_format().unwrap());
    assert_eq!(
        ClipboardImage::from_encoded(&png).unwrap().rgba().unwrap(),
        pixels.rgba().unwrap()
    );
    let dib = native_bytes(CF_DIBV5);
    assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), -2);
    assert_eq!(
        ClipboardImage::from_dib(&dib).unwrap().rgba().unwrap(),
        pixels.rgba().unwrap()
    );
    clear_cache();
    let reread = read_snapshot().await.unwrap();
    assert_eq!(image(&reread).rgba().unwrap(), pixels.rgba().unwrap());
    assert_eq!(reread.content, written.content);
}

#[tokio::test]
async fn native_jpeg_payload_uses_dibv5_without_requiring_png() {
    let _restore = RestoreClipboard::capture().await;
    let mut jpeg = Vec::new();
    ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode(
            &[220, 10, 30, 5, 180, 240],
            2,
            1,
            ::image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    let received = Arc::new(ClipboardImage::from_encoded(&jpeg).unwrap());
    assert!(received.png().is_none());
    let expected = read_snapshot().await.unwrap();
    let written = write_if_unchanged(&expected, &Payload::Image(received.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unsafe { IsClipboardFormatAvailable(png_format().unwrap()) },
        0
    );
    assert_eq!(
        ClipboardImage::from_dib(&native_bytes(CF_DIBV5))
            .unwrap()
            .rgba()
            .unwrap(),
        received.rgba().unwrap()
    );
    clear_cache();
    assert_eq!(read_snapshot().await.unwrap().content, written.content);
}

#[tokio::test]
async fn native_png_dibv5_and_legacy_dib_take_priority_over_text() {
    let _restore = RestoreClipboard::capture().await;
    let pixels = fixture_image();
    publish_formats(vec![
        (png_format().unwrap(), pixels.png().unwrap().to_vec()),
        (
            CF_UNICODETEXT,
            text_bytes("https://example.invalid/image.png"),
        ),
    ]);
    assert_eq!(
        image(&read_snapshot().await.unwrap()).rgba().unwrap(),
        pixels.rgba().unwrap()
    );

    // A malformed registered PNG must not hide a valid native screenshot.
    publish_formats(vec![
        (png_format().unwrap(), vec![0, 1, 2, 3]),
        (CF_DIBV5, dibv5_bytes(&pixels).unwrap()),
        (CF_UNICODETEXT, text_bytes("accompanying browser text")),
    ]);
    assert_eq!(
        image(&read_snapshot().await.unwrap()).rgba().unwrap(),
        pixels.rgba().unwrap()
    );

    // BITMAPINFOHEADER, 24-bit BGR, bottom-up with four-byte row alignment.
    let mut dib = vec![0; 40];
    dib[0..4].copy_from_slice(&40u32.to_le_bytes());
    dib[4..8].copy_from_slice(&2i32.to_le_bytes());
    dib[8..12].copy_from_slice(&2i32.to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&24u16.to_le_bytes());
    dib[20..24].copy_from_slice(&16u32.to_le_bytes());
    dib.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]); // bottom: blue, white
    dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // top: red, green
    publish_formats(vec![
        (CF_DIB, dib),
        (CF_UNICODETEXT, text_bytes("not image data")),
    ]);
    assert_eq!(
        image(&read_snapshot().await.unwrap()).rgba().unwrap(),
        vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
    );
}

#[tokio::test]
async fn native_image_reads_reuse_sequence_cache_instead_of_decoding_again() {
    let _restore = RestoreClipboard::capture().await;
    let pixels = fixture_image();
    publish_formats(vec![(
        png_format().unwrap(),
        pixels.png().unwrap().to_vec(),
    )]);
    let before = IMAGE_DECODES.load(Ordering::Relaxed);
    let first = read_snapshot().await.unwrap();
    for _ in 0..8 {
        let next = read_snapshot().await.unwrap();
        assert_eq!(next.revision, first.revision);
        assert!(Arc::ptr_eq(image(&first), image(&next)));
    }
    assert_eq!(IMAGE_DECODES.load(Ordering::Relaxed), before + 1);
}

#[tokio::test]
async fn native_text_and_image_changes_invalidate_old_revisions() {
    let _restore = RestoreClipboard::capture().await;
    write_text("first text").await.unwrap();
    let text = read_snapshot().await.unwrap();
    let payload = Payload::Image(fixture_image());
    let image_snapshot = write_if_unchanged(&text, &payload).await.unwrap().unwrap();
    assert!(
        write_if_unchanged(&text, &Payload::Text("stale remote text".into()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!write_text_if_unchanged(&None, "stale text-only writer")
        .await
        .unwrap());
    assert_eq!(
        read_snapshot().await.unwrap().content,
        image_snapshot.content
    );

    write_text("new local text").await.unwrap();
    assert!(write_if_unchanged(&image_snapshot, &payload)
        .await
        .unwrap()
        .is_none());
    let before_recopy = read_snapshot().await.unwrap();
    write_text("new local text").await.unwrap();
    let after_recopy = read_snapshot().await.unwrap();
    assert_eq!(before_recopy.content, after_recopy.content);
    assert_ne!(before_recopy.revision, after_recopy.revision);
    assert!(write_if_unchanged(&before_recopy, &payload)
        .await
        .unwrap()
        .is_none());
    assert!(
        write_if_unchanged(&after_recopy, &Payload::Text("invalid\0text".into()))
            .await
            .is_err()
    );
    assert_eq!(read_snapshot().await.unwrap(), after_recopy);
}

#[tokio::test]
async fn native_post_write_refresh_preserves_a_new_local_copys_revision() {
    let _restore = RestoreClipboard::capture().await;
    let expected = read_snapshot().await.unwrap();
    let written = write_if_unchanged(&expected, &Payload::Image(fixture_image()))
        .await
        .unwrap()
        .unwrap();
    // Reproduce a local copy in the gap after CloseClipboard, before the
    // write's final revision/cache refresh. The newer revision must never be
    // attached to the old remote image or used to overwrite the local text.
    write_text("copied immediately after remote image")
        .await
        .unwrap();
    let mut cache = CLIPBOARD_ACCESS.lock().unwrap();
    let refreshed = refresh_written_snapshot(&mut cache, written.clone());
    assert_eq!(refreshed, written);
    let local = cache.snapshot.as_ref().unwrap();
    assert_eq!(
        local.content,
        Some(Payload::Text(
            "copied immediately after remote image".into()
        ))
    );
    assert_ne!(local.revision, written.revision);
}

#[tokio::test]
async fn native_unsupported_image_is_cached_without_blocking_future_receives() {
    let _restore = RestoreClipboard::capture().await;
    publish_formats(vec![
        (png_format().unwrap(), vec![0, 1, 2, 3]),
        (
            CF_UNICODETEXT,
            text_bytes("do not upload this fallback URL"),
        ),
    ]);
    let before = IMAGE_DECODES.load(Ordering::Relaxed);
    let unsupported = read_snapshot().await.unwrap();
    assert!(unsupported.content.is_none());
    assert!(unsupported.revision.is_some());
    assert_eq!(read_snapshot().await.unwrap(), unsupported);
    assert_eq!(IMAGE_DECODES.load(Ordering::Relaxed), before + 1);
    assert!(
        write_if_unchanged(&unsupported, &Payload::Image(fixture_image()))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn native_occupied_clipboard_returns_error_and_retry_preserves_new_local_copy() {
    let _restore = RestoreClipboard::capture().await;
    write_text("before lock").await.unwrap();
    let expected = read_snapshot().await.unwrap();
    clear_cache();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let owner = ClipboardOwner::new().unwrap();
        retry_occupied(|| {
            if unsafe { OpenClipboard(owner.0) } == 0 {
                Err(ClipboardError::ClipboardOccupied)
            } else {
                Ok(())
            }
        })
        .unwrap();
        let _open = OpenClipboardGuard;
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    assert!(read_snapshot().await.is_err());
    // A refresh after a committed write is best effort. Temporary occupancy
    // cannot retroactively report the successful write as an error.
    assert_eq!(
        refresh_written_snapshot(&mut CLIPBOARD_ACCESS.lock().unwrap(), expected.clone()),
        expected
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    let (ready_tx, ready_rx) = mpsc::channel();
    let copier = std::thread::spawn(move || {
        let mut prepared =
            PreparedPayload::new(&Payload::Text("copied while remote write waited".into()))
                .unwrap();
        let owner = ClipboardOwner::new().unwrap();
        retry_occupied(|| {
            if unsafe { OpenClipboard(owner.0) } == 0 {
                Err(ClipboardError::ClipboardOccupied)
            } else {
                Ok(())
            }
        })
        .unwrap();
        let _open = OpenClipboardGuard;
        ready_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        unsafe { prepared.publish() }.unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        write_if_unchanged(&expected, &Payload::Image(fixture_image()))
            .await
            .unwrap()
            .is_none()
    );
    copier.join().unwrap();
    assert_eq!(
        read_snapshot().await.unwrap().content,
        Some(Payload::Text("copied while remote write waited".into()))
    );
}

#[test]
fn clipboard_occupancy_is_retried_but_unavailable_content_is_not() {
    let mut attempts = 0;
    let value = retry_occupied(|| {
        attempts += 1;
        if attempts < 3 {
            Err(ClipboardError::ClipboardOccupied)
        } else {
            Ok("ready")
        }
    })
    .unwrap();
    assert_eq!((value, attempts), ("ready", 3));
    let mut attempts = 0;
    let error = retry_occupied::<()>(|| {
        attempts += 1;
        Err(ClipboardError::ContentNotAvailable)
    });
    assert!(error.is_err());
    assert_eq!(attempts, 1);
    let mut attempts = 0;
    assert!(retry_occupied::<()>(|| {
        attempts += 1;
        Err(ClipboardError::ClipboardOccupied)
    })
    .is_err());
    assert_eq!(attempts, 6);
}
