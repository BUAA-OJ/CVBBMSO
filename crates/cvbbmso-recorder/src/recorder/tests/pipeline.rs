use std::{
    fs, io,
    ops::ControlFlow,
    sync::{Arc, atomic::Ordering, mpsc},
    task::{Context, Waker},
    time::Duration,
};

use cvbbmso_protocol::{RecordingPolicy, SegmentMeta};
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::{OJ_USER_ID, PICTURE, SEQUENCE, candidate_dir, exam_id};
use crate::{
    Recorder, RecorderEvent, RecordingError,
    capture::{Frame, SampledFrame},
    container::{AvcConfig, MkvFile, Sample},
    encode::Nv12Frame,
    recorder::worker::VideoEncoder,
};

enum Input {
    Frame(SampledFrame),
    Fence(oneshot::Sender<()>),
    Fault,
    Panic(oneshot::Sender<()>),
}

fn drive<E: VideoEncoder>(recorder: &mut Recorder) -> mpsc::Sender<Input> {
    let (send, receive) = mpsc::channel();
    recorder.driver = Some(Box::new(move |job, stop, ready| {
        job.run::<E>(stop, ready, |policy, stop, started, sink| {
            assert_eq!(policy, RecordingPolicy::new(1, 10).expect("policy"));
            started();
            for input in receive {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                match input {
                    Input::Frame(frame) => {
                        if sink(frame)? == ControlFlow::Break(()) {
                            break;
                        }
                    }
                    Input::Fence(ready) => ready.send(()).expect("fence receiver"),
                    Input::Fault => return Err(platform_fault()),
                    Input::Panic(ready) => {
                        ready.send(()).expect("announce interruption");
                        panic!("injected capture worker panic");
                    }
                }
            }
            Ok(())
        })
    }));
    send
}

fn platform_fault() -> RecordingError {
    RecordingError::Platform(Some(io::Error::other("test screen source failed")))
}

fn push_frame(send: &mpsc::Sender<Input>, width: u32, height: u32, millis: u64) {
    let level = u8::try_from((millis / 100) % 180 + 50).expect("intensity");
    send.send(Input::Frame(SampledFrame {
        elapsed: Duration::from_millis(millis),
        frame: Arc::new(Frame {
            width,
            height,
            pixels: vec![level; width as usize * height as usize * 4],
        }),
    }))
    .expect("submit synthetic frame");
}

async fn fence(send: &mpsc::Sender<Input>) {
    let (done, ready) = oneshot::channel();
    send.send(Input::Fence(done)).expect("fence");
    ready.await.expect("samples consumed");
}

async fn open(directory: &TempDir) -> (Recorder, tokio::sync::mpsc::Receiver<RecorderEvent>) {
    Recorder::open(
        exam_id(),
        OJ_USER_ID,
        candidate_dir(directory, OJ_USER_ID),
        RecordingPolicy::new(1, 10).expect("policy"),
    )
    .await
    .expect("open recorder")
}

async fn segment_event(
    events: &mut tokio::sync::mpsc::Receiver<RecorderEvent>,
    index: u32,
) -> SegmentMeta {
    let event = events.recv().await;
    let Some(RecorderEvent::SegmentFinalized(meta)) = event else {
        panic!("expected segment {index}, received {event:?}");
    };
    assert_eq!(meta.index, index);
    meta
}

struct DelayedEncoder {
    keyframe: bool,
    pending: Option<Sample>,
}

impl VideoEncoder for DelayedEncoder {
    fn open(_width: u32, _height: u32, fps: u32) -> Result<Self, RecordingError> {
        assert_eq!(fps, 10);
        Ok(Self {
            keyframe: false,
            pending: None,
        })
    }

    fn force_keyframe(&mut self) -> Result<(), RecordingError> {
        self.keyframe = true;
        Ok(())
    }

    fn encode(&mut self, _frame: &Nv12Frame, at: Duration) -> Result<Vec<Sample>, RecordingError> {
        let keyframe = std::mem::take(&mut self.keyframe);
        let sample = Sample {
            timestamp_ms: u64::try_from(at.as_millis()).expect("timestamp"),
            keyframe,
            data: vec![0, 0, 0, 3, if keyframe { 0x65 } else { 0x41 }, 0x88, 0x84],
        };
        Ok(self.pending.replace(sample).into_iter().collect())
    }

    fn flush(&mut self) -> Result<Vec<Sample>, RecordingError> {
        Ok(self.pending.take().into_iter().collect())
    }

    fn config(&self) -> Result<AvcConfig, RecordingError> {
        Ok(AvcConfig::new(SEQUENCE.to_vec(), PICTURE.to_vec()).expect("valid AVC configuration"))
    }
}

#[tokio::test]
async fn start_waits_for_the_backend_and_can_retry_a_setup_failure() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, _) = open(&directory).await;
    recorder.driver = Some(Box::new(|job, stop, ready| {
        job.run::<DelayedEncoder>(stop, ready, |_, _, _, _| Err(platform_fault()))
    }));
    assert!(matches!(
        recorder.start().await,
        Err(RecordingError::Platform(Some(_)))
    ));

    let input = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("retry");
    recorder.start().await.expect("idempotent start");
    drop(input);
    recorder.stop().await.expect("stop");
}

