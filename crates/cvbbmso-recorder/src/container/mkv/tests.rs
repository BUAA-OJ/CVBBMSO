use super::{MkvFile, MkvWriter};
use crate::container::avc::AvcConfig;
use crate::container::{ContainerError, Sample};

const SEQUENCE: [u8; 6] = [0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x01];
const PICTURE: [u8; 4] = [0x68, 0xCE, 0x38, 0x80];

fn config() -> AvcConfig {
    AvcConfig::new(SEQUENCE.to_vec(), PICTURE.to_vec()).expect("configuration")
}

fn writer() -> MkvWriter {
    MkvWriter::new(config(), 640, 360)
}

fn sample(timestamp_ms: u64, keyframe: bool) -> Sample {
    let mut data = Vec::new();
    data.extend_from_slice(&3_u32.to_be_bytes());
    data.extend_from_slice(&[0x65, 0x88, 0x84]);
    Sample {
        timestamp_ms,
        keyframe,
        data,
    }
}

fn round_trip(samples: &[Sample]) -> MkvFile {
    let mut writer = writer();
    for sample in samples {
        writer.push(sample.clone()).expect("push");
    }
    let bytes = writer.finish().expect("render");
    MkvFile::parse(&bytes).expect("parse")
}

#[test]
fn writes_the_ebml_signature() {
    let bytes = writer().finish().expect("render");
    assert_eq!(&bytes[0..4], &[0x1A, 0x45, 0xDF, 0xA3]);
}

#[test]
fn round_trips_a_single_sample() {
    let file = round_trip(&[sample(0, true)]);

    assert_eq!(file.width, 640);
    assert_eq!(file.height, 360);
    assert_eq!(file.config, config());
    assert_eq!(file.samples, vec![sample(0, true)]);
}

#[test]
fn round_trips_timestamps_and_keyframes() {
    let samples = [sample(0, true), sample(66, false), sample(133, false)];

    let file = round_trip(&samples);

    assert_eq!(file.samples, samples);
}

#[test]
fn splits_clusters_beyond_the_timecode_span() {
    let samples = [sample(0, true), sample(40_000, true)];

    let file = round_trip(&samples);

    assert_eq!(file.samples, samples);
}

#[test]
fn rejects_out_of_order_samples() {
    let mut writer = writer();
    writer.push(sample(100, true)).expect("push");

    assert!(matches!(
        writer.push(sample(50, true)),
        Err(ContainerError::OutOfOrder)
    ));
}

#[test]
fn parses_an_empty_segment() {
    let bytes = writer().finish().expect("render");

    let file = MkvFile::parse(&bytes).expect("parse");

    assert_eq!(file.samples, Vec::new());
}

#[test]
fn rejects_a_foreign_byte_stream() {
    assert!(MkvFile::parse(&[0x00, 0x01, 0x02]).is_err());
    assert!(MkvFile::parse(&[]).is_err());
}
