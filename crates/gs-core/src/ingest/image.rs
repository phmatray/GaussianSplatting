//! The colour image of a capture frame: JPEG decoding and resampling.
//!
//! The phone sends 640x480 up to 1920x1440 colour pixels with a depth map of
//! 256x192: one depth pixel covers 2.5 to 7.5 colour pixels on a side. The
//! colour of a surface element must be the average over that footprint, not
//! one texel picked from it (which aliases on every texture and takes JPEG
//! block noise at full strength). The decoder is therefore asked for the
//! smallest of its DCT-domain scales (1/1, 1/2, 1/4, 1/8, each a proper
//! average of the full image) that still covers the wanted grid, which is
//! also several times faster than a full-size decode, and the result is
//! brought to the grid with an area filter.

use std::fmt;

use jpeg_decoder::{Decoder, PixelFormat};

/// Most pixels a colour image may announce. ARKit's video frames are
/// 1920x1440 (2.8 megapixels); this bounds what a hostile JPEG header can
/// make the decoder allocate.
pub const MAX_COLOR_PIXELS: u64 = 1 << 23;

/// Most scans a JPEG may have. A baseline JPEG has one to four, a
/// progressive one about ten; a picture with an embedded thumbnail a few
/// more. Every scan of a progressive JPEG is a pass over the whole picture
/// and can be as short as 25 bytes, so that a single frame of 8 MiB could
/// otherwise hold 300 000 of them and keep the decoder busy for minutes.
pub const MAX_JPEG_SCANS: usize = 64;