#[tokio::test]
async fn rotates_stops_recovers_and_finishes_with_full_event_channels() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("start");
    for segment in 0..19 {
        push_frame(&source, 15, 15, segment * 1000);
        push_frame(&source, 15, 15, segment * 1000 + 500);
    }
    fence(&source).await;
    assert_eq!(events.len(), events.max_capacity());
    {
        let mut stopping = std::pin::pin!(recorder.stop());
        assert!(
            stopping
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(source);
        stopping.await.expect("stop under backpressure");
    }
    let mut expected = Vec::new();
    for index in 1..=19 {
        expected.push(segment_event(&mut events, index).await);
    }
    drop(recorder);
    drop(events);

    let (mut recorder, mut events) = open(&directory).await;
    for expected in &expected {
        let RecorderEvent::SegmentFinalized(meta) = events.try_recv().expect("recovered event")
        else {
            panic!("expected recovered segment");
        };
        assert_eq!(&meta, expected);
    }
    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("restart");
    for segment in 0..38 {
        push_frame(&source, 15, 15, segment * 1000);
        push_frame(&source, 15, 15, segment * 1000 + 500);
    }
    fence(&source).await;
    assert_eq!(events.len(), events.max_capacity());
    let manifest = {
        let mut finishing = std::pin::pin!(recorder.finish());
        assert!(
            finishing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(source);
        finishing.await.expect("finish under backpressure")
    };
    assert_eq!(manifest.segments.len(), 57);
    assert_eq!(manifest.segments[..19], expected);
    for index in 20..=57 {
        assert_eq!(
            segment_event(&mut events, index).await,
            manifest.segments[(index - 1) as usize]
        );
    }
    for index in 1..=57 {
        let bytes = fs::read(recorder.store.video_path(index)).expect("published MKV");
        let file = MkvFile::parse(&bytes).expect("readable MKV");
        assert_eq!((file.width, file.height), (16, 16));
        assert_eq!(file.samples.len(), 2);
        assert!(file.samples[0].is_idr().expect("IDR"));
        assert_eq!(file.samples[1].timestamp_ms, 500);
    }
    assert_eq!(recorder.finish().await.expect("cached manifest"), manifest);
}

#[tokio::test]
async fn permits_recovery_after_an_async_fault_without_publishing_its_open_segment() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("start");
    push_frame(&source, 16, 16, 0);
    push_frame(&source, 16, 16, 1000);
    segment_event(&mut events, 1).await;
    source.send(Input::Fault).expect("inject fault");
    assert!(matches!(
        events.recv().await,
        Some(RecorderEvent::Fault(RecordingError::Platform(_)))
    ));
    assert!(!recorder.store.video_path(2).exists());

    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("retry after async fault");
    push_frame(&source, 16, 16, 0);
    fence(&source).await;
    drop(source);
    recorder.stop().await.expect("stop");
    segment_event(&mut events, 2).await;
    let manifest = recorder.finish().await.expect("finish");
    assert_eq!(manifest.segments.len(), 2);
}

#[tokio::test]
async fn revisits_disk_before_finishing_after_a_worker_panics() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("start");
    push_frame(&source, 16, 16, 0);
    push_frame(&source, 16, 16, 1000);
    let sealed = segment_event(&mut events, 1).await;
    let (ready, interrupted) = oneshot::channel();
    source.send(Input::Panic(ready)).expect("interrupt worker");
    interrupted.await.expect("worker interruption");
    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::Io(_))
    ));

    let manifest = recorder
        .finish()
        .await
        .expect("finish from recovered evidence");
    assert_eq!(manifest.segments, vec![sealed]);
}

#[cfg(any(windows, target_os = "macos"))]
#[tokio::test]
#[ignore = "requires the platform H.264 encoder; uses synthetic frames only"]
async fn records_synthetic_frames_across_rotation_with_the_native_encoder() {
    use crate::recorder::worker::NativeEncoder;

    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    let source = drive::<NativeEncoder>(&mut recorder);
    recorder.start().await.expect("start");
    for index in 0..20 {
        push_frame(&source, 319, 239, index * 100);
    }
    fence(&source).await;
    drop(source);
    recorder.stop().await.expect("stop");
    let manifest = recorder.finish().await.expect("merge");
    assert_eq!(manifest.segments.len(), 2);
    for index in 1..=2 {
        segment_event(&mut events, index).await;
        let bytes = fs::read(recorder.store.video_path(index)).expect("published MKV");
        let file = MkvFile::parse(&bytes).expect("readable MKV");
        assert_eq!((file.width, file.height), (320, 240));
        assert_eq!(file.samples.len(), 10);
        assert!(file.samples[0].is_idr().expect("IDR"));
    }
    if let Some(path) = std::env::var_os("CVBBMSO_RECORDER_TEST_MP4") {
        let mut source = fs::File::open(recorder.store.final_path()).expect("open final movie");
        let mut output = fs::File::create_new(path).expect("create the acceptance artifact");
        io::copy(&mut source, &mut output).expect("export for independent decoding");
    }
}
