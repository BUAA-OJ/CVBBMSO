//! Structural checks for the MP4 writer.

use super::{Mp4Writer, boxed};
use crate::container::{AvcConfig, Sample};

const SEQUENCE: [u8; 6] = [0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x01];
const PICTURE: [u8; 4] = [0x68, 0xCE, 0x38, 0x80];

fn config() -> AvcConfig {
    AvcConfig::new(SEQUENCE.to_vec(), PICTURE.to_vec()).expect("configuration")
}

fn sample(timestamp_ms: u64, keyframe: bool, length: usize) -> Sample {
    Sample {
        timestamp_ms,
        keyframe,
        data: vec![0x41; length],
    }
}

/// One top-level box.
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
            .map_or_else(
                || panic!("missing box {:?}", String::from_utf8_lossy(&id)),
                |(_, payload)| *payload,
            )
    }
}

/// Parsed view of a generated MP4 file.
struct Parsed {
    width: u16,
    height: u16,
    config: AvcConfig,
    colour: Option<(u16, u16, u16, u8)>,
    sync: Vec<u32>,
    sizes: Vec<u32>,
    deltas: Vec<u32>,
    chunk_offset: u64,
    data: Vec<u8>,
}

fn parse(file: &[u8]) -> Parsed {
    let top = Boxes::parse(file);
    assert_eq!(top.child(*b"ftyp")[..4], *b"isom");
    let moov = Boxes::parse(top.child(*b"moov"));
    let mvhd = moov.child(*b"mvhd");
    assert_eq!(&mvhd[..4], &[0, 0, 0, 0]);
    let trak = Boxes::parse(moov.child(*b"trak"));
    let tkhd = trak.child(*b"tkhd");
    let width = u16::try_from(u32::from_be_bytes(tkhd[76..80].try_into().expect("width")) >> 16)
        .expect("width");
    let height = u16::try_from(u32::from_be_bytes(tkhd[80..84].try_into().expect("height")) >> 16)
        .expect("height");

    let mdia = Boxes::parse(trak.child(*b"mdia"));
    let minf = Boxes::parse(mdia.child(*b"minf"));
    let stbl = Boxes::parse(minf.child(*b"stbl"));

    let description_box = stbl.child(*b"stsd");
    let sample_entry = Boxes::parse(&description_box[8..]).child(*b"avc1");
    let decoder_config = Boxes::parse(&sample_entry[78..]).child(*b"avcC");
    let config = AvcConfig::parse(decoder_config).expect("configuration record");
    let colour = Boxes::parse(&sample_entry[78..])
        .entries
        .iter()
        .find(|(id, _)| *id == *b"colr")
        .map(|(_, payload)| {
            (
                u16::from_be_bytes(payload[4..6].try_into().expect("primaries")),
                u16::from_be_bytes(payload[6..8].try_into().expect("transfer")),
                u16::from_be_bytes(payload[8..10].try_into().expect("matrix")),
                payload[10],
            )
        });

    let time_box = stbl.child(*b"stts");
    let runs = u32::from_be_bytes(time_box[4..8].try_into().expect("run count"));
    let mut deltas = Vec::new();
    for index in 0..runs {
        let offset = 8 + 8 * usize::try_from(index).expect("index");
        let count = u32::from_be_bytes(time_box[offset..offset + 4].try_into().expect("run count"));
        let delta = u32::from_be_bytes(
            time_box[offset + 4..offset + 8]
                .try_into()
                .expect("run delta"),
        );
        deltas.extend(std::iter::repeat_n(
            delta,
            usize::try_from(count).expect("count"),
        ));
    }

    let size_box = stbl.child(*b"stsz");
    let count = u32::from_be_bytes(size_box[8..12].try_into().expect("sample count"));
    let sizes = (0..count)
        .map(|index| {
            let offset = 12 + 4 * usize::try_from(index).expect("index");
            u32::from_be_bytes(
                size_box[offset..offset + 4]
                    .try_into()
                    .expect("sample size"),
            )
        })
        .collect();

    let offset_box = stbl.child(*b"stco");
    let chunks = u32::from_be_bytes(offset_box[4..8].try_into().expect("chunk count"));
    let chunk_offset = if chunks == 0 {
        0
    } else {
        u64::from(u32::from_be_bytes(
            offset_box[8..12].try_into().expect("chunk offset"),
        ))
    };

    let sync_box = stbl.child(*b"stss");
    let sync_count = u32::from_be_bytes(sync_box[4..8].try_into().expect("sync count"));
    let sync = (0..sync_count)
        .map(|index| {
            let offset = 8 + 4 * usize::try_from(index).expect("index");
            u32::from_be_bytes(
                sync_box[offset..offset + 4]
                    .try_into()
                    .expect("sync sample"),
            )
        })
        .collect();

    let mdat = top.child(*b"mdat").to_vec();
    Parsed {
        width,
        height,
        config,
        colour,
        sync,
        sizes,
        deltas,
        chunk_offset,
        data: mdat,
    }
}

