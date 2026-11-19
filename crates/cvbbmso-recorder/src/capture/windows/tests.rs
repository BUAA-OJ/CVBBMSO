use super::Desktop;

#[test]
#[ignore = "requires an attached display"]
fn captures_consecutive_frames_from_the_primary_display() {
    let mut desktop = Desktop::open().expect("open the primary display");

    let mut captured = 0;
    for _ in 0..40 {
        // A still desktop reports no change; that is not a fault.
        if let Some(frame) = desktop.next_frame(100).expect("capture") {
            assert!(frame.width > 0 && frame.height > 0, "empty frame");
            let expected = usize::try_from(frame.width).unwrap_or(0)
                * usize::try_from(frame.height).unwrap_or(0)
                * 4;
            assert_eq!(frame.pixels.len(), expected);
            captured += 1;
            if captured == 3 {
                break;
            }
        }
    }

    assert!(captured > 0, "no frame captured in four seconds");
}
