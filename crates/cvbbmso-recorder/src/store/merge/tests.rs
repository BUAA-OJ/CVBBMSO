//! Merge behavior over real segment files.

use std::fs;

use tempfile::TempDir;
use time::OffsetDateTime;

use super::SegmentStore;
use crate::RecordingError;
use crate::container::{AvcConfig, MkvWriter, Sample};

const SEQUENCE: [u8; 24] = [
    0x67, 0x4D, 0x40, 0x15, 0x95, 0xA0, 0x50, 0x7E, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0x78, 0x36,
    0x82, 0x21, 0x1A, 0x80,
];
const PICTURE: [u8; 4] = [0x68, 0xCE, 0x38, 0x80];

fn config() -> AvcConfig {
    AvcConfig::new(SEQUENCE.to_vec(), PICTURE.to_vec()).expect("configuration")
}

fn timestamp(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).expect("timestamp in range")
}

fn sample(timestamp_ms: u64, keyframe: bool, byte: u8) -> Sample {
    Sample {
        timestamp_ms,
        keyframe,
        data: vec![0, 0, 0, 3, 0x41, byte, byte],
    }
}

/// Renders a segment whose sample payloads are distinguishable by segment.
fn segment(timestamps: &[u64], byte: u8) -> Vec<u8> {
    let mut writer = MkvWriter::new(config(), 320, 240);
    for (index, timestamp) in timestamps.iter().copied().enumerate() {
        writer
            .push(sample(timestamp, index == 0, byte))
            .expect("push");
    }
    writer.finish().expect("render")
}

fn store(directory: &TempDir) -> SegmentStore {
    SegmentStore::open(directory.path()).expect("store")
}

fn seal(store: &mut SegmentStore, index: u32, seconds: i64, bytes: &[u8]) {
    fs::write(store.video_path(index), bytes).expect("video");
    store
        .seal(index, timestamp(seconds), timestamp(seconds + 30))
        .expect("seal");
}

/// One top-level MP4 box.
struct Boxes<'a> {
    entries: Vec<([u8; 4], &'a [u8])>,
}

impl<'a> Boxes<'a> {
    fn parse(data: &'a [u8]) -> Self {
        let mut entries = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let size = u32::from_be_bytes(data[offset..offset + 4].try_into().expect("box size"));
            let id = data[offset + 4..offset + 8].try_into().expect("box id");
            let (header, length) = if size == 1 {
                let large = u64::from_be_bytes(
                    data[offset + 8..offset + 16]
                        .try_into()
                        .expect("large box size"),
                );
                (16, usize::try_from(large).expect("box fits memory"))
            } else {
                (8, usize::try_from(size).expect("box fits memory"))
            };
            entries.push((id, &data[offset + header..offset + length]));
            offset += length;
        }
        Self { entries }
    }

    fn child(&self, id: [u8; 4]) -> &'a [u8] {
        self.entries
            .iter()
            .find(|(entry_id, _)| *entry_id == id)
            .map(|(_, payload)| *payload)
            .expect("box is present")
    }
}

fn sample_count(file: &[u8]) -> u32 {
    let top = Boxes::parse(file);
    let moov = top.child(*b"moov");
    let trak = Boxes::parse(moov).child(*b"trak");
    let mdia = Boxes::parse(trak).child(*b"mdia");
    let minf = Boxes::parse(mdia).child(*b"minf");
    let stbl = Boxes::parse(minf).child(*b"stbl");
    let stsz = Boxes::parse(stbl).child(*b"stsz");
    u32::from_be_bytes(stsz[8..12].try_into().expect("sample count"))
}