fn written(samples: &[Sample]) -> (Vec<u8>, Vec<u8>) {
    let mut writer = Mp4Writer::new(config(), 640, 360);
    for sample in samples {
        writer
            .push(sample.timestamp_ms, sample.keyframe, sample.data.len())
            .expect("push");
    }
    let mut file = writer.start().expect("start");
    let payload: Vec<u8> = samples
        .iter()
        .flat_map(|sample| sample.data.clone())
        .collect();
    assert_eq!(
        u64::try_from(payload.len()).expect("length"),
        writer.data_length()
    );
    file.extend_from_slice(&payload);
    (file, payload)
}

#[test]
fn writes_a_playable_movie_header() {
    let samples = [
        sample(0, true, 12),
        sample(66, false, 8),
        sample(132, false, 9),
    ];
    let (file, payload) = written(&samples);
    let parsed = parse(&file);

    assert_eq!(parsed.width, 640);
    assert_eq!(parsed.height, 360);
    assert_eq!(parsed.config, config());
    assert_eq!(parsed.colour, Some((1, 1, 1, 0)));
    assert_eq!(parsed.sizes, [12, 8, 9]);
    assert_eq!(parsed.deltas, [66, 66, 66]);
    assert_eq!(parsed.sync, [1]);
    assert_eq!(parsed.chunk_offset, (file.len() - payload.len()) as u64);
    assert_eq!(parsed.data, payload);
}

#[test]
fn explicit_tail_duration_overrides_the_previous_delta() {
    let samples = [sample(0, true, 12), sample(66, false, 8)];
    let mut writer = Mp4Writer::new(config(), 640, 360);
    for sample in &samples {
        writer
            .push(sample.timestamp_ms, sample.keyframe, sample.data.len())
            .expect("push");
    }
    writer.set_tail_duration(1);
    let mut file = writer.start().expect("start");
    for sample in samples {
        file.extend_from_slice(&sample.data);
    }

    let parsed = parse(&file);

    assert_eq!(parsed.deltas, [66, 1]);
    assert_eq!(parsed.colour, Some((1, 1, 1, 0)));
}

#[test]
fn writes_an_empty_movie() {
    let writer = Mp4Writer::new(config(), 640, 360);
    let file = writer.start().expect("start");
    assert_eq!(writer.data_length(), 0);
    let parsed = parse(&file);

    assert_eq!(parsed.sizes, Vec::<u32>::new());
    assert_eq!(parsed.sync, Vec::<u32>::new());
    assert_eq!(parsed.colour, Some((1, 1, 1, 0)));
    assert_eq!(parsed.data, Vec::<u8>::new());
}

#[test]
fn rejects_out_of_order_samples() {
    let mut writer = Mp4Writer::new(config(), 640, 360);
    writer.push(100, true, 4).expect("first");

    assert!(matches!(
        writer.push(50, true, 4),
        Err(crate::container::ContainerError::OutOfOrder)
    ));
}

#[test]
fn box_header_points_at_the_payload() {
    let boxed = boxed(*b"test", b"payload");
    assert_eq!(&boxed[..4], &[0, 0, 0, 15]);
    assert_eq!(&boxed[4..8], b"test");
    assert_eq!(&boxed[8..], b"payload");
}
