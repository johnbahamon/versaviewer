//! RGB(A) -> I420 conversion with area-average downscaling, multi-threaded.
//!
//! Screen content is mostly text and UI, so naive nearest-neighbour
//! downscaling would drop thin strokes. Each destination pixel averages the
//! box of source pixels it covers, and conversion + scaling happen in a single
//! pass over the source (no intermediate RGB buffer).
//!
//! Coefficients: BT.601 limited range, identical to OpenH264's own
//! `rgb2yuv`, so the decoder's YUV->RGB brings colours back unchanged.

use openh264::formats::YUVSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelOrder {
    Rgba,
    Bgra,
}

/// Borrowed 32-bit-per-pixel image.
#[derive(Debug, Clone, Copy)]
pub struct RawImage<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Bytes per row (>= width * 4).
    pub stride: usize,
    pub order: PixelOrder,
}

/// Largest even size that fits in `max_w` x `max_h` keeping the aspect ratio.
/// Never upscales.
pub fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64).min(1.0);
    let w = ((src_w as f64 * scale).round() as u32).max(2) & !1;
    let h = ((src_h as f64 * scale).round() as u32).max(2) & !1;
    (w, h)
}

/// Planar I420 frame owned by us, fed to the encoder without copies.
pub struct I420Buffer {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl I420Buffer {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width.is_multiple_of(2) && height.is_multiple_of(2), "I420 needs even dimensions");
        let (w, h) = (width as usize, height as usize);
        Self { width: w, height: h, data: vec![0; w * h * 3 / 2] }
    }

    pub fn dimensions_u32(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let y_len = self.width * self.height;
        let c_len = y_len / 4;
        let (y, rest) = self.data.split_at_mut(y_len);
        let (u, v) = rest.split_at_mut(c_len);
        (y, u, v)
    }

    /// Converts (and downscales if needed) `src` into this buffer.
    pub fn fill_from(&mut self, src: &RawImage<'_>) {
        let (dst_w, dst_h) = (self.width, self.height);
        let (src_w, src_h) = (src.width as usize, src.height as usize);
        assert!(src.stride >= src_w * 4 && src.data.len() >= src.stride * (src_h - 1) + src_w * 4);

        // Source span [start, end) covered by each destination column / row.
        let spans = |dst: usize, src_len: usize| -> Vec<(u32, u32)> {
            (0..dst)
                .map(|d| {
                    let a = d * src_len / dst;
                    let b = ((d + 1) * src_len / dst).max(a + 1).min(src_len);
                    (a as u32, b as u32)
                })
                .collect()
        };
        let x_spans = spans(dst_w, src_w);
        let y_spans = spans(dst_h, src_h);
        let (ri, bi) = match src.order {
            PixelOrder::Rgba => (0, 2),
            PixelOrder::Bgra => (2, 0),
        };

        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        let row_pairs = dst_h / 2;
        let pairs_per_thread = row_pairs.div_ceil(threads);
        let (y_plane, u_plane, v_plane) = self.planes_mut();

        std::thread::scope(|scope| {
            let y_chunks = y_plane.chunks_mut(pairs_per_thread * 2 * dst_w);
            let u_chunks = u_plane.chunks_mut(pairs_per_thread * dst_w / 2);
            let v_chunks = v_plane.chunks_mut(pairs_per_thread * dst_w / 2);
            for (t, ((y_out, u_out), v_out)) in y_chunks.zip(u_chunks).zip(v_chunks).enumerate() {
                let (x_spans, y_spans) = (&x_spans, &y_spans);
                scope.spawn(move || {
                    let mut rgb = vec![[0u32; 3]; dst_w * 2];
                    let first_pair = t * pairs_per_thread;
                    let pairs = y_out.len() / (2 * dst_w);
                    for p in 0..pairs {
                        // Average RGB for the two destination rows of this pair.
                        for r in 0..2 {
                            let (y0, y1) = y_spans[(first_pair + p) * 2 + r];
                            for (dx, &(x0, x1)) in x_spans.iter().enumerate() {
                                let mut acc = [0u32; 3];
                                for sy in y0..y1 {
                                    let row = &src.data[sy as usize * src.stride..];
                                    for sx in x0..x1 {
                                        let px = &row[sx as usize * 4..sx as usize * 4 + 4];
                                        acc[0] += u32::from(px[ri]);
                                        acc[1] += u32::from(px[1]);
                                        acc[2] += u32::from(px[bi]);
                                    }
                                }
                                let n = (y1 - y0) * (x1 - x0);
                                rgb[r * dst_w + dx] =
                                    [(acc[0] + n / 2) / n, (acc[1] + n / 2) / n, (acc[2] + n / 2) / n];
                            }
                        }
                        // Luma for both rows.
                        for r in 0..2 {
                            let out = &mut y_out[(p * 2 + r) * dst_w..(p * 2 + r + 1) * dst_w];
                            for (o, c) in out.iter_mut().zip(&rgb[r * dst_w..(r + 1) * dst_w]) {
                                *o = luma(c[0] as i32, c[1] as i32, c[2] as i32);
                            }
                        }
                        // Chroma from the 2x2 average.
                        let half = dst_w / 2;
                        for cx in 0..half {
                            let q = [
                                rgb[cx * 2],
                                rgb[cx * 2 + 1],
                                rgb[dst_w + cx * 2],
                                rgb[dst_w + cx * 2 + 1],
                            ];
                            let avg = |i: usize| ((q[0][i] + q[1][i] + q[2][i] + q[3][i] + 2) / 4) as i32;
                            let (r, g, b) = (avg(0), avg(1), avg(2));
                            u_out[p * half + cx] = clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
                            v_out[p * half + cx] = clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
                        }
                    }
                });
            }
        });
    }
}

