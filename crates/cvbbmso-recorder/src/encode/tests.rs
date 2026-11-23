use super::Nv12Frame;
use crate::RecordingError;

fn pixel(red: u8, green: u8, blue: u8) -> [u8; 4] {
    [blue, green, red, 0xFF]
}

fn converted(width: u32, height: u32, pixels: &[u8]) -> Nv12Frame {
    let mut frame = Nv12Frame::new(width, height).expect("even frame");
    frame.convert_bgra(pixels).expect("conversion");
    frame
}

fn assert_invalid<T>(result: Result<T, RecordingError>) {
    assert!(matches!(result, Err(RecordingError::Io(error))
        if error.kind() == std::io::ErrorKind::InvalidInput));
}

#[test]
fn gray_and_endpoints_use_limited_range() {
    for (gray, luma) in [(0, 16), (128, 126), (255, 235)] {
        let frame = converted(2, 2, &pixel(gray, gray, gray).repeat(4));
        assert_eq!(frame.as_bytes(), &[luma, luma, luma, luma, 128, 128]);
    }
}

#[test]
fn primary_colors_use_bt709_not_bt601() {
    for (color, yuv) in [
        (pixel(255, 0, 0), [63, 102, 240]),
        (pixel(0, 255, 0), [173, 42, 26]),
        (pixel(0, 0, 255), [32, 240, 118]),
    ] {
        let frame = converted(2, 2, &color.repeat(4));
        assert_eq!(&frame.as_bytes()[..4], &[yuv[0]; 4]);
        assert_eq!(&frame.as_bytes()[4..], &yuv[1..]);
    }
}

#[test]
fn luma_keeps_each_corner_and_chroma_averages_the_whole_block() {
    let pixels = [
        pixel(255, 0, 0),
        pixel(0, 255, 0),
        pixel(0, 0, 255),
        pixel(255, 255, 255),
    ]
    .concat();
    let frame = converted(2, 2, &pixels);
    assert_eq!(frame.as_bytes(), &[63, 173, 32, 235, 128, 128]);
}

#[test]
fn uv_pairs_stay_in_their_block_and_row() {
    let red = pixel(255, 0, 0);
    let blue = pixel(0, 0, 255);
    let green = pixel(0, 255, 0);
    let white = pixel(255, 255, 255);
    let pixels = [
        red, red, blue, blue, red, red, blue, blue, green, green, white, white, green, green,
        white, white,
    ]
    .concat();
    let frame = converted(4, 4, &pixels);
    assert_eq!(
        &frame.as_bytes()[16..],
        &[102, 240, 240, 118, 42, 26, 128, 128]
    );
}

#[test]
fn chroma_rounds_once_after_averaging_fractional_rgb_values() {
    let pixels = [
        pixel(29, 7, 24),
        pixel(5, 18, 23),
        pixel(12, 4, 2),
        pixel(14, 18, 5),
    ]
    .concat();
    // The average is (15, 11.75, 13.5). Truncating it to (15, 11, 13)
    // prematurely changes Cr from 129 to 130.
    let frame = converted(2, 2, &pixels);
    assert_eq!(&frame.as_bytes()[4..], &[128, 129]);
}

#[test]
fn alpha_is_ignored() {
    let opaque = pixel(7, 129, 245).repeat(4);
    let mut transparent = opaque.clone();
    for alpha in transparent.iter_mut().skip(3).step_by(4) {
        *alpha = 0;
    }
    assert_eq!(
        converted(2, 2, &opaque).as_bytes(),
        converted(2, 2, &transparent).as_bytes()
    );
}

#[test]
fn repeated_conversion_reuses_storage_and_replaces_both_planes() {
    let mut frame = Nv12Frame::new(4, 2).expect("frame");
    let pointer = frame.as_bytes().as_ptr();
    frame
        .convert_bgra(&pixel(255, 0, 0).repeat(8))
        .expect("red");
    assert_eq!(&frame.as_bytes()[8..], &[102, 240, 102, 240]);
    frame
        .convert_bgra(&pixel(0, 0, 0).repeat(8))
        .expect("black");
    assert_eq!(
        frame.as_bytes(),
        &[16, 16, 16, 16, 16, 16, 16, 16, 128, 128, 128, 128]
    );
    assert_eq!(frame.as_bytes().as_ptr(), pointer);
}

