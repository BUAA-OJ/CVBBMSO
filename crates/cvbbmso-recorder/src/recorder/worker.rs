//! Thread-confined capture, encoding and segment rotation.

use std::{
    io,
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cvbbmso_protocol::RecordingPolicy;
use time::OffsetDateTime;
use tokio::sync::oneshot;

use crate::{
    RecorderEvent, RecordingError,
    capture::{self, Frame, SampledFrame},
    container::{AvcConfig, MkvWriter, Sample},
    encode::Nv12Frame,
    store::{MAX_SEGMENT_BYTES, SegmentStore},
};

use super::events::Events;

pub(super) struct Job {
    pub(super) store: SegmentStore,
    pub(super) policy: RecordingPolicy,
    pub(super) events: Arc<Events>,
}

pub(super) struct Outcome {
    pub(super) store: SegmentStore,
    pub(super) result: Result<(), RecordingError>,
    pub(super) faulted: bool,
}

type FrameSink<'a> = dyn FnMut(SampledFrame) -> Result<ControlFlow<()>, RecordingError> + 'a;

impl Job {
    pub(super) fn platform(self, stop: &AtomicBool, ready: oneshot::Sender<()>) -> Outcome {
        self.run::<NativeEncoder>(stop, ready, |policy, stop, started, sink| {
            capture::run_started(policy, stop, started, sink)
        })
    }

    pub(super) fn run<E: VideoEncoder>(
        mut self,
        stop: &AtomicBool,
        ready: oneshot::Sender<()>,
        capture: impl FnOnce(
            RecordingPolicy,
            &AtomicBool,
            &mut dyn FnMut(),
            &mut FrameSink<'_>,
        ) -> Result<(), RecordingError>,
    ) -> Outcome {
        let mut started = false;
        let mut ready = Some(ready);
        let result = self.store.existing_video_layout().and_then(|layout| {
            let mut recording = Recording::<E> {
                store: &mut self.store,
                events: &self.events,
                policy: self.policy,
                layout,
                input: None,
                segment: None,
            };
            let captured = capture(
                self.policy,
                stop,
                &mut || {
                    if let Some(ready) = ready.take() {
                        started = true;
                        let _ = ready.send(());
                    }
                },
                &mut |frame| recording.sample(&frame),
            );
            // The runtime receives mid-session faults through its event stream.
            // Errors during startup or the requested final drain are returned
            // to the caller, which is already waiting for their outcome.
            if let Err(error) = captured {
                if started {
                    stop.store(true, Ordering::Release);
                    recording.events.send(RecorderEvent::Fault(error));
                    return Ok(true);
                }
                return Err(error);
            }
            recording.seal_segment()?;
            Ok(false)
        });
        let faulted = !matches!(result, Ok(false));
        Outcome {
            store: self.store,
            result: result.map(|_| ()),
            faulted,
        }
    }
}

// Both platform adapters have this interface. Tests can drive the same roll
// and seal logic with an encoder whose asynchronous output is deterministic.
pub(super) trait VideoEncoder: Sized {
    fn open(width: u32, height: u32, fps: u32) -> Result<Self, RecordingError>;
    fn force_keyframe(&mut self) -> Result<(), RecordingError>;
    fn encode(&mut self, frame: &Nv12Frame, at: Duration) -> Result<Vec<Sample>, RecordingError>;
    fn flush(&mut self) -> Result<Vec<Sample>, RecordingError>;
    fn config(&self) -> Result<AvcConfig, RecordingError>;
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) type NativeEncoder = crate::encode::Encoder;

#[cfg(any(windows, target_os = "macos"))]
impl VideoEncoder for NativeEncoder {
    fn open(width: u32, height: u32, fps: u32) -> Result<Self, RecordingError> {
        Self::open(width, height, fps, crate::encode::DEFAULT_BITRATE)
    }

    fn force_keyframe(&mut self) -> Result<(), RecordingError> {
        self.force_keyframe()
    }

    fn encode(&mut self, frame: &Nv12Frame, at: Duration) -> Result<Vec<Sample>, RecordingError> {
        self.encode(frame, at)
    }

    fn flush(&mut self) -> Result<Vec<Sample>, RecordingError> {
        self.flush()
    }

    fn config(&self) -> Result<AvcConfig, RecordingError> {
        self.config()
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
enum NativeEncoder {}

#[cfg(not(any(windows, target_os = "macos")))]
impl VideoEncoder for NativeEncoder {
    fn open(_width: u32, _height: u32, _fps: u32) -> Result<Self, RecordingError> {
        Err(RecordingError::Encoder(None))
    }

    fn force_keyframe(&mut self) -> Result<(), RecordingError> {
        match *self {}
    }

    fn encode(&mut self, _frame: &Nv12Frame, _at: Duration) -> Result<Vec<Sample>, RecordingError> {
        match *self {}
    }

    fn flush(&mut self) -> Result<Vec<Sample>, RecordingError> {
        match *self {}
    }

    fn config(&self) -> Result<AvcConfig, RecordingError> {
        match *self {}
    }
}

type VideoLayout = (u32, u32, AvcConfig);

struct Recording<'a, E> {
    store: &'a mut SegmentStore,
    events: &'a Arc<Events>,
    policy: RecordingPolicy,
    layout: Option<VideoLayout>,
    input: Option<VideoInput>,
    segment: Option<Segment<E>>,
}

impl<E: VideoEncoder> Recording<'_, E> {
    fn sample(&mut self, sample: &SampledFrame) -> Result<ControlFlow<()>, RecordingError> {
        let sampled_at = OffsetDateTime::now_utc();
        let duration = Duration::from_secs(u64::from(self.policy.segment_duration_secs));
        if self
            .segment
            .as_ref()
            .is_some_and(|segment| sample.elapsed.saturating_sub(segment.origin) >= duration)
        {
            self.seal_segment()?;
        }

        let frame = &sample.frame;
        if self.input.is_none() {
            self.input = Some(VideoInput::new(frame)?);
        }
        let Some(input) = self.input.as_mut() else {
            return Err(encode_error("missing encoder input"));
        };
        let (width, height) = input.dimensions;
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| (layout.0, layout.1) != input.dimensions)
        {
            return Err(encode_error(
                "display dimensions disagree with the sealed recording",
            ));
        }
        if self.segment.is_none() {
            let mut encoder = E::open(width, height, self.policy.fps)?;
            encoder.force_keyframe()?;
            self.segment = Some(Segment {
                encoder,
                dimensions: (width, height),
                origin: sample.elapsed,
                start_at: sampled_at,
                end_at: sampled_at,
                writer: None,
                buffered: 0,
            });
        }
        let Some(segment) = &mut self.segment else {
            return Err(encode_error("missing active segment"));
        };
        input.convert(frame)?;
        let relative = sample.elapsed.saturating_sub(segment.origin);
        let samples = segment.encoder.encode(&input.nv12, relative)?;
        segment.push(samples, &mut self.layout)?;
        segment.end_at = segment.end_at.max(sampled_at);
        Ok(ControlFlow::Continue(()))
    }

    fn seal_segment(&mut self) -> Result<(), RecordingError> {
        if let Some(mut segment) = self.segment.take() {
            let drained = segment.encoder.flush()?;
            segment.push(drained, &mut self.layout)?;
            let writer = segment
                .writer
                .ok_or_else(|| encode_error("encoder produced no samples"))?;
            let bytes = writer.finish().map_err(container_error)?;
            let meta = self
                .store
                .write_segment(&bytes, segment.start_at, segment.end_at)?;
            self.events.send(RecorderEvent::SegmentFinalized(meta));
        }
        Ok(())
    }
}

