use super::*;

/// Microsoft MFT SPS with timing, bitstream restrictions and escaped bytes.
const SEQUENCE: [u8; 24] = [
    0x67, 0x4D, 0x40, 0x15, 0x95, 0xA0, 0x50, 0x7E, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0x78, 0x36,
    0x82, 0x21, 0x1A, 0x80,
];

/// Independently produced by `FFmpeg` `h264_metadata=chroma_sample_loc_type=1`.
const CENTERED_SEQUENCE: [u8; 24] = [
    0x67, 0x4D, 0x40, 0x15, 0x95, 0xA0, 0x50, 0x7E, 0x8A, 0x50, 0, 0, 3, 0, 0x10, 0, 0, 3, 1, 0xE0,
    0xDA, 8, 0x84, 0x6A,
];

#[test]
fn chroma_rewrite_matches_an_independent_bitstream_filter() {
    assert_eq!(centered_chroma(&SEQUENCE).expect("SPS"), CENTERED_SEQUENCE);
    assert_eq!(
        centered_chroma(&CENTERED_SEQUENCE).expect("repeat"),
        CENTERED_SEQUENCE
    );
}

#[test]
fn high_profile_keeps_timing_hrd_and_buffering_parameters() {
    // Independent x264 High Profile CBR/HRD fixture and FFmpeg reference output.
    let sequence = [
        0x67, 0x64, 0, 0x14, 0xAC, 0xD9, 0x44, 0x26, 0xC0, 0x44, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0x79,
        0x81, 0, 3, 0xD0, 0x90, 0, 0x7A, 0x13, 0x92, 0xCA, 1, 0xE2, 0x85, 0x32, 0xC0,
    ];
    let expected = [
        0x67, 0x64, 0, 0x14, 0xAC, 0xD9, 0x44, 0x26, 0xC0, 0x4A, 0x50, 0, 0, 3, 0, 0x10, 0, 0, 3,
        1, 0xE6, 4, 0, 0x0F, 0x42, 0x40, 1, 0xE8, 0x4E, 0x4B, 0x28, 7, 0x8A, 0x14, 0xCB,
    ];
    assert_eq!(
        centered_chroma(&sequence).expect("High Profile SPS"),
        expected
    );
}

#[test]
fn inserts_vui_when_the_encoder_omits_it_entirely() {
    // A baseline 320x240 SPS ending immediately after vui_parameters_present=0.
    let without_vui = [0x67, 0x42, 0, 0x1E, 0xF4, 0x0A, 0x0F, 0xC8];
    let result = centered_chroma(&without_vui).expect("SPS without VUI");
    let bytes = unescape(&result[1..]).expect("RBSP");
    let mut bits = Bits::new(&bytes).expect("bits");
    skip_sequence(&mut bits).expect("SPS prefix");
    assert_eq!(bits.read(1).expect("VUI flag"), 1);
    skip_vui_prefix(&mut bits).expect("VUI prefix");
    assert_eq!(bits.read(1).expect("chroma flag"), 1);
    assert_eq!(bits.ue(5).expect("top"), 1);
    assert_eq!(bits.ue(5).expect("bottom"), 1);
    assert_eq!(bits.read(5).expect("remaining VUI flags"), 0);
    assert_eq!(bits.position + 1, bits.data.len());
    assert_eq!(centered_chroma(&result).expect("repeat"), result);
}

#[test]
fn malformed_sps_or_emulation_prevention_is_rejected() {
    for bytes in [
        &[][..],
        &[0x67, 0x42, 0, 0x1E],
        &[0x68, 1, 2, 3, 4],
        &[0x67, 0x42, 0, 0, 3],
        &[0x67, 0x42, 0, 0, 3, 4],
        &[0x67, 0x42, 0, 0, 1],
        &[0x67, 0x42, 0, 0x1E, 0],
    ] {
        assert!(centered_chroma(bytes).is_err(), "invalid SPS: {bytes:?}");
    }
    for length in 5..SEQUENCE.len() {
        // Not every shorter prefix is structurally invalid, but no truncation
        // is allowed to panic or read beyond its input.
        let _ = centered_chroma(&SEQUENCE[..length]);
    }
}

#[test]
fn in_band_sps_is_updated_without_touching_slice_bytes() {
    let mut data = u32::try_from(SEQUENCE.len())
        .expect("length")
        .to_be_bytes()
        .to_vec();
    data.extend(SEQUENCE);
    let slice = [0, 0, 0, 3, 0x65, 0x88, 0x80];
    data.extend(slice);
    let normalized = crate::container::with_centered_chroma(&data).expect("sample");
    assert_eq!(
        &normalized[..4],
        &u32::try_from(CENTERED_SEQUENCE.len())
            .expect("length")
            .to_be_bytes()
    );
    assert_eq!(&normalized[4..28], &CENTERED_SEQUENCE);
    assert_eq!(&normalized[28..], &slice);
    for invalid in [&[0, 0, 0][..], &[0, 0, 0, 4, 0x65], &[0, 0, 0, 0]] {
        assert!(crate::container::with_centered_chroma(invalid).is_err());
    }
}
