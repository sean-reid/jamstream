//! AvatarImage cap and decode behavior.

mod common;

use std::io::Cursor;

use common::solid_png;
use jamstream_broadcast::{AvatarError, AvatarImage, MAX_BYTES, MAX_DIM};

/// The byte cap belongs to the control plane: it is how big an avatar the
/// roster will carry, and this crate retypes the literal because it links
/// nothing of ours and cannot import it. Drift either way is a live defect. If
/// the transfer cap rose, the renderer would refuse pictures the session had
/// already delivered; if it fell, this crate would decode bytes no member can
/// send. A dev-dependency and this test are the only thing that can hold them
/// together (#232).
#[test]
fn the_byte_cap_is_the_control_planes_own_number() {
    assert_eq!(
        MAX_BYTES,
        jamstream_protocol::control::MAX_AVATAR_BYTES,
        "the renderer's avatar byte cap has drifted from the wire's"
    );
}

#[test]
fn oversized_bytes_error_before_decoding() {
    let bytes = vec![0u8; MAX_BYTES + 1];
    match AvatarImage::from_bytes(&bytes) {
        Err(AvatarError::TooManyBytes(n)) => assert_eq!(n, MAX_BYTES + 1),
        other => panic!("expected TooManyBytes, got {other:?}"),
    }
}

#[test]
fn oversized_dimensions_error_without_full_decode() {
    // A solid PNG at twice the dimension cap compresses far below the byte
    // cap, so only the dimension check can reject it.
    let over = MAX_DIM * 2;
    let bytes = solid_png([10, 20, 30], over);
    assert!(bytes.len() <= MAX_BYTES, "fixture must stay under byte cap");
    match AvatarImage::from_bytes(&bytes) {
        Err(AvatarError::TooLarge { width, height }) => {
            assert_eq!((width, height), (over, over));
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn garbage_bytes_error_cleanly() {
    let err = AvatarImage::from_bytes(&[0xde, 0xad, 0xbe, 0xef]).unwrap_err();
    assert!(matches!(err, AvatarError::Decode(_)), "got {err:?}");
}

#[test]
fn png_and_jpeg_at_the_caps_decode() {
    let png = solid_png([200, 100, 50], MAX_DIM);
    let av = AvatarImage::from_bytes(&png).expect("a png at the dimension cap");
    assert_eq!((av.width(), av.height()), (MAX_DIM, MAX_DIM));

    let img = image::RgbaImage::from_pixel(48, 32, image::Rgba([90, 140, 60, 255]));
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .to_rgb8()
        .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .expect("jpeg encodes");
    let av = AvatarImage::from_bytes(&jpeg).expect("jpeg decodes");
    assert_eq!((av.width(), av.height()), (48, 32));
}
