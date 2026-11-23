//! Core Video regression and `ScreenCaptureKit` hardware acceptance tests.

use std::{
    fs,
    ops::ControlFlow,
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use cvbbmso_protocol::RecordingPolicy;
use objc2::{rc::Weak, runtime::NSObject};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBufferCreate, CVPixelBufferCreateWithBytes, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferExtendedPixelsLeftKey,
    kCVPixelBufferExtendedPixelsTopKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelFormatType_32ARGB, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

use super::{packed_bgra_rows, pixel_buffer_frame, wait_for_display};
use crate::capture::run;

#[test]
fn copies_only_visible_bytes_from_a_packed_bgra_buffer() {
    // Two rows of 8 visible bytes followed by 4 bytes of padding.
    let mut pixels = [
        1_u8, 2, 3, 4, 5, 6, 7, 8, 255, 255, 255, 255, //
        9, 10, 11, 12, 13, 14, 15, 16, 255, 255, 255, 255,
    ];
    for format in [kCVPixelFormatType_32BGRA, kCVPixelFormatType_32ARGB] {
        let mut buffer = ptr::null_mut();
        // SAFETY: pixels backs both rows and outlives the created pixel buffer.
        // The out pointer is valid for the duration of the call.
        let status = unsafe {
            CVPixelBufferCreateWithBytes(
                None,
                2,
                2,
                format,
                NonNull::new(pixels.as_mut_ptr())
                    .expect("pixel data")
                    .cast(),
                12,
                None,
                ptr::null_mut(),
                None,
                NonNull::from(&mut buffer),
            )
        };
        assert_eq!(status, 0);
        // SAFETY: Core Video returns one owned reference on success.
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(buffer).expect("pixel buffer")) };
        let frame = pixel_buffer_frame(&buffer);
        if format == kCVPixelFormatType_32BGRA {
            let frame = frame.expect("packed BGRA frame");
            assert_eq!((frame.width, frame.height), (2, 2));
            assert_eq!(frame.pixels, (1..=16).collect::<Vec<_>>());
        } else {
            assert!(frame.is_none(), "ARGB input was treated as BGRA");
        }
    }
}

#[test]
fn copies_an_iosurface_with_extended_pixels() {
    const WIDTH: usize = 24;
    const HEIGHT: usize = 24;
    let padding = CFNumber::new_i32(6);
    let surface = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: these keys are Core Video constants with static lifetimes.
    let keys = unsafe {
        [
            kCVPixelBufferExtendedPixelsLeftKey,
            kCVPixelBufferExtendedPixelsTopKey,
            kCVPixelBufferIOSurfacePropertiesKey,
        ]
    };
    let values: [&CFType; 3] = [&padding, &padding, &surface];
    let attributes = CFDictionary::from_slices(&keys, &values);
    let mut buffer = ptr::null_mut();
    // SAFETY: the dictionary and the out pointer are valid for the call.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            WIDTH,
            HEIGHT,
            kCVPixelFormatType_32BGRA,
            Some(attributes.as_opaque()),
            NonNull::from(&mut buffer),
        )
    };
    assert_eq!(status, 0);
    // SAFETY: Core Video returns one owned reference on success.
    let buffer = unsafe { CFRetained::from_raw(NonNull::new(buffer).expect("pixel buffer")) };
    let flags = CVPixelBufferLockFlags(0);
    // SAFETY: a successful lock here permits writing the new pixel buffer.
    assert_eq!(unsafe { CVPixelBufferLockBaseAddress(&buffer, flags) }, 0);
    let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
    assert!(!base.is_null());
    let pitch = CVPixelBufferGetBytesPerRow(&buffer);
    for row in 0..HEIGHT {
        // SAFETY: the locked BGRA buffer was allocated for these dimensions.
        // Initialize its visible bytes without touching any extended pixels.
        unsafe { base.add(row * pitch).write_bytes(42, WIDTH * 4) };
    }
    // SAFETY: balances the successful lock above with identical flags.
    assert_eq!(unsafe { CVPixelBufferUnlockBaseAddress(&buffer, flags) }, 0);

    let frame = pixel_buffer_frame(&buffer).expect("extended pixel buffer");
    assert_eq!((frame.width, frame.height), (24, 24));
    assert_eq!(frame.pixels, [42; WIDTH * HEIGHT * 4]);
}

#[test]
fn rejects_a_planar_pixel_buffer() {
    let mut buffer = ptr::null_mut();
    // SAFETY: the out pointer is valid for the duration of the call.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            16,
            16,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            None,
            NonNull::from(&mut buffer),
        )
    };
    assert_eq!(status, 0);
    // SAFETY: Core Video returns one owned reference on success.
    let buffer = unsafe { CFRetained::from_raw(NonNull::new(buffer).expect("pixel buffer")) };
    assert!(pixel_buffer_frame(&buffer).is_none());
}

