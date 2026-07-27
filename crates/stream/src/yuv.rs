//! RGBA to planar yuv420p, BT.709 limited range.
//!
//! The card renderer produces RGBA; x264 wants yuv420p. Converting here
//! instead of in ffmpeg costs a memory pass and saves 2.7x on the pipe:
//! 1280x720 is 3.7 MB per frame as RGBA and 1.4 MB as yuv420p, which at
//! 30 fps is 110 MB/s versus 41 MB/s through a FIFO. It also pins the color
//! conversion (BT.709, limited range, matching the flags we hand ffmpeg)
//! rather than leaving it to swscale's defaults.

/// Bytes in one yuv420p frame.
pub fn i420_len(width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + 2 * (w / 2) * (h / 2)
}

/// Converts `rgba` (width*height*4) into `out` (see [`i420_len`]).
///
/// Chroma is averaged over each 2x2 RGB block before conversion, so a
/// one-pixel accent on the near-black stage does not shimmer between frames.
///
/// # Panics
/// If the dimensions are odd or either buffer is the wrong length.
pub fn rgba_to_i420(rgba: &[u8], width: u32, height: u32, out: &mut [u8]) {
    assert!(
        width % 2 == 0 && height % 2 == 0,
        "yuv420p needs even dimensions, got {width}x{height}"
    );
    let (w, h) = (width as usize, height as usize);
    assert_eq!(rgba.len(), w * h * 4, "rgba buffer size");
    assert_eq!(out.len(), i420_len(width, height), "i420 buffer size");

    let (cw, ch) = (w / 2, h / 2);
    let (y_plane, chroma) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = chroma.split_at_mut(cw * ch);

    for y in 0..h {
        let row = &rgba[y * w * 4..(y + 1) * w * 4];
        let y_row = &mut y_plane[y * w..(y + 1) * w];
        for x in 0..w {
            let px = &row[x * 4..x * 4 + 3];
            y_row[x] = luma(px[0], px[1], px[2]);
        }
    }

    for cy in 0..ch {
        for cx in 0..cw {
            // Average the 2x2 block in RGB, then convert once.
            let mut sum = [0u32; 3];
            for dy in 0..2 {
                let base = ((cy * 2 + dy) * w + cx * 2) * 4;
                for dx in 0..2 {
                    let px = &rgba[base + dx * 4..base + dx * 4 + 3];
                    sum[0] += u32::from(px[0]);
                    sum[1] += u32::from(px[1]);
                    sum[2] += u32::from(px[2]);
                }
            }
            let (r, g, b) = ((sum[0] / 4) as u8, (sum[1] / 4) as u8, (sum[2] / 4) as u8);
            u_plane[cy * cw + cx] = chroma_b(r, g, b);
            v_plane[cy * cw + cx] = chroma_r(r, g, b);
        }
    }
}

// Fixed-point BT.709 limited-range coefficients, 1/256 scale. Y lands in
// 16..=235, chroma in 16..=240, which is what -color_range tv promises the
// decoder on the other side of the platform.
fn luma(r: u8, g: u8, b: u8) -> u8 {
    let v = 16 * 256 + 47 * i32::from(r) + 157 * i32::from(g) + 16 * i32::from(b);
    (v / 256).clamp(16, 235) as u8
}

fn chroma_b(r: u8, g: u8, b: u8) -> u8 {
    let v = 128 * 256 - 26 * i32::from(r) - 87 * i32::from(g) + 112 * i32::from(b);
    (v / 256).clamp(16, 240) as u8
}

fn chroma_r(r: u8, g: u8, b: u8) -> u8 {
    let v = 128 * 256 + 112 * i32::from(r) - 102 * i32::from(g) - 10 * i32::from(b);
    (v / 256).clamp(16, 240) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            buf.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        buf
    }

    fn convert(rgb: [u8; 3]) -> (u8, u8, u8) {
        let (w, h) = (4, 4);
        let src = solid(w, h, rgb);
        let mut out = vec![0u8; i420_len(w, h)];
        rgba_to_i420(&src, w, h, &mut out);
        let y = out[0];
        let u = out[(w * h) as usize];
        let v = out[(w * h + (w / 2) * (h / 2)) as usize];
        // Planes are uniform for a solid frame.
        assert!(out[..(w * h) as usize].iter().all(|&p| p == y));
        (y, u, v)
    }

    #[test]
    fn black_and_white_hit_the_limited_range_endpoints() {
        assert_eq!(convert([0, 0, 0]), (16, 128, 128));
        let (y, u, v) = convert([255, 255, 255]);
        assert_eq!(y, 235);
        assert!((i32::from(u) - 128).abs() <= 1, "u={u}");
        assert!((i32::from(v) - 128).abs() <= 1, "v={v}");
    }

    #[test]
    fn primaries_land_where_bt709_says() {
        // Reference BT.709 limited-range values, within a fixed-point step.
        for (rgb, want) in [
            ([255u8, 0, 0], (63u8, 102u8, 240u8)),
            ([0, 255, 0], (173, 42, 26)),
            ([0, 0, 255], (32, 240, 118)),
        ] {
            let (y, u, v) = convert(rgb);
            let close = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() <= 2;
            assert!(
                close(y, want.0) && close(u, want.1) && close(v, want.2),
                "{rgb:?} gave ({y},{u},{v}), wanted {want:?}"
            );
        }
    }

    #[test]
    fn chroma_averages_the_two_by_two_block() {
        // Left half red, right half black in a 2x2 frame: the single chroma
        // sample is the average of the block, not the top-left pixel.
        let mut src = Vec::new();
        for _ in 0..2 {
            src.extend_from_slice(&[255, 0, 0, 255]);
            src.extend_from_slice(&[0, 0, 0, 255]);
        }
        let mut out = vec![0u8; i420_len(2, 2)];
        rgba_to_i420(&src, 2, 2, &mut out);
        let half_red = convert([127, 0, 0]);
        assert_eq!(out[4], half_red.1);
        assert_eq!(out[5], half_red.2);
        // Luma keeps full resolution.
        assert_eq!(out[0], luma(255, 0, 0));
        assert_eq!(out[1], luma(0, 0, 0));
    }

    #[test]
    fn frame_size_matches_the_pipe_budget() {
        assert_eq!(i420_len(1280, 720), 1_382_400);
    }
}
