//! `VideoToolbox` hardware acceptance tests.

use std::time::Duration;

use super::{DEFAULT_BITRATE, Encoder};
use crate::encode::Nv12Frame;

/// Builds a black NV12 frame of the requested even size.
fn black_frame(width: u32, height: u32) -> Nv12Frame {
    let mut frame = Nv12Frame::new(width, height).expect("even frame");
    let pixels =
        vec![128_u8; usize::try_from(u64::from(width) * u64::from(height) * 4).expect("size")];
    frame.convert_bgra(&pixels).expect("conversion");
    frame
}

#[test]
fn timestamps_are_converted_from_the_encoder_timescale() {
    use objc2_core_media::CMTime;

    // SAFETY: `CMTimeMake` has no preconditions for finite integer times.
    let time = unsafe { CMTime::new(1_500_000, 1_000_000) };

    assert_eq!(super::milliseconds(time), 1_500);
    // SAFETY: the invalid time is a framework constant.
    assert_eq!(
        super::milliseconds(unsafe { objc2_core_media::kCMTimeInvalid }),
        0
    );
}

#[test]
#[ignore = "requires `VideoToolbox`"]
fn opens_and_configures_an_h264_encoder() {
    let encoder = Encoder::open(1920, 1080, 15, DEFAULT_BITRATE).expect("encoder");

    let config = encoder.config().expect("configuration");
    assert!(config.record().expect("record").len() > 1);
}

#[test]
#[ignore = "requires `VideoToolbox`"]
fn encodes_frames_into_an_elementary_stream() {
    use crate::container::{MkvFile, MkvWriter};

    let mut encoder = Encoder::open(320, 240, 15, DEFAULT_BITRATE).expect("encoder");
    let config = encoder.config().expect("configuration");
    let mut writer = MkvWriter::new(config, 320, 240);
    let frame = black_frame(320, 240);
    let mut produced = 0_usize;
    for index in 0..10_u32 {
        let samples = encoder
            .encode(&frame, Duration::from_millis(u64::from(index) * 66))
            .expect("encode");
        for sample in samples {
            produced += 1;
            writer.push(sample).expect("push");
        }
    }
    for sample in encoder.flush().expect("flush") {
        produced += 1;
        writer.push(sample).expect("push");
    }
    let bytes = writer.finish().expect("render");
    let parsed = MkvFile::parse(&bytes).expect("parse");
    assert_eq!(produced, 10);
    assert_eq!(parsed.samples.len(), produced);
    assert!(parsed.samples[0].keyframe);
    println!(
        "samples: {produced}, first: {} bytes, file: {} bytes",
        parsed.samples[0].data.len(),
        bytes.len()
    );
}
