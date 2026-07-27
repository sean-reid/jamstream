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
        if width > MAX_DIM || height > MAX_DIM || width == 0 || height == 0 {
            return Err(AvatarError::TooLarge { width, height });
        }
        let mut rgba = image::load_from_memory(bytes)?.to_rgba8();
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
