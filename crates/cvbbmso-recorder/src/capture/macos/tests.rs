//! `ScreenCaptureKit` hardware acceptance tests.

use std::{
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

use crate::capture::run;

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
