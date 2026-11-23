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

use super::{
    OJ_USER_ID, PICTURE, SEQUENCE, candidate_dir, exam_id, open_with_video_segments,
    wait_for_merge_completion,
};
use crate::{
    Recorder, RecorderEvent, RecordingError,
    capture::{Frame, SampledFrame},
    container::{AvcConfig, MkvFile, Sample},
    encode::Nv12Frame,
    recorder::{State, worker::VideoEncoder},
};

enum Input {
    Frame(SampledFrame),
    Fence(oneshot::Sender<()>),
    Fault,
    EncoderFault,
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
                    Input::EncoderFault => return Err(RecordingError::Encoder(None)),
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
async fn stopping_before_readiness_is_not_a_successful_start() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, _) = open(&directory).await;
    recorder.driver = Some(Box::new(|job, stop, ready| {
        job.run::<DelayedEncoder>(stop, ready, |_, _, _, _| Ok(()))
    }));
    assert!(matches!(recorder.start().await,
        Err(RecordingError::Platform(Some(error))) if error.kind() == io::ErrorKind::Interrupted));
}

#[tokio::test]
async fn finishing_waits_for_all_segment_events_under_backpressure() {
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
    assert_pending(recorder.finish());
    drop(source);
    let State::Capturing(capture) = &recorder.state else {
        panic!("finish should retain the capture worker");
    };
    while !capture.task.is_finished() {
        tokio::task::yield_now().await;
    }
    assert_pending(recorder.finish());
    if matches!(recorder.state, State::Finalizing(_)) {
        wait_for_merge_completion(&recorder).await;
    }
    // Even a successful merge cannot complete finish while the tail is still
    // waiting for capacity. Cancelling this wait must retain that merge.
    assert_pending(recorder.finish());
    assert!(matches!(recorder.state, State::Complete(_)));

    let mut received = Vec::new();
    let summary = {
        let mut finishing = std::pin::pin!(recorder.finish());
        loop {
            tokio::select! {
                biased;
                result = &mut finishing => break result.expect("finish under backpressure"),
                event = events.recv() => {
                    let Some(RecorderEvent::SegmentFinalized(meta)) = event else {
                        panic!("expected segment while finishing");
                    };
                    received.push(meta);
                }
            }
        }
    };
    while let Ok(event) = events.try_recv() {
        let RecorderEvent::SegmentFinalized(meta) = event else {
            panic!("expected segment after finishing");
        };
        received.push(meta);
    }
    assert_eq!(summary.segment_count.get(), 57);
    expected.extend(received);
    assert_eq!(expected, recorder.store.segments());
    for index in 1..=57 {
        let bytes = fs::read(recorder.store.video_path(index)).expect("published MKV");
        let file = MkvFile::parse(&bytes).expect("readable MKV");
        assert_eq!((file.width, file.height), (16, 16));
        assert_eq!(file.samples.len(), 2);
        assert!(file.samples[0].is_idr().expect("IDR"));
        assert_eq!(file.samples[1].timestamp_ms, 500);
    }
    assert_eq!(recorder.finish().await.expect("cached summary"), summary);
    assert!(events.try_recv().is_err());
}

fn assert_pending(future: impl Future) {
    let mut future = std::pin::pin!(future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
}

#[tokio::test]
async fn seals_captured_frames_before_reporting_a_platform_fault_and_recovers() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("start");
    push_frame(&source, 16, 16, 0);
    push_frame(&source, 16, 16, 1000);
    segment_event(&mut events, 1).await;
    source.send(Input::Fault).expect("inject fault");
    segment_event(&mut events, 2).await;
    assert!(matches!(
        events.recv().await,
        Some(RecorderEvent::Fault(RecordingError::Platform(_)))
    ));
    let saved = fs::read(recorder.store.video_path(2)).expect("pre-fault segment retained");
    assert_eq!(
        MkvFile::parse(&saved).expect("readable tail").samples.len(),
        1
    );

    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("retry after async fault");
    push_frame(&source, 16, 16, 0);
    fence(&source).await;
    drop(source);
    let summary = recorder.finish().await.expect("finish");
    assert_eq!(summary.segment_count.get(), 3);
    let RecorderEvent::SegmentFinalized(tail) = events.try_recv().expect("tail already delivered")
    else {
        panic!("unexpected tail event");
    };
    assert_eq!(tail.index, 3);
}

#[tokio::test]
async fn finish_delivers_a_segment_sealed_before_an_interrupted_send() {
    let (mut recorder, mut events, _directory) = open_with_video_segments(1).await;
    recorder.driver = Some(Box::new(|mut job, _, ready| {
        ready.send(()).expect("ready");
        let bytes = fs::read(job.store.video_path(1)).expect("fixture MKV");
        let at = time::OffsetDateTime::UNIX_EPOCH;
        job.store
            .write_segment(&bytes, at, at)
            .expect("seal before interruption");
        panic!("worker lost before publishing the tail event");
    }));
    recorder.start().await.expect("start");
    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::Io(_))
    ));

    let summary = recorder
        .finish()
        .await
        .expect("finish from recovered evidence");
    assert_eq!(summary.segment_count.get(), 2);
    for index in [1, 2] {
        let RecorderEvent::SegmentFinalized(meta) =
            events.try_recv().expect("recovered segments ready")
        else {
            panic!("unexpected recovery event");
        };
        assert_eq!(meta.index, index);
    }
    assert_eq!(recorder.finish().await.expect("cached summary"), summary);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn repeated_display_faults_keep_every_captured_tail_in_sequence() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    for index in 1..=3 {
        let source = drive::<DelayedEncoder>(&mut recorder);
        recorder.start().await.expect("start or recover");
        push_frame(&source, 16, 16, 0);
        source.send(Input::Fault).expect("display lost");
        segment_event(&mut events, index).await;
        assert!(matches!(
            events.recv().await,
            Some(RecorderEvent::Fault(RecordingError::Platform(_)))
        ));
    }
    assert_eq!(
        recorder
            .finish()
            .await
            .expect("merge all tails")
            .segment_count
            .get(),
        3
    );
}

#[tokio::test]
async fn encoder_failure_does_not_publish_its_suspect_tail() {
    let directory = TempDir::new().expect("directory");
    let (mut recorder, mut events) = open(&directory).await;
    let source = drive::<DelayedEncoder>(&mut recorder);
    recorder.start().await.expect("start");
    push_frame(&source, 16, 16, 0);
    source.send(Input::EncoderFault).expect("encoder fault");
    assert!(matches!(
        events.recv().await,
        Some(RecorderEvent::Fault(RecordingError::Encoder(_)))
    ));
    assert!(!recorder.store.video_path(1).exists());
    assert!(matches!(
        recorder.finish().await,
        Err(RecordingError::NoSegments)
    ));
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
    let summary = recorder.finish().await.expect("merge");
    assert_eq!(summary.segment_count.get(), 2);
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
