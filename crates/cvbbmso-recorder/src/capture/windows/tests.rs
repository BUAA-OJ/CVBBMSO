use std::{
    error::Error,
    fs,
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use cvbbmso_protocol::RecordingPolicy;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST;

use super::{DXGI_OUTDUPL_FRAME_INFO, WindowsError, desktop_update, failure};
use crate::capture::{Frame, run};

#[test]
fn pointer_only_acquisition_does_not_read_an_uninitialized_desktop_image() {
    let info = DXGI_OUTDUPL_FRAME_INFO {
        LastMouseUpdateTime: 1,
        ..DXGI_OUTDUPL_FRAME_INFO::default()
    };
    let frame = desktop_update(&info, || panic!("pointer-only surface must not be copied"))
        .expect("no desktop update");
    assert!(frame.is_none());
}

#[test]
fn presented_desktop_image_is_delivered() {
    let info = DXGI_OUTDUPL_FRAME_INFO {
        LastPresentTime: 1,
        ..DXGI_OUTDUPL_FRAME_INFO::default()
    };
    let frame = desktop_update(&info, || {
        Ok(Frame {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 255],
        })
    })
    .expect("read image")
    .expect("desktop image");
    assert_eq!(frame.pixels, [1, 2, 3, 255]);
}

#[test]
fn platform_failure_preserves_the_original_windows_error() {
    let original = WindowsError::new(DXGI_ERROR_ACCESS_LOST, "capture device context");
    let message = original.to_string();
    let error = failure("reading desktop frame", original);
    let storage = error
        .source()
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("platform source");
    let windows = storage
        .get_ref()
        .and_then(std::error::Error::source)
        .and_then(|cause| cause.downcast_ref::<WindowsError>())
        .expect("original Windows error, not a string replacement");

    assert_eq!(windows.code(), DXGI_ERROR_ACCESS_LOST);
    assert_eq!(windows.to_string(), message);
    assert!(storage.to_string().contains("reading desktop frame"));
    assert!(storage.to_string().contains("0x887A0026"));
}

/// Opt-in hardware acceptance: writes three BGRA images and their presentation
/// times below the workspace's ignored target directory. Desktop and COM are
/// created and destroyed on the capture thread, including failure/stop paths.
#[test]
#[ignore = "requires an interactive primary display; writes raw capture artifacts"]
fn writes_sampled_frames_from_the_primary_display() {
    const FPS: u32 = 5;
    const FRAMES: usize = 3;
    let artifacts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/recorder-capture-check");
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
    // unit tests use a fake clock and never sleep or depend on desktop updates.
    let completed = completion.recv_timeout(Duration::from_secs(5));
    stopped.store(true, Ordering::Release);
    let frames = worker.join().expect("capture worker").expect("capture");
    assert!(
        completed.is_ok(),
        "capture did not complete within five seconds"
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