fn deltas(file: &[u8]) -> Vec<u32> {
    let top = Boxes::parse(file);
    let moov = Boxes::parse(top.child(*b"moov"));
    let trak = Boxes::parse(moov.child(*b"trak"));
    let mdia = Boxes::parse(trak.child(*b"mdia"));
    let minf = Boxes::parse(mdia.child(*b"minf"));
    let stbl = Boxes::parse(minf.child(*b"stbl"));
    let stts = stbl.child(*b"stts");
    let runs = u32::from_be_bytes(stts[4..8].try_into().expect("run count"));
    let mut deltas = Vec::new();
    for index in 0..runs {
        let offset = 8 + 8 * usize::try_from(index).expect("index");
        let count = u32::from_be_bytes(stts[offset..offset + 4].try_into().expect("count"));
        let delta = u32::from_be_bytes(stts[offset + 4..offset + 8].try_into().expect("delta"));
        deltas.extend(std::iter::repeat_n(
            delta,
            usize::try_from(count).expect("count"),
        ));
    }
    deltas
}

/// Reads the duration fields that must agree with the sample table.
fn durations(file: &[u8]) -> (u32, u32, u32) {
    let top = Boxes::parse(file);
    let moov = Boxes::parse(top.child(*b"moov"));
    let mvhd = moov.child(*b"mvhd");
    let trak = Boxes::parse(moov.child(*b"trak"));
    let tkhd = trak.child(*b"tkhd");
    let mdia = Boxes::parse(trak.child(*b"mdia"));
    let media_header = mdia.child(*b"mdhd");
    let read = |payload: &[u8], offset: usize| {
        u32::from_be_bytes(payload[offset..offset + 4].try_into().expect("duration"))
    };
    (read(mvhd, 16), read(tkhd, 20), read(media_header, 16))
}

fn mdat(file: &[u8]) -> Vec<u8> {
    Boxes::parse(file).child(*b"mdat").to_vec()
}

fn expected_payload(segments: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for bytes in segments {
        let file = crate::container::MkvFile::parse(bytes).expect("segment");
        for sample in file.samples {
            payload.extend_from_slice(&sample.data);
        }
    }
    payload
}

#[test]
fn merges_segments_into_one_movie() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    let first = segment(&[0, 66, 132], 1);
    let second = segment(&[0, 66], 2);
    seal(&mut store, 1, 0, &first);
    seal(&mut store, 2, 30, &second);

    let digest = store.merge().expect("merge");
    let file = fs::read(store.final_path()).expect("final video");

    assert_eq!(sample_count(&file), 5);
    assert_eq!(mdat(&file), expected_payload(&[first, second]));
    assert_eq!(
        digest,
        crate::store::hash_file(&store.final_path()).expect("digest")
    );
}

#[test]
fn legacy_in_band_sps_matches_avcc_and_preserves_the_sealed_segment() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    let sequence = [0x67, 0x42, 0, 0x1E, 0xF4, 0x0A, 0x0F, 0xC8];
    let legacy = AvcConfig::new(sequence.to_vec(), PICTURE.to_vec()).expect("legacy config");
    let mut payload = u32::try_from(sequence.len())
        .expect("length")
        .to_be_bytes()
        .to_vec();
    payload.extend(sequence);
    payload.extend([0, 0, 0, 3, 0x65, 0x88, 0x80]);
    let expected = crate::container::with_centered_chroma(&payload).expect("updated NALs");
    assert_ne!(payload.len(), expected.len(), "fixture changes SPS length");
    let mut writer = MkvWriter::new(legacy.clone(), 320, 240);
    writer
        .push(Sample {
            timestamp_ms: 0,
            keyframe: true,
            data: payload,
        })
        .expect("sample");
    let original = writer.finish().expect("segment");
    seal(&mut store, 1, 0, &original);

    store.merge().expect("merge legacy recording");
    let movie = fs::read(store.final_path()).expect("movie");
    assert_eq!(mdat(&movie), expected);
    let mut table = movie.as_slice();
    for id in [*b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl"] {
        table = Boxes::parse(table).child(id);
    }
    let sizes = Boxes::parse(table).child(*b"stsz");
    assert_eq!(
        u32::from_be_bytes(sizes[12..16].try_into().expect("sample size")),
        u32::try_from(expected.len()).expect("expected length")
    );
    let description = Boxes::parse(table).child(*b"stsd");
    let entry = Boxes::parse(&description[8..]).child(*b"avc1");
    let config = Boxes::parse(&entry[78..]).child(*b"avcC");
    assert_eq!(
        config,
        legacy
            .with_centered_chroma()
            .expect("VUI")
            .record()
            .expect("record")
    );
    assert_eq!(
        fs::read(store.video_path(1)).expect("sealed video"),
        original
    );
    assert_eq!(
        store.segments()[0].sha256,
        crate::store::hash_file(&store.video_path(1)).expect("hash")
    );
}

