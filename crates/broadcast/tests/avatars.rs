//! AvatarImage cap and decode behavior.

mod common;

use std::io::Cursor;

use common::solid_png;
use jamstream_broadcast::{AvatarError, AvatarImage};

#[test]
fn oversized_bytes_error_before_decoding() {
    let bytes = vec![0u8; 256 * 1024 + 1];
    match AvatarImage::from_bytes(&bytes) {
        Err(AvatarError::TooManyBytes(n)) => assert_eq!(n, 256 * 1024 + 1),
        other => panic!("expected TooManyBytes, got {other:?}"),
    }
}

#[test]
fn oversized_dimensions_error_without_full_decode() {
    // A 2048x2048 solid PNG compresses far below the byte cap, so only the
    // dimension check can reject it.
    let bytes = solid_png([10, 20, 30], 2048);
    assert!(
        bytes.len() <= 256 * 1024,
        "fixture must stay under byte cap"
    );
    match AvatarImage::from_bytes(&bytes) {
        Err(AvatarError::TooLarge { width, height }) => {
            assert_eq!((width, height), (2048, 2048));
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
    let png = solid_png([200, 100, 50], 1024);
    let av = AvatarImage::from_bytes(&png).expect("1024x1024 png at the cap");
    assert_eq!((av.width(), av.height()), (1024, 1024));

    let img = image::RgbaImage::from_pixel(48, 32, image::Rgba([90, 140, 60, 255]));
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .to_rgb8()
        .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .expect("jpeg encodes");
    let av = AvatarImage::from_bytes(&jpeg).expect("jpeg decodes");
    assert_eq!((av.width(), av.height()), (48, 32));
}