#[test]
fn checks_base_stride_and_full_visible_range_before_slicing() {
    let mut storage = [0_u8; 20];
    let base = storage.as_mut_ptr();
    assert_eq!(
        packed_bgra_rows(base, 2, 2, 12, 20).map(|row| row.1),
        Some(8)
    );
    assert!(packed_bgra_rows(ptr::null_mut(), 2, 2, 12, 20).is_none());
    assert!(packed_bgra_rows(base, 0, 2, 12, 20).is_none());
    assert!(packed_bgra_rows(base, 2, 0, 12, 20).is_none());
    assert!(packed_bgra_rows(base, 2, 2, 7, 20).is_none());
    assert!(packed_bgra_rows(base, 2, 2, 12, 19).is_none());
    assert!(packed_bgra_rows(base, usize::MAX, 1, usize::MAX, usize::MAX).is_none());
    assert!(packed_bgra_rows(base, 1, 3, usize::MAX, usize::MAX).is_none());
    assert!(packed_bgra_rows(base, 1, 2, isize::MAX as usize, usize::MAX).is_none());
}

#[test]
fn a_callback_after_timeout_releases_its_owned_object() {
    let (send, receive) = mpsc::channel();
    let display = NSObject::new();
    let weak = Weak::new(&*display);
    let complete = |display| {
        let _ = send.send(Ok(display));
    };

    let error = wait_for_display(receive, Duration::ZERO).expect_err("query timed out");
    assert!(
        error
            .to_string()
            .ends_with("the shareable content query timed out")
    );
    complete(display);

    assert!(weak.load().is_none());
}

#[test]
fn dropping_the_receiver_releases_a_queued_owned_object() {
    let (send, receive) = mpsc::channel::<Result<_, String>>();
    let display = NSObject::new();
    let weak = Weak::new(&*display);
    send.send(Ok(display)).expect("queue the result");
    assert!(weak.load().is_some());
    drop(receive);
    assert!(weak.load().is_none());
}

/// Opt-in hardware acceptance: writes three BGRA images and their presentation
/// times below the workspace's ignored target directory. The stream is created
/// and destroyed on the capture thread, including failure and stop paths.
#[test]
#[ignore = "requires screen-recording permission and a display; writes raw capture artifacts"]
fn writes_sampled_frames_from_the_primary_display() {
    const FPS: u32 = 5;
    const FRAMES: usize = 3;
    let artifacts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/recorder-capture-check-macos");
    fs::create_dir_all(&artifacts).expect("capture artifact directory");
    let directory = tempfile::Builder::new()
        .prefix("capture-")
        .tempdir_in(&artifacts)
        .expect("capture run directory");
    let output = directory.path().to_path_buf();
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stopped);
    let (done, completion) = mpsc::channel();

    let worker = thread::spawn(move || {
        let mut frames = Vec::new();
        let mut previous_slot = None;
        let result = run(
            RecordingPolicy::new(30, FPS).expect("policy"),
            &worker_stop,
            |sample| {
                let image = sample.frame;
                assert!(image.width > 0 && image.height > 0, "empty frame");
                assert_eq!(
                    u64::try_from(image.pixels.len()).expect("pixel length"),
                    u64::from(image.width) * u64::from(image.height) * 4
                );
                let slot = sample.elapsed.as_nanos() * u128::from(FPS) / 1_000_000_000;
                assert!(previous_slot.is_none_or(|previous| slot > previous));
                previous_slot = Some(slot);

                let name = format!("{:06}.bgra", frames.len() + 1);
                fs::write(output.join(&name), &image.pixels)?;
                frames.push(serde_json::json!({
                    "file": name,
                    "width": image.width,
                    "height": image.height,
                    "elapsed_ns": sample.elapsed.as_nanos(),
                }));
                Ok(if frames.len() == FRAMES {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            },
        );
        let _ = done.send(());
        result.map(|()| frames)
    });

    // This real-time deadline belongs only to the opt-in hardware check. Normal
    // unit tests use a fake clock and never sleep or depend on display updates.
    let completed = completion.recv_timeout(Duration::from_secs(10));
    stopped.store(true, Ordering::Release);
    let frames = worker.join().expect("capture worker").expect("capture");
    assert!(
        completed.is_ok(),
        "capture did not complete within ten seconds"
    );
    assert_eq!(frames.len(), FRAMES);
    let manifest = serde_json::json!({
        "pixel_format": "bgra8",
        "fps": FPS,
        "frames": frames,
    });
    fs::write(
        directory.path().join("frames.json"),
        serde_json::to_vec_pretty(&manifest).expect("raw capture metadata"),
    )
    .expect("write capture metadata");
    let _ = directory.keep();
}