struct Segment<E> {
    encoder: E,
    dimensions: (u32, u32),
    origin: Duration,
    start_at: OffsetDateTime,
    end_at: OffsetDateTime,
    writer: Option<MkvWriter>,
    buffered: u64,
}

impl<E: VideoEncoder> Segment<E> {
    fn push(
        &mut self,
        samples: Vec<Sample>,
        layout: &mut Option<VideoLayout>,
    ) -> Result<(), RecordingError> {
        for sample in samples {
            let (width, height) = self.dimensions;
            let writer = match &mut self.writer {
                Some(writer) => writer,
                slot => {
                    if !sample.is_idr().map_err(container_error)? {
                        return Err(encode_error("segment does not begin with an IDR frame"));
                    }
                    let config = self.encoder.config()?;
                    match layout {
                        Some((_, _, expected)) if *expected != config => {
                            return Err(encode_error(
                                "AVC configuration disagrees with the sealed recording",
                            ));
                        }
                        Some(_) => {}
                        None => *layout = Some((width, height, config.clone())),
                    }
                    slot.insert(MkvWriter::new(config, width, height))
                }
            };
            // Budget each packet and its container overhead before retaining it.
            self.buffered = self.buffered.saturating_add(sample.data.len() as u64 + 32);
            if self.buffered > MAX_SEGMENT_BYTES {
                return Err(encode_error("encoded segment exceeds the buffering limit"));
            }
            writer.push(sample).map_err(container_error)?;
        }
        Ok(())
    }
}

struct VideoInput {
    source: (u32, u32),
    dimensions: (u32, u32),
    padded: Vec<u8>,
    nv12: Nv12Frame,
}

impl VideoInput {
    fn new(frame: &Frame) -> Result<Self, RecordingError> {
        let width = frame
            .width
            .checked_next_multiple_of(2)
            .ok_or_else(|| encode_error("display is too wide"))?;
        let height = frame
            .height
            .checked_next_multiple_of(2)
            .ok_or_else(|| encode_error("display is too tall"))?;
        let nv12 = Nv12Frame::new(width, height)?;
        let mut padded = Vec::new();
        if (width, height) != (frame.width, frame.height) {
            let size = bgra_bytes(width, height)?;
            padded.try_reserve_exact(size).map_err(io::Error::other)?;
            padded.resize(size, 0);
        }
        Ok(Self {
            source: (frame.width, frame.height),
            dimensions: (width, height),
            padded,
            nv12,
        })
    }

    fn convert(&mut self, frame: &Frame) -> Result<(), RecordingError> {
        if (frame.width, frame.height) != self.source {
            return Err(RecordingError::Platform(Some(io::Error::other(
                "primary display dimensions changed during capture",
            ))));
        }
        if self.padded.is_empty() {
            return self.nv12.convert_bgra(&frame.pixels);
        }
        if frame.pixels.len() != bgra_bytes(frame.width, frame.height)? {
            return Err(encode_error("invalid BGRA buffer size"));
        }
        let source_row = bgra_bytes(frame.width, 1)?;
        let target_row = bgra_bytes(self.dimensions.0, 1)?;
        for (source, target) in frame
            .pixels
            .chunks_exact(source_row)
            .zip(self.padded.chunks_exact_mut(target_row))
        {
            target[..source_row].copy_from_slice(source);
        }
        self.nv12.convert_bgra(&self.padded)
    }
}

fn bgra_bytes(width: u32, height: u32) -> Result<usize, RecordingError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| encode_error("BGRA frame size overflows"))
}

fn encode_error(message: &'static str) -> RecordingError {
    RecordingError::Encoder(Some(io::Error::new(io::ErrorKind::InvalidData, message)))
}

fn container_error(error: crate::container::ContainerError) -> RecordingError {
    RecordingError::Encoder(Some(io::Error::new(io::ErrorKind::InvalidData, error)))
}
