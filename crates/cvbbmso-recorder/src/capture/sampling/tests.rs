use std::{cell::Cell, collections::VecDeque, rc::Rc};

use super::*;

#[derive(Default)]
struct FakeClock {
    elapsed: Rc<Cell<Duration>>,
    cancel_at: Option<Duration>,
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.elapsed.get()
    }

    fn wait_until(&mut self, deadline: Duration, stopped: &AtomicBool) {
        if stopped.load(Ordering::Acquire) {
            return;
        }
        if let Some(at) = self.cancel_at
            && deadline.max(self.now()) >= at
        {
            self.elapsed.set(at.max(self.now()));
            stopped.store(true, Ordering::Release);
            return;
        }
        self.elapsed.set(deadline.max(self.now()));
    }
}

struct Source<'a> {
    images: VecDeque<Result<Option<Frame>, RecordingError>>,
    timeouts: Vec<u32>,
    cancel_on_read: Option<&'a AtomicBool>,
    read_delay: Option<(Rc<Cell<Duration>>, Duration)>,
}

impl Source<'_> {
    fn new(images: impl IntoIterator<Item = Option<Frame>>) -> Self {
        Self {
            images: images.into_iter().map(Ok).collect(),
            timeouts: Vec::new(),
            cancel_on_read: None,
            read_delay: None,
        }
    }
}

impl FrameSource for Source<'_> {
    fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, RecordingError> {
        self.timeouts.push(timeout_ms);
        if let Some((elapsed, delay)) = &self.read_delay {
            elapsed.set(elapsed.get() + *delay);
        }
        if let Some(stopped) = self.cancel_on_read {
            stopped.store(true, Ordering::Release);
        }
        self.images.pop_front().unwrap_or(Ok(None))
    }
}

fn image(value: u8) -> Frame {
    Frame {
        width: 1,
        height: 1,
        pixels: vec![value, 0, 0, 255],
    }
}

fn rate(fps: u32) -> NonZeroU32 {
    NonZeroU32::new(fps).expect("positive test rate")
}