#[test]
fn constructor_rejects_zero_odd_and_overflowing_dimensions() {
    for (width, height) in [
        (0, 0),
        (0, 2),
        (2, 0),
        (1, 2),
        (2, 1),
        (3, 3),
        (u32::MAX - 1, u32::MAX - 1),
    ] {
        assert_invalid(Nv12Frame::new(width, height));
    }
}

#[test]
fn input_length_must_match_exactly_and_failures_preserve_the_previous_image() {
    let mut frame = converted(2, 2, &pixel(0, 0, 255).repeat(4));
    let saved = frame.as_bytes().to_vec();
    for length in [0, 3, 15, 17, 32] {
        assert_invalid(frame.convert_bgra(&vec![0; length]));
        assert_eq!(frame.as_bytes(), saved);
    }
}

#[test]
fn destination_pitch_is_respected_for_both_planes() {
    let pixels = [
        pixel(255, 0, 0),
        pixel(0, 255, 0),
        pixel(0, 0, 255),
        pixel(255, 255, 255),
    ]
    .concat();
    let frame = converted(2, 2, &pixels);
    let mut target = [0xCD; 15];
    assert_eq!(frame.copy_to(&mut target, 4).expect("copy"), 12);
    assert_eq!(
        target,
        [
            63, 173, 0xCD, 0xCD, 32, 235, 0xCD, 0xCD, 128, 128, 0xCD, 0xCD, 0xCD, 0xCD, 0xCD
        ]
    );
    let mut compact = [0; 6];
    assert_eq!(frame.copy_to(&mut compact, 2).expect("compact"), 6);
    assert_eq!(compact, frame.as_bytes());
}

#[test]
fn invalid_destination_cannot_be_partially_written() {
    let frame = Nv12Frame::new(2, 2).expect("frame");
    let mut target = [0xCD; 12];
    assert_invalid(frame.copy_to(&mut target, 1));
    assert_invalid(frame.copy_to(&mut target, usize::MAX));
    assert_invalid(frame.copy_to(&mut target[..11], 4));
    assert_eq!(target, [0xCD; 12]);
}

/// Independent BT.709 reference, using floating-point Kr/Kb instead of the
/// converter's fixed-point coefficients. Alpha has no role in this reference.
fn reference(red: f64, green: f64, blue: f64) -> [f64; 3] {
    let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    [
        (16.0 + 219.0 * luma / 255.0).round(),
        (128.0 + 112.0 * (blue - luma) / (255.0 * (1.0 - 0.0722))).round(),
        (128.0 + 112.0 * (red - luma) / (255.0 * (1.0 - 0.2126))).round(),
    ]
}

#[test]
fn mixed_chroma_and_luma_match_an_independent_reference_within_one_code_value() {
    // Fixed seed and integer generator keep this test deterministic.
    let mut state = 0x1234_5678_u32;
    let pixels: Vec<u8> = (0..16 * 8 * 4)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state.to_be_bytes()[0]
        })
        .collect();
    let frame = converted(16, 8, &pixels);
    let data = frame.as_bytes();
    for y in (0..8).step_by(2) {
        for x in (0..16).step_by(2) {
            let mut sum = [0.0; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let index = (y + dy) * 16 + x + dx;
                    let b = f64::from(pixels[index * 4]);
                    let g = f64::from(pixels[index * 4 + 1]);
                    let r = f64::from(pixels[index * 4 + 2]);
                    assert!((f64::from(data[index]) - reference(r, g, b)[0]).abs() <= 1.0);
                    sum[0] += r;
                    sum[1] += g;
                    sum[2] += b;
                }
            }
            let expected = reference(sum[0] / 4.0, sum[1] / 4.0, sum[2] / 4.0);
            let uv = 128 + (y / 2) * 16 + x;
            assert!((f64::from(data[uv]) - expected[1]).abs() <= 1.0);
            assert!((f64::from(data[uv + 1]) - expected[2]).abs() <= 1.0);
        }
    }
}
