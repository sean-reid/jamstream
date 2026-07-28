//! Decoded avatar pixels, capped and premultiplied before they reach the
//! renderer. Dimensions are checked from the header before the full decode
//! so a small file cannot decompress into a huge allocation.

use thiserror::Error;

/// Encoded size cap, checked before any decoding.
pub const MAX_BYTES: usize = 256 * 1024;
/// Decoded dimension cap per axis.
pub const MAX_DIM: u32 = 1024;

#[derive(Debug, Error)]
pub enum AvatarError {
    #[error("avatar is {0} bytes; the cap is {MAX_BYTES}")]
    TooManyBytes(usize),
    #[error("avatar decodes to {width}x{height}; the cap is {MAX_DIM}x{MAX_DIM}")]
    TooLarge { width: u32, height: u32 },
    #[error("avatar did not decode: {0}")]
    Decode(#[from] image::ImageError),
}

/// Premultiplied RGBA pixels, ready to blit through tiny-skia.
#[derive(Clone, Debug)]
pub struct AvatarImage {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl AvatarImage {
    /// Decodes PNG or JPEG bytes, enforcing [`MAX_BYTES`] and [`MAX_DIM`].
    pub fn from_bytes(bytes: &[u8]) -> Result<AvatarImage, AvatarError> {
        if bytes.len() > MAX_BYTES {
            return Err(AvatarError::TooManyBytes(bytes.len()));
        }
        let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(image::ImageError::IoError)?;
        let (width, height) = reader.into_dimensions()?;
        if over_cap(width, height) {
            return Err(AvatarError::TooLarge { width, height });
        }
        let mut rgba = image::load_from_memory(bytes)?.to_rgba8();
        // The header was only a cheap guard against a decompression bomb. The
        // pixels are the size: everything downstream indexes this buffer as
        // width*height*4, so the two must come from one place.
        let (width, height) = rgba.dimensions();
        if over_cap(width, height) {
            return Err(AvatarError::TooLarge { width, height });
        }
        for px in rgba.pixels_mut() {
            let a = px[3] as u16;
            for c in 0..3 {
                px[c] = ((px[c] as u16 * a + 127) / 255) as u8;
            }
        }
        Ok(AvatarImage {
            width,
            height,
            data: rgba.into_raw(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }

    /// Identity of the pixel buffer, used to detect roster changes cheaply.
    pub(crate) fn data_ptr(&self) -> usize {
        self.data.as_ptr() as usize
    }
}

/// Outside [`MAX_DIM`] on either axis, or empty.
fn over_cap(width: u32, height: u32) -> bool {
    width > MAX_DIM || height > MAX_DIM || width == 0 || height == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemberVisual, Renderer, Role, SceneConfig};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x * 3) as u8, (y * 5) as u8, 90, 255])
        });
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("png encodes");
        bytes
    }

    /// The invariant the renderer indexes by, held whatever the header said.
    #[test]
    fn the_buffer_always_backs_the_reported_dimensions() {
        for (w, h) in [(48, 32), (1, 1), (1024, 7)] {
            let av = AvatarImage::from_bytes(&png(w, h)).expect("decodes");
            assert_eq!((av.width(), av.height()), (w, h));
            assert_eq!(av.data.len(), (w * h * 4) as usize);
        }
    }

    /// The state the old two-source decode could hand the renderer: a header
    /// size that the pixel buffer does not back. Nothing can build this
    /// through `from_bytes` any more, so it is built by hand here, and the
    /// render thread has to survive it either way.
    #[test]
    fn a_buffer_shorter_than_its_dimensions_does_not_panic_the_renderer() {
        let liar = AvatarImage {
            width: 512,
            height: 512,
            data: vec![200u8; 8 * 8 * 4],
        };
        let cfg = SceneConfig::new("mismatched avatar");
        let (w, h) = (cfg.width, cfg.height);
        let mut renderer = Renderer::new(cfg);
        let members = vec![MemberVisual {
            name: "Ana Solari".to_owned(),
            avatar: Some(liar),
            level_peak: 0.4,
            level_rms: 0.2,
            connected: true,
            role: Role::Musician,
        }];
        let mut frame = vec![0u8; (w * h * 4) as usize];
        renderer.render(0, &members, 0, &mut frame);
        // The card fell back to the initials disc, so the frame is still a
        // frame: opaque everywhere, and not the empty stage.
        assert!(frame.chunks_exact(4).all(|px| px[3] == 255));
        let mut empty = Renderer::new(SceneConfig::new("mismatched avatar"));
        let mut bare = vec![0u8; (w * h * 4) as usize];
        empty.render(0, &[], 0, &mut bare);
        assert_ne!(frame, bare, "the member card was never drawn");
    }
}
