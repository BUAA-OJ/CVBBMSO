use super::bgra_to_nv12;

fn pixel(red: u8, green: u8, blue: u8) -> [u8; 4] {
    [blue, green, red, 0xFF]
}

fn frame(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
    let count = usize::try_from(width * height).expect("pixel count");
    colour.repeat(count)
}

#[test]
fn mid_gray_lands_in_limited_range() {
    let nv12 = bgra_to_nv12(&frame(2, 2, pixel(128, 128, 128)), 2, 2);

    assert_eq!(nv12.len(), 4 + 2);
    assert_eq!(&nv12[..4], &[126, 126, 126, 126]);
    assert_eq!(&nv12[4..], &[128, 128]);
}

#[test]
fn black_and_white_stay_inside_the_video_range() {
    let black = bgra_to_nv12(&frame(2, 2, pixel(0, 0, 0)), 2, 2);
    let white = bgra_to_nv12(&frame(2, 2, pixel(255, 255, 255)), 2, 2);

    assert_eq!(black[0], 16);
    assert_eq!(white[0], 235);
    assert_eq!(&black[4..], &[128, 128]);
    assert_eq!(&white[4..], &[128, 128]);
}

#[test]
fn chroma_is_subsampled_per_block() {
    let mut pixels = Vec::new();
    for _ in 0..2 {
        pixels.extend_from_slice(&pixel(255, 0, 0));
        pixels.extend_from_slice(&pixel(255, 0, 0));
        pixels.extend_from_slice(&pixel(0, 0, 255));
        pixels.extend_from_slice(&pixel(0, 0, 255));
    }

    let nv12 = bgra_to_nv12(&pixels, 4, 2);

    assert_eq!(nv12.len(), 8 + 4);
    // Red block then blue block, Cb and Cr interleaved.
    assert_eq!(&nv12[8..], &[90, 240, 240, 110]);
}

#[test]
fn odd_dimensions_do_not_emit_a_partial_chroma_row() {
    let nv12 = bgra_to_nv12(&frame(3, 3, pixel(0, 0, 0)), 3, 3);

    assert_eq!(nv12.len(), 9 + 2);
}

#[test]
fn mismatched_length_is_rejected() {
    assert_eq!(bgra_to_nv12(&[0, 0, 0], 2, 2), Vec::<u8>::new());
    assert_eq!(bgra_to_nv12(&[], 0, 0), Vec::<u8>::new());
}