#[test]
fn normalizes_nonzero_segment_origins() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    seal(&mut store, 1, 0, &segment(&[100, 166], 1));
    seal(&mut store, 2, 30, &segment(&[100, 166], 2));

    store.merge().expect("merge");
    let file = fs::read(store.final_path()).expect("final video");

    assert_eq!(deltas(&file), [66, 66, 66, 66]);
    assert_eq!(durations(&file), (264, 264, 264));
}

#[test]
fn normalizes_a_second_segment_with_global_timestamps() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    seal(&mut store, 1, 0, &segment(&[0, 66], 1));
    seal(&mut store, 2, 30, &segment(&[1_000, 1_066], 2));

    store.merge().expect("merge");
    let file = fs::read(store.final_path()).expect("final video");

    assert_eq!(deltas(&file), [66, 66, 66, 66]);
    assert_eq!(durations(&file), (264, 264, 264));
}

#[test]
fn final_single_frame_segment_keeps_the_one_millisecond_tail() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    seal(&mut store, 1, 0, &segment(&[0, 66], 1));
    seal(&mut store, 2, 30, &segment(&[0], 2));

    store.merge().expect("merge");
    let file = fs::read(store.final_path()).expect("final video");

    assert_eq!(deltas(&file), [66, 66, 1]);
    assert_eq!(durations(&file), (133, 133, 133));
}

#[test]
fn a_single_frame_recording_lasts_one_millisecond() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    seal(&mut store, 1, 0, &segment(&[0], 1));

    store.merge().expect("merge");
    let file = fs::read(store.final_path()).expect("final video");

    assert_eq!(deltas(&file), [1]);
    assert_eq!(durations(&file), (1, 1, 1));
}

#[test]
fn final_segment_tail_uses_its_own_frame_interval() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    seal(&mut store, 1, 0, &segment(&[0, 66], 1));
    seal(&mut store, 2, 30, &segment(&[0, 100], 2));

    store.merge().expect("merge");
    let file = fs::read(store.final_path()).expect("final video");

    assert_eq!(deltas(&file), [66, 66, 100, 100]);
    assert_eq!(durations(&file), (332, 332, 332));
}

#[test]
fn rejects_a_tampered_segment_without_publishing_a_final_file() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    let bytes = segment(&[0, 66], 1);
    seal(&mut store, 1, 0, &bytes);
    fs::write(store.video_path(1), b"not the sealed bytes").expect("tamper");

    assert!(matches!(store.merge(), Err(RecordingError::Merge(_))));
    assert!(!store.final_path().exists());
}

#[test]
fn rejects_segments_that_disagree_about_the_video_layout() {
    let directory = TempDir::new().expect("temp dir");
    let mut store = store(&directory);
    seal(&mut store, 1, 0, &segment(&[0, 66], 1));
    let mut writer = MkvWriter::new(config(), 640, 360);
    writer.push(sample(0, true, 2)).expect("push");
    let different = writer.finish().expect("render");
    seal(&mut store, 2, 30, &different);

    assert!(matches!(store.merge(), Err(RecordingError::Merge(_))));
}

#[test]
fn reports_no_segments_for_an_empty_store() {
    let directory = TempDir::new().expect("temp dir");
    let store = store(&directory);

    assert!(matches!(store.merge(), Err(RecordingError::NoSegments)));
}

#[test]
fn removes_a_stale_final_temporary_file_on_open() {
    let directory = TempDir::new().expect("temp dir");
    let opened = store(&directory);
    fs::write(opened.final_temporary_path(), b"half written").expect("stale temporary");
    drop(opened);

    let reopened = store(&directory);

    assert!(!reopened.final_temporary_path().exists());
}