#[test]
fn readiness_follows_the_first_accepted_frame_and_is_announced_once() {
    let started = Cell::new(0);
    let frames = Cell::new(0);
    sample_started(
        &mut Source::new([None, Some(image(1)), None]),
        &mut FakeClock::default(),
        rate(10),
        &AtomicBool::new(false),
        || started.set(started.get() + 1),
        |_| {
            assert_eq!(started.get(), u32::from(frames.get() > 0));
            frames.set(frames.get() + 1);
            Ok(if frames.get() == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        },
    )
    .expect("capture");
    assert_eq!(started.get(), 1);
}

#[test]
fn a_display_without_images_times_out_without_reporting_recovery() {
    let started = Cell::new(false);
    let mut clock = FakeClock::default();
    let result = sample_started(
        &mut Source::new([]),
        &mut clock,
        rate(10),
        &AtomicBool::new(false),
        || started.set(true),
        |_| panic!("no image was available"),
    );
    assert!(
        matches!(result, Err(RecordingError::Platform(Some(error))) if error.kind() == io::ErrorKind::TimedOut)
    );
    assert!(!started.get());
    assert_eq!(clock.now(), FIRST_FRAME_TIMEOUT);
}

#[test]
fn a_late_first_frame_is_not_delivered_or_reported_as_recovered() {
    let started = Cell::new(false);
    let mut clock = FakeClock::default();
    let mut source = Source::new([Some(image(1))]);
    source.read_delay = Some((
        Rc::clone(&clock.elapsed),
        FIRST_FRAME_TIMEOUT + Duration::from_millis(1),
    ));
    let result = sample_started(
        &mut source,
        &mut clock,
        rate(10),
        &AtomicBool::new(false),
        || started.set(true),
        |_| panic!("a late first frame must not reach the sink"),
    );
    assert!(
        matches!(result, Err(RecordingError::Platform(Some(error))) if error.kind() == io::ErrorKind::TimedOut)
    );
    assert!(!started.get());
}

#[test]
fn a_rejected_first_frame_does_not_report_recovery() {
    let started = Cell::new(false);
    let result = sample_started(
        &mut Source::new([Some(image(1))]),
        &mut FakeClock::default(),
        rate(10),
        &AtomicBool::new(false),
        || started.set(true),
        |_| Err(RecordingError::Encoder(None)),
    );
    assert!(matches!(result, Err(RecordingError::Encoder(None))));
    assert!(!started.get());
}

#[test]
fn changing_images_are_sampled_on_absolute_ticks_without_rounding_drift() {
    let mut clock = FakeClock::default();
    let mut source = Source::new((0..16).map(|value| Some(image(value))));
    let stopped = AtomicBool::new(false);
    let mut frames = Vec::new();
    sample(&mut source, &mut clock, rate(15), &stopped, |frame| {
        frames.push(frame);
        Ok(if frames.len() == 16 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })
    .expect("sample");

    assert_eq!(frames[0].elapsed, Duration::ZERO);
    assert_eq!(frames[1].elapsed, Duration::from_nanos(66_666_667));
    assert_eq!(frames[2].elapsed, Duration::from_nanos(133_333_334));
    assert_eq!(frames[15].elapsed, Duration::from_secs(1));
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(usize::from(frame.frame.pixels[0]), index);
    }
}

#[test]
fn still_images_reuse_the_buffer_with_advancing_timestamps() {
    let mut clock = FakeClock::default();
    let mut source = Source::new([Some(image(42)), None, None]);
    let mut frames = Vec::new();
    sample(
        &mut source,
        &mut clock,
        rate(10),
        &AtomicBool::new(false),
        |frame| {
            frames.push(frame);
            Ok(if frames.len() == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        },
    )
    .expect("sample");

    assert!(Arc::ptr_eq(&frames[0].frame, &frames[1].frame));
    assert!(Arc::ptr_eq(&frames[1].frame, &frames[2].frame));
    assert_eq!(frames[1].elapsed, Duration::from_millis(100));
    assert_eq!(frames[2].elapsed, Duration::from_millis(200));
    assert_eq!(source.timeouts, [STOP_POLL_MS, 0, 0]);
}

#[test]
fn initial_timeouts_produce_no_empty_frame_and_do_not_spin() {
    let mut clock = FakeClock::default();
    let mut source = Source::new([None, None, Some(image(1))]);
    let mut times = Vec::new();
    sample(
        &mut source,
        &mut clock,
        rate(10),
        &AtomicBool::new(false),
        |frame| {
            times.push(frame.elapsed);
            Ok(ControlFlow::Break(()))
        },
    )
    .expect("sample");

    assert_eq!(times, [Duration::from_millis(200)]);
    assert_eq!(source.timeouts.len(), 3);
}

#[test]
fn slow_sink_skips_missed_ticks_instead_of_emitting_a_burst() {
    let mut clock = FakeClock::default();
    let elapsed = Rc::clone(&clock.elapsed);
    let mut source = Source::new([Some(image(1))]);
    let mut times = Vec::new();
    sample(
        &mut source,
        &mut clock,
        rate(10),
        &AtomicBool::new(false),
        |frame| {
            times.push(frame.elapsed);
            elapsed.set(elapsed.get() + Duration::from_millis(250));
            Ok(if times.len() == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        },
    )
    .expect("sample");

    assert_eq!(times, [0, 300, 600].map(Duration::from_millis));
    assert_eq!(source.timeouts.len(), 3);
}

#[test]
fn slow_readback_skips_ticks_and_keeps_the_sampling_time() {
    let mut clock = FakeClock::default();
    let mut source = Source::new((0..3).map(|value| Some(image(value))));
    source.read_delay = Some((Rc::clone(&clock.elapsed), Duration::from_millis(250)));
    let mut times = Vec::new();
    sample(
        &mut source,
        &mut clock,
        rate(10),
        &AtomicBool::new(false),
        |frame| {
            times.push(frame.elapsed);
            Ok(if times.len() == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        },
    )
    .expect("sample");

    assert_eq!(times, [0, 300, 600].map(Duration::from_millis));
    assert_eq!(source.timeouts.len(), 3);
}

#[test]
fn stop_during_wait_emits_no_extra_sample() {
    let mut clock = FakeClock {
        cancel_at: Some(Duration::from_millis(50)),
        ..FakeClock::default()
    };
    let mut source = Source::new([Some(image(1))]);
    let mut count = 0;
    sample(
        &mut source,
        &mut clock,
        rate(1),
        &AtomicBool::new(false),
        |_| {
            count += 1;
            Ok(ControlFlow::Continue(()))
        },
    )
    .expect("stop");

    assert_eq!(count, 1);
    assert_eq!(source.timeouts.len(), 1);
    assert_eq!(clock.now(), Duration::from_millis(50));
}

#[test]
fn stop_during_acquisition_prevents_delivery() {
    let stopped = AtomicBool::new(false);
    let mut source = Source::new([Some(image(1))]);
    source.cancel_on_read = Some(&stopped);
    sample(
        &mut source,
        &mut FakeClock::default(),
        rate(10),
        &stopped,
        |_| panic!("must not emit after cancellation"),
    )
    .expect("stop");
    assert_eq!(source.timeouts.len(), 1);
}

#[test]
fn source_failure_is_returned_instead_of_reusing_the_previous_image() {
    let mut source = Source::new([Some(image(1))]);
    source
        .images
        .push_back(Err(RecordingError::Platform(Some(io::Error::other(
            "lost capture device",
        )))));
    let mut count = 0;
    let result = sample(
        &mut source,
        &mut FakeClock::default(),
        rate(10),
        &AtomicBool::new(false),
        |_| {
            count += 1;
            Ok(ControlFlow::Continue(()))
        },
    );
    assert!(matches!(result, Err(RecordingError::Platform(Some(_)))));
    assert_eq!(count, 1);
}

#[test]
fn sink_failure_is_returned_without_taking_another_frame() {
    let mut source = Source::new([Some(image(1))]);
    let result = sample(
        &mut source,
        &mut FakeClock::default(),
        rate(10),
        &AtomicBool::new(false),
        |_| Err(io::Error::new(io::ErrorKind::WriteZero, "raw frame sink failed").into()),
    );
    assert!(
        matches!(result, Err(RecordingError::Io(error)) if error.kind() == io::ErrorKind::WriteZero)
    );
    assert_eq!(source.timeouts.len(), 1);
}

#[test]
fn invalid_policy_is_rejected_before_opening_the_display() {
    for policy in [
        RecordingPolicy {
            segment_duration_secs: 0,
            fps: 15,
        },
        RecordingPolicy {
            segment_duration_secs: 30,
            fps: 0,
        },
    ] {
        let result = run(policy, &AtomicBool::new(false), |_| {
            panic!("invalid policy")
        });
        assert!(
            matches!(result, Err(RecordingError::Io(error)) if error.kind() == io::ErrorKind::InvalidInput)
        );
    }
}

#[test]
fn stop_before_start_does_not_open_the_display() {
    run(
        RecordingPolicy::new(30, 15).expect("policy"),
        &AtomicBool::new(true),
        |_| panic!("stopped capture"),
    )
    .expect("stop");
}

#[test]
fn extreme_rate_still_advances_time() {
    assert_eq!(
        next_deadline(Duration::ZERO, rate(u32::MAX)).expect("deadline"),
        Duration::from_nanos(1)
    );
    assert_eq!(
        next_deadline(Duration::from_nanos(1), rate(u32::MAX)).expect("deadline"),
        Duration::from_nanos(2)
    );
}