#[inline]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16)
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

impl YUVSource for I420Buffer {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    fn u(&self) -> &[u8] {
        let y_len = self.width * self.height;
        &self.data[y_len..y_len + y_len / 4]
    }

    fn v(&self) -> &[u8] {
        let y_len = self.width * self.height;
        &self.data[y_len + y_len / 4..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| rgba).collect()
    }

    #[test]
    fn fit_within_keeps_aspect_and_never_upscales() {
        assert_eq!(fit_within(3840, 2160, 1920, 1200), (1920, 1080));
        assert_eq!(fit_within(2560, 1600, 1920, 1200), (1920, 1200));
        assert_eq!(fit_within(1280, 720, 1920, 1200), (1280, 720));
        assert_eq!(fit_within(1366, 767, 1920, 1200), (1366, 766));
    }

    #[test]
    fn matches_bt601_limited_for_solid_colours() {
        for (rgba, (y, u, v)) in [
            ([0, 0, 0, 255], (16, 128, 128)),
            ([255, 255, 255, 255], (235, 128, 128)),
            ([255, 0, 0, 255], (82, 90, 240)),
        ] {
            let src = solid(64, 48, rgba);
            let mut buf = I420Buffer::new(32, 24); // 2x downscale
            buf.fill_from(&RawImage {
                data: &src,
                width: 64,
                height: 48,
                stride: 64 * 4,
                order: PixelOrder::Rgba,
            });
            assert!(buf.y().iter().all(|&p| p == y), "Y for {rgba:?}");
            assert!(buf.u().iter().all(|&p| p == u), "U for {rgba:?}");
            assert!(buf.v().iter().all(|&p| p == v), "V for {rgba:?}");
        }
    }

    #[test]
    fn bgra_order_and_padded_stride() {
        // Pure blue in BGRA with 8 bytes of row padding.
        let (w, h, stride) = (8u32, 4u32, 8 * 4 + 8);
        let mut src = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                src[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[255, 0, 0, 255]);
            }
        }
        let mut buf = I420Buffer::new(8, 4);
        buf.fill_from(&RawImage { data: &src, width: w, height: h, stride, order: PixelOrder::Bgra });
        assert!(buf.y().iter().all(|&p| p == 41), "{:?}", buf.y());
        assert!(buf.u().iter().all(|&p| p == 240));
    }

    #[test]
    fn area_average_preserves_thin_lines() {
        // 1px white vertical line every 4px; 4x downscale must keep it as grey,
        // not drop it (nearest-neighbour could skip it entirely).
        let (w, h) = (64u32, 16u32);
        let src: Vec<u8> = (0..w * h)
            .flat_map(|i| if (i % w) % 4 == 3 { [255, 255, 255, 255] } else { [0, 0, 0, 255] })
            .collect();
        let mut buf = I420Buffer::new(16, 4);
        buf.fill_from(&RawImage { data: &src, width: w, height: h, stride: w as usize * 4, order: PixelOrder::Rgba });
        let expected = luma(64, 64, 64); // 1/4 white
        assert!(buf.y().iter().all(|&p| p == expected), "{:?}", buf.y());
    }
}