/// An sRGB image, 3 bytes per pixel, row-major from the top-left.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ColorImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Why a colour image cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// More than [`MAX_COLOR_PIXELS`], or an empty image.
    Size { width: u32, height: u32 },
    /// A JPEG flavour without a plain colour meaning (CMYK).
    Unsupported,
    /// More than [`MAX_JPEG_SCANS`] scans.
    Scans,
    /// Not a JPEG, or a damaged one.
    Decode(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size { width, height } => write!(f, "colour image of {width}x{height} pixels"),
            Self::Unsupported => f.write_str("unsupported JPEG pixel format"),
            Self::Scans => write!(f, "JPEG with more than {MAX_JPEG_SCANS} scans"),
            Self::Decode(detail) => write!(f, "JPEG: {detail}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Decodes a JPEG at the smallest scale the decoder offers that is still at
/// least `min_width` x `min_height` pixels (full size when the image is
/// smaller than that).
pub fn decode_jpeg(data: &[u8], min_width: u32, min_height: u32) -> Result<ColorImage, ImageError> {
    let fail = |e: jpeg_decoder::Error| ImageError::Decode(e.to_string());
    if has_too_many_scans(data) {
        return Err(ImageError::Scans);
    }
    let mut decoder = Decoder::new(data);
    decoder.read_info().map_err(fail)?;
    let info = decoder.info().ok_or_else(|| ImageError::Decode("no image header".into()))?;
    let (full_w, full_h) = (info.width as u32, info.height as u32);
    if full_w == 0 || full_h == 0 || full_w as u64 * full_h as u64 > MAX_COLOR_PIXELS {
        return Err(ImageError::Size { width: full_w, height: full_h });
    }
    let (request_w, request_h) = scale_request(full_w, full_h, min_width, min_height);
    let (width, height) = decoder.scale(request_w, request_h).map_err(fail)?;
    let (width, height) = (width as usize, height as usize);
    // Output plus the decoder's working planes; a baseline JPEG needs far
    // less, a progressive one keeps its coefficients (16 bits per sample).
    decoder.set_max_decoding_buffer_size(full_w as usize * full_h as usize * 8 + (1 << 20));
    let pixels = decoder.decode().map_err(fail)?;
    let rgb = match info.pixel_format {
        PixelFormat::RGB24 => pixels,
        PixelFormat::L8 => pixels.iter().flat_map(|&l| [l, l, l]).collect(),
        // Big-endian samples: the high byte is the 8-bit value.
        PixelFormat::L16 => pixels.chunks_exact(2).flat_map(|l| [l[0], l[0], l[0]]).collect(),
        PixelFormat::CMYK32 => return Err(ImageError::Unsupported),
    };
    if rgb.len() != width * height * 3 {
        return Err(ImageError::Decode("decoded size does not match the header".into()));
    }
    Ok(ColorImage { width: width as u32, height: height as u32, rgb })
}

/// Whether `data` holds more start-of-scan markers (`FF DA`) than
/// [`MAX_JPEG_SCANS`].
///
/// Every byte pair is looked at instead of walking the segments the way a
/// decoder would: entropy-coded data never contains the pair (an `FF` in it
/// is followed by `00` or a restart marker), so the count can only err on
/// the high side, by pairs that happen to occur in metadata, and no
/// malformed structure can hide a scan from it.
fn has_too_many_scans(data: &[u8]) -> bool {
    let pairs = data.iter().zip(data.iter().skip(1));
    pairs.filter(|&(&a, &b)| a == 0xFF && b == 0xDA).nth(MAX_JPEG_SCANS).is_some()
}

/// What to ask of [`Decoder::scale`] so that it picks the smallest of its
/// scales (eighths: 1, 2, 4, 8) at which the picture is still at least
/// `min_width` x `min_height`.
///
/// The decoder itself settles for the first scale that satisfies *either*
/// side, which halves a picture along the side it is already short on when
/// its proportions are not those of the grid it is wanted for (a 16:9
/// picture for a 4:3 depth map). So the scale is chosen here, and the
/// decoder is asked for exactly the longer side at that scale and for a
/// shorter side it can never reach.
fn scale_request(full_w: u32, full_h: u32, min_width: u32, min_height: u32) -> (u16, u16) {
    // The decoder's own rounding: `ceil(len * eighths / 8)`.
    let scaled = |len: u32, eighths: u32| (len * eighths).div_ceil(8);
    let eighths = [1, 2, 4].into_iter().find(|&e| scaled(full_w, e) >= min_width && scaled(full_h, e) >= min_height).unwrap_or(8);
    let exact = |len: u32| scaled(len, eighths).clamp(1, u16::MAX as u32) as u16;
    if full_w >= full_h { (exact(full_w), u16::MAX) } else { (u16::MAX, exact(full_h)) }
}

/// One output sample of a box filter: the source samples `first..` it
/// covers and their weights (which sum to one).
struct Span {
    first: usize,
    weights: Vec<f32>,
}

/// The spans that map `src` samples onto `dst` samples covering the same
/// extent.
fn spans(src: usize, dst: usize) -> Vec<Span> {
    let ratio = src as f64 / dst as f64;
    (0..dst)
        .map(|i| {
            let (lo, hi) = (i as f64 * ratio, (i + 1) as f64 * ratio);
            let first = (lo.floor() as usize).min(src - 1);
            let last = ((hi.ceil() as usize).max(first + 1)).min(src);
            let weights: Vec<f32> = (first..last)
                .map(|s| ((hi.min((s + 1) as f64) - lo.max(s as f64)) / (hi - lo)).max(0.0) as f32)
                .collect();
            let sum: f32 = weights.iter().sum();
            let weights = if sum > 0.0 { weights.iter().map(|w| w / sum).collect() } else { vec![1.0] };
            Span { first, weights }
        })
        .collect()
}

/// Resamples `image` to `width` x `height` with an area (box) filter: every
/// output pixel is the average of the source area it covers. Both images
/// span the same field of view. Returns an empty image for an empty input
/// or an empty target.
pub fn resample_area(image: &ColorImage, width: u32, height: u32) -> ColorImage {
    let (sw, sh, dw, dh) = (image.width as usize, image.height as usize, width as usize, height as usize);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || image.rgb.len() < sw * sh * 3 {
        return ColorImage::default();
    }
    if (sw, sh) == (dw, dh) {
        return image.clone();
    }
    // Horizontal pass into floats, then vertical.
    let columns = spans(sw, dw);
    let mut wide = vec![0.0f32; dw * sh * 3];
    for y in 0..sh {
        let row = &image.rgb[y * sw * 3..(y + 1) * sw * 3];
        let out = &mut wide[y * dw * 3..(y + 1) * dw * 3];
        for (x, span) in columns.iter().enumerate() {
            let mut acc = [0.0f32; 3];
            for (k, w) in span.weights.iter().enumerate() {
                let p = &row[(span.first + k) * 3..(span.first + k) * 3 + 3];
                acc[0] += w * p[0] as f32;
                acc[1] += w * p[1] as f32;
                acc[2] += w * p[2] as f32;
            }
            out[x * 3..x * 3 + 3].copy_from_slice(&acc);
        }
    }
    let rows = spans(sh, dh);
    let mut rgb = vec![0u8; dw * dh * 3];
    let mut acc = vec![0.0f32; dw * 3];
    for (y, span) in rows.iter().enumerate() {
        acc.fill(0.0);
        for (k, w) in span.weights.iter().enumerate() {
            let row = &wide[(span.first + k) * dw * 3..(span.first + k + 1) * dw * 3];
            for (a, v) in acc.iter_mut().zip(row) {
                *a += w * v;
            }
        }
        for (out, a) in rgb[y * dw * 3..(y + 1) * dw * 3].iter_mut().zip(&acc) {
            *out = (a + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    ColorImage { width, height, rgb }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> ColorImage {
        let rgb = (0..height).flat_map(|y| (0..width).map(move |x| (x, y))).flat_map(|(x, y)| f(x, y)).collect();
        ColorImage { width, height, rgb }
    }

    #[test]
    fn area_filter_averages_the_footprint() {
        // A one-pixel checkerboard is flat grey at half the size: a single
        // texel per output pixel would give black or white.
        let checker = image(64, 48, |x, y| if (x + y) % 2 == 0 { [255; 3] } else { [0; 3] });
        for (w, h) in [(32, 24), (16, 12), (8, 6)] {
            let small = resample_area(&checker, w, h);
            assert_eq!((small.width, small.height, small.rgb.len()), (w, h, (w * h * 3) as usize));
            assert!(small.rgb.iter().all(|&v| (127..=128).contains(&v)), "{w}x{h}");
        }
        // Non-integer ratios keep the mean and stay within the input range.
        let ramp = image(60, 45, |x, _| [(x * 4) as u8, 100, 200]);
        let small = resample_area(&ramp, 16, 12);
        let mean = |img: &ColorImage, c: usize| img.rgb.iter().skip(c).step_by(3).map(|&v| v as f64).sum::<f64>() / (img.rgb.len() / 3) as f64;
        assert!((mean(&small, 0) - mean(&ramp, 0)).abs() < 1.0);
        assert!(small.rgb.chunks(3).all(|p| p[1] == 100 && p[2] == 200));
        // Left to right still grows.
        assert!(small.rgb[0] < small.rgb[15 * 3]);
        // Enlarging repeats and blends, never invents values.
        let big = resample_area(&ramp, 240, 180);
        assert!(big.rgb.chunks(3).all(|p| p[0] <= 236 && p[1] == 100));
        assert_eq!(resample_area(&ramp, 60, 45), ramp);
        assert_eq!(resample_area(&ramp, 0, 4), ColorImage::default());
        assert_eq!(resample_area(&ColorImage::default(), 4, 4), ColorImage::default());
        let lying = ColorImage { width: 100, height: 100, rgb: vec![0; 30] };
        assert_eq!(resample_area(&lying, 4, 4), ColorImage::default());
    }

    #[test]
    fn spans_cover_the_source_exactly_once() {
        for (src, dst) in [(480, 256), (360, 192), (256, 256), (7, 3), (3, 7), (1, 5), (5, 1), (1920, 96)] {
            let spans = spans(src, dst);
            let mut coverage = vec![0.0f32; src];
            for span in &spans {
                assert!((span.weights.iter().sum::<f32>() - 1.0).abs() < 1e-4);
                assert!(span.first + span.weights.len() <= src);
                for (k, w) in span.weights.iter().enumerate() {
                    coverage[span.first + k] += w * src as f32 / dst as f32;
                }
            }
            if src >= dst {
                assert!(coverage.iter().all(|c| (c - 1.0).abs() < 1e-3), "{src} -> {dst}: {coverage:?}");
            }
        }
    }

    #[test]
    fn the_scale_covers_the_wanted_grid_on_both_sides() {
        // Replays the decoder's choice (`choose_idct_size` of jpeg-decoder
        // 0.3) on the request and checks what comes out.
        let scaled = |len: u32, e: u32| (len * e).div_ceil(8);
        let decoder_picks = |w: u32, h: u32, (rw, rh): (u16, u16)| {
            let e = [1, 2, 4].into_iter().find(|&e| scaled(w, e) >= rw as u32 || scaled(h, e) >= rh as u32).unwrap_or(8);
            (scaled(w, e), scaled(h, e))
        };
        for (w, h, min_w, min_h, expected) in [
            (1920, 1440, 256, 192, (480, 360)),
            (960, 720, 256, 192, (480, 360)),
            (640, 480, 256, 192, (320, 240)),
            (1920, 1080, 256, 192, (480, 270)),
            (1080, 1920, 256, 192, (270, 480)),
            (3, 500, 256, 192, (3, 500)),
            (500, 3, 256, 192, (500, 3)),
            (2048, 1536, 256, 192, (256, 192)),
            (1920, 1440, 1, 1, (240, 180)),
            (100, 100, 4000, 3000, (100, 100)),
            (1, 1, 256, 192, (1, 1)),
            (65535, 128, 16, 16, (8192, 16)),
        ] {
            let got = decoder_picks(w, h, scale_request(w, h, min_w, min_h));
            assert_eq!(got, expected, "{w}x{h} for {min_w}x{min_h}");
            // Never smaller than wanted unless the picture itself is.
            assert!((got.0 >= min_w && got.1 >= min_h) || got == (w, h));
        }
    }

    #[test]
    fn garbage_is_not_a_jpeg() {
        for bad in [&b""[..], b"\xFF", b"\xFF\xD8", b"\xFF\xD8\xFF\xD9", b"GSCF\x01\x00\x02\x00", &[0u8; 4096]] {
            assert!(matches!(decode_jpeg(bad, 16, 16), Err(ImageError::Decode(_))), "{bad:?}");
        }
    }
}
