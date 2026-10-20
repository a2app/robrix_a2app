//! Small picture previews of attachments, decoded under strict limits and
//! always re-encoded, so no original bytes or metadata reach a script heap.

use std::io::{Cursor, Write};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageDecoder, ImageEncoder, ImageError, ImageFormat,
    ImageReader, Limits,
};

/// The longest preview edge, in pixels, when the caller names none.
pub const DEFAULT_EDGE: u32 = 256;
/// The smallest longest edge a caller may request.
pub const MIN_EDGE: u32 = 64;
/// The largest longest edge a caller may request.
pub const MAX_EDGE: u32 = 1024;

const MAX_SOURCE_EDGE: u32 = 16384;
const MAX_SOURCE_PIXELS: u64 = 32 * 1024 * 1024;
/// The decoded source buffer; a 16-bit RGBA picture at the pixel cap exceeds it.
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
/// One decode's worst case: decoded pixels, decoder buffers, input copies and the shrink.
const MAX_DECODE_MEMORY: u64 = 160 * 1024 * 1024;
/// Honored only by the PNG decoder's own buffers and GIF's off-screen frames.
const DECODER_LIMIT: u64 = 16 * 1024 * 1024;
/// Tables, rows and the encoder's buffers.
const FIXED_OVERHEAD: u64 = 4 * 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
const JPEG_QUALITY: u8 = 80;

/// A re-encoded preview: JPEG when the picture is opaque, PNG when alpha matters.
pub struct PreviewImage {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
    /// The decoded source's upright dimensions.
    pub source_width: u32,
    pub source_height: u32,
}

impl std::fmt::Debug for PreviewImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PreviewImage").field("mime_type", &self.mime_type)
            .field("size", &self.bytes.len()).field("width", &self.width).field("height", &self.height)
            .field("source_width", &self.source_width).field("source_height", &self.source_height).finish()
    }
}

/// Why a picture has no preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewError {
    /// Not JPEG, PNG, GIF or WebP.
    Unsupported,
    /// Larger than the decode limits.
    TooLarge,
    /// A supported format whose bytes do not decode.
    Invalid,
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "No picture preview for this attachment: its picture format is not supported.",
            Self::TooLarge => "No picture preview for this attachment: its picture is too large to preview.",
            Self::Invalid => "No picture preview for this attachment: its picture could not be decoded.",
        })
    }
}

impl std::error::Error for PreviewError {}

/// Whether a declared MIME type could be decoded here. Undeclared and
/// non-image types are left for the byte signature to decide.
pub fn may_decode(mime_type: Option<&str>) -> bool {
    let Some(mime_type) = mime_type.map(|mime| mime.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()) else { return true };
    !mime_type.starts_with("image/") || matches!(mime_type.as_str(),
        "image/jpeg" | "image/jpg" | "image/pjpeg" | "image/png" | "image/x-png" | "image/apng" | "image/gif" | "image/webp")
}

/// Decodes `bytes` (the first frame of an animation) and fits it within
/// `max_edge` pixels, never enlarging it, applying any EXIF orientation.
pub fn render(bytes: &[u8], max_edge: u32) -> Result<PreviewImage, PreviewError> {
    let max_edge = max_edge.clamp(MIN_EDGE, MAX_EDGE);
    let format = match image::guess_format(bytes) {
        Ok(format @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP)) => format,
        _ => return Err(PreviewError::Unsupported),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_EDGE);
    limits.max_image_height = Some(MAX_SOURCE_EDGE);
    limits.max_alloc = Some(DECODER_LIMIT);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 { return Err(PreviewError::Invalid); }
    let decoded_bytes = decoder.total_bytes();
    if u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS || decoded_bytes > MAX_DECODED_BYTES {
        return Err(PreviewError::TooLarge);
    }
    // A malformed EXIF block only loses the rotation, not the preview.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let are_axes_swapped = matches!(orientation, Orientation::Rotate90 | Orientation::Rotate270
        | Orientation::Rotate90FlipH | Orientation::Rotate270FlipH);
    let (source_width, source_height) = if are_axes_swapped { (height, width) } else { (width, height) };
    let (fit_width, fit_height) = fit_within((source_width, source_height), max_edge);
    let fit_pixels = u64::from(fit_width) * u64::from(fit_height);
    if peak_memory(format, (width, height), decoded_bytes, bytes.len() as u64, fit_pixels) > MAX_DECODE_MEMORY {
        return Err(PreviewError::TooLarge);
    }
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    // Resize before rotating, so only the small copy is ever rotated.
    let target = if are_axes_swapped { (fit_height, fit_width) } else { (fit_width, fit_height) };
    let mut preview = if is_opaque(&decoded) {
        DynamicImage::ImageRgb8(shrink(decoded, target).to_rgb8())
    } else {
        scale_by_alpha(&mut decoded, false);
        let mut small = shrink(decoded, target);
        scale_by_alpha(&mut small, true);
        DynamicImage::ImageRgba8(small.to_rgba8())
    };
    preview.apply_orientation(orientation);
    let (preview, bytes) = encode_within(preview, MAX_PREVIEW_BYTES)?;
    Ok(PreviewImage {
        mime_type: if matches!(preview, DynamicImage::ImageRgba8(_)) { "image/png" } else { "image/jpeg" },
        width: preview.width(), height: preview.height(), bytes, source_width, source_height,
    })
}

fn decode_error(error: ImageError) -> PreviewError {
    match error {
        ImageError::Limits(_) => PreviewError::TooLarge,
        ImageError::Unsupported(_) => PreviewError::Unsupported,
        _ => PreviewError::Invalid,
    }
}

/// One decode's worst-case memory, from its header. The JPEG and WebP decoders ignore
/// `Limits`, so their own buffers, as their sources allocate them, are counted here.
fn peak_memory(format: ImageFormat, (width, height): (u32, u32), decoded_bytes: u64, input_bytes: u64, fit_pixels: u64) -> u64 {
    let pixels = u64::from(width) * u64::from(height);
    let decoder = match format {
        // Up to four 16-bit coefficient planes over the MCU-padded frame, and copies of the input.
        ImageFormat::Jpeg => 2 * input_bytes + 8 * u64::from(width.next_multiple_of(32)) * u64::from(height.next_multiple_of(32)),
        // An animation's RGBA canvas and frame, or a lossy frame's planes and alpha, and copies of the input.
        ImageFormat::WebP => 2 * input_bytes + 8 * pixels,
        // The LZW index buffer; an off-screen first frame's RGBA buffer is held to `DECODER_LIMIT`.
        ImageFormat::Gif => pixels.max(DECODER_LIMIT + DECODER_LIMIT / 4),
        _ => DECODER_LIMIT,
    };
    // The box pre-shrink and the target in the source's pixel type, plus the resize's f32 rows.
    let shrink = (5 * (decoded_bytes / pixels) + 32) * fit_pixels;
    decoded_bytes + FIXED_OVERHEAD + decoder.max(shrink)
}

/// Fits `(width, height)` within a square of `edge` pixels, never enlarging it.
fn fit_within((width, height): (u32, u32), edge: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= edge { return (width, height); }
    let scale = |side: u32| ((u64::from(side) * u64::from(edge) + u64::from(long) / 2) / u64::from(long)).max(1) as u32;
    (scale(width), scale(height))
}

/// Shrinks in the source's own pixel type, so no full-size copy is ever converted. A box
/// pre-shrink to about twice the target keeps the Catmull-Rom pass, and its buffers, small.
fn shrink(image: DynamicImage, (width, height): (u32, u32)) -> DynamicImage {
    if image.dimensions() == (width, height) { return image; }
    let (source_width, source_height) = image.dimensions();
    let image = if source_width.max(source_height) > 2 * width.max(height) {
        let reduced = image.thumbnail_exact((2 * width).min(source_width), (2 * height).min(source_height));
        drop(image);
        reduced
    } else { image };
    image.resize_exact(width, height, FilterType::CatmullRom)
}

/// Whether no pixel is see-through, as for every picture without an alpha channel.
fn is_opaque(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageLumaA8(buffer) => buffer.pixels().all(|pixel| pixel[1] == u8::MAX),
        DynamicImage::ImageRgba8(buffer) => buffer.pixels().all(|pixel| pixel[3] == u8::MAX),
        DynamicImage::ImageLumaA16(buffer) => buffer.pixels().all(|pixel| pixel[1] == u16::MAX),
        DynamicImage::ImageRgba16(buffer) => buffer.pixels().all(|pixel| pixel[3] == u16::MAX),
        other => !other.color().has_alpha(),
    }
}

/// Premultiplies color by alpha in place (or, `is_reverse`, divides it back out): filtering
/// premultiplied color keeps transparent pixels' hidden color out of the edges.
fn scale_by_alpha(image: &mut DynamicImage, is_reverse: bool) {
    match image {
        DynamicImage::ImageLumaA8(buffer) => scale_samples(buffer, 2, u8::MAX.into(), is_reverse),
        DynamicImage::ImageRgba8(buffer) => scale_samples(buffer, 4, u8::MAX.into(), is_reverse),
        DynamicImage::ImageLumaA16(buffer) => scale_samples(buffer, 2, u16::MAX.into(), is_reverse),
        DynamicImage::ImageRgba16(buffer) => scale_samples(buffer, 4, u16::MAX.into(), is_reverse),
        _ => {}
    }
}

fn scale_samples<T: Copy + Into<u64> + TryFrom<u64>>(samples: &mut [T], channels: usize, max: u64, is_reverse: bool) {
    for pixel in samples.chunks_exact_mut(channels) {
        let alpha: u64 = pixel[channels - 1].into();
        for sample in &mut pixel[..channels - 1] {
            let value: u64 = (*sample).into();
            let scaled = match (is_reverse, alpha) {
                (false, _) => (value * alpha + max / 2) / max,
                (true, 0) => 0,
                (true, _) => ((value * max + alpha / 2) / alpha).min(max),
            };
            *sample = T::try_from(scaled).unwrap_or(*sample);
        }
    }
}

/// Encodes within `cap` bytes, shrinking pathological (noise-like) pictures until they fit.
fn encode_within(mut image: DynamicImage, cap: usize) -> Result<(DynamicImage, Vec<u8>), PreviewError> {
    loop {
        if let Some(bytes) = encode(&image, cap)? { return Ok((image, bytes)); }
        if image.width() == 1 && image.height() == 1 { return Err(PreviewError::TooLarge); }
        image = image.resize_exact((image.width() * 3 / 4).max(1), (image.height() * 3 / 4).max(1), FilterType::Triangle);
    }
}

fn encode(image: &DynamicImage, cap: usize) -> Result<Option<Vec<u8>>, PreviewError> {
    let mut writer = BoundedWriter { bytes: Vec::new(), cap, exceeded: false };
    let result = match image {
        DynamicImage::ImageRgba8(rgba) => PngEncoder::new_with_quality(&mut writer, CompressionType::Best, PngFilter::Adaptive)
            .write_image(rgba.as_raw(), rgba.width(), rgba.height(), ExtendedColorType::Rgba8),
        DynamicImage::ImageRgb8(rgb) => JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY)
            .write_image(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8),
        other => {
            let rgb = other.to_rgb8();
            JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY)
                .write_image(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
        }
    };
    if writer.exceeded { return Ok(None); }
    result.map_err(|_| PreviewError::Invalid)?;
    Ok(Some(writer.bytes))
}

struct BoundedWriter { bytes: Vec<u8>, cap: usize, exceeded: bool }

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.cap {
            self.exceeded = true;
            return Err(std::io::Error::other("preview size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, ImageBuffer, LumaA, Rgb, RgbImage, Rgba, RgbaImage};

    const RED: [u8; 3] = [230, 30, 40];
    const BLUE: [u8; 3] = [20, 40, 220];

    fn jpeg(image: &RgbImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 95).encode_image(image).unwrap();
        bytes
    }

    fn png(image: &DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png).unwrap();
        bytes
    }

    /// Left half red, right half blue.
    fn halves(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, _| Rgb(if x < width / 2 { RED } else { BLUE }))
    }

    fn close(actual: &[u8], expected: &[u8]) -> bool {
        actual.iter().zip(expected).all(|(actual, expected)| actual.abs_diff(*expected) < 24)
    }

    fn decoded(preview: &PreviewImage) -> DynamicImage {
        let format = image::guess_format(&preview.bytes).unwrap();
        assert_eq!(format.to_mime_type(), preview.mime_type);
        let image = image::load_from_memory(&preview.bytes).unwrap();
        assert_eq!((image.width(), image.height()), (preview.width, preview.height));
        image
    }

    /// Recomputes a PNG chunk's CRC after its header fields were patched.
    fn patch_png_header(bytes: &mut [u8], width: u32, height: u32, bit_depth: u8, color_type: u8) {
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes[24] = bit_depth;
        bytes[25] = color_type;
        let mut crc = u32::MAX;
        for byte in &bytes[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 { crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1)); }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
    }

    #[test]
    fn large_opaque_pictures_fit_the_requested_edge_as_jpeg() {
        let source = halves(1600, 1200);
        for (bytes, max_edge) in [(jpeg(&source), DEFAULT_EDGE), (png(&DynamicImage::ImageRgb8(source.clone())), MIN_EDGE)] {
            let preview = render(&bytes, max_edge).unwrap();
            assert_eq!(preview.mime_type, "image/jpeg");
            assert_eq!((preview.width, preview.height), (max_edge, max_edge * 3 / 4));
            assert_eq!((preview.source_width, preview.source_height), (1600, 1200));
            assert!(preview.bytes.len() < 16 * 1024, "a preview must be compact");
            let pixels = decoded(&preview).to_rgb8();
            assert!(close(&pixels.get_pixel(preview.width / 4, preview.height / 2).0, &RED));
            assert!(close(&pixels.get_pixel(preview.width * 3 / 4, preview.height / 2).0, &BLUE));
        }
    }

    #[test]
    fn small_pictures_are_never_enlarged_and_extreme_shapes_keep_one_pixel() {
        let preview = render(&png(&DynamicImage::ImageRgb8(halves(100, 50))), MAX_EDGE).unwrap();
        assert_eq!((preview.width, preview.height, preview.source_width, preview.source_height), (100, 50, 100, 50));
        assert!(close(&decoded(&preview).to_rgb8().get_pixel(10, 25).0, &RED));
        let preview = render(&png(&DynamicImage::ImageRgb8(halves(4000, 3))), DEFAULT_EDGE).unwrap();
        assert_eq!((preview.width, preview.height), (256, 1));
        assert_eq!(fit_within((3, 4000), 256), (1, 256));
        assert_eq!(fit_within((1025, 1024), 1024), (1024, 1023));
        assert_eq!(fit_within((64, 64), 64), (64, 64));
    }

    #[test]
    fn translucent_pictures_stay_png_with_alpha_and_clean_edges() {
        // Hidden black under transparent pixels must not darken the red edge.
        let source = RgbaImage::from_fn(800, 800, |x, y| {
            if (200..600).contains(&x) && (200..600).contains(&y) { Rgba([RED[0], RED[1], RED[2], 255]) } else { Rgba([0, 0, 0, 0]) }
        });
        let preview = render(&png(&DynamicImage::ImageRgba8(source)), 100).unwrap();
        assert_eq!(preview.mime_type, "image/png");
        assert_eq!((preview.width, preview.height), (100, 100));
        let pixels = decoded(&preview).to_rgba8();
        assert_eq!(pixels.get_pixel(50, 50).0[3], 255);
        assert_eq!(pixels.get_pixel(5, 5).0, [0, 0, 0, 0]);
        for pixel in pixels.pixels().filter(|pixel| pixel[3] >= 16) {
            assert!(pixel[0] > 200 && pixel[2] < 80, "edge pixel {:?} was darkened by hidden color", pixel.0);
        }
    }

    #[test]
    fn fully_opaque_alpha_channels_encode_as_jpeg() {
        let source = RgbaImage::from_fn(300, 200, |x, _| Rgba(if x < 150 { [RED[0], RED[1], RED[2], 255] } else { [BLUE[0], BLUE[1], BLUE[2], 255] }));
        let preview = render(&png(&DynamicImage::ImageRgba8(source)), 128).unwrap();
        assert_eq!(preview.mime_type, "image/jpeg");
        assert_eq!((preview.width, preview.height), (128, 85));
    }

    #[test]
    fn exif_orientation_is_applied_to_the_preview_and_its_source_size() {
        let mut original = jpeg(&halves(320, 160));
        // Little-endian TIFF orientation 6 means rotate clockwise 90 degrees.
        let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(exif);
        original.splice(2..2, segment);
        let preview = render(&original, 64).unwrap();
        assert_eq!((preview.source_width, preview.source_height), (160, 320));
        assert_eq!((preview.width, preview.height), (32, 64));
        let pixels = decoded(&preview).to_rgb8();
        assert!(close(&pixels.get_pixel(16, 12).0, &RED), "the red left half must turn to the top");
        assert!(close(&pixels.get_pixel(16, 52).0, &BLUE));
    }

    #[test]
    fn gif_and_webp_originals_decode_and_animations_use_their_first_frame() {
        let first = RgbaImage::from_pixel(96, 64, Rgba([RED[0], RED[1], RED[2], 255]));
        let second = RgbaImage::from_pixel(96, 64, Rgba([BLUE[0], BLUE[1], BLUE[2], 255]));
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode_frames([Frame::new(first.clone()), Frame::new(second)]).unwrap();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(first.as_raw(), 96, 64, ExtendedColorType::Rgba8).unwrap();
        for bytes in [gif, webp] {
            let preview = render(&bytes, MIN_EDGE).unwrap();
            assert_eq!(preview.mime_type, "image/jpeg");
            assert_eq!((preview.width, preview.height), (64, 43));
            assert!(close(&decoded(&preview).to_rgb8().get_pixel(32, 20).0, &RED));
        }
        let mut transparent = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut transparent)
            .encode(RgbaImage::from_fn(32, 32, |x, _| Rgba(if x < 16 { [0, 0, 0, 0] } else { [RED[0], RED[1], RED[2], 255] })).as_raw(),
                32, 32, ExtendedColorType::Rgba8).unwrap();
        assert_eq!(render(&transparent, MIN_EDGE).unwrap().mime_type, "image/png");
    }

    #[test]
    fn bomb_dimensions_are_refused_before_any_pixel_buffer_is_allocated() {
        let small = png(&DynamicImage::ImageRgb8(halves(8, 8)));
        for (width, height, bit_depth, color_type) in [
            (20_000, 20_000, 8, 2),  // past the per-edge limit
            (8_000, 8_000, 8, 2),    // within each edge, past the pixel cap
            (6_000, 5_000, 16, 6),   // within the pixel cap, past the decoded-bytes cap
        ] {
            let mut bomb = small.clone();
            patch_png_header(&mut bomb, width, height, bit_depth, color_type);
            assert_eq!(render(&bomb, DEFAULT_EDGE).unwrap_err(), PreviewError::TooLarge, "{width}x{height}");
        }
        let mut jpeg = jpeg(&halves(16, 16));
        let start = jpeg.windows(2).position(|marker| marker == [0xff, 0xc0]).unwrap();
        jpeg[start + 5..start + 9].copy_from_slice(&[0x17, 0x70, 0x17, 0x70]);  // 6000x6000
        assert_eq!(render(&jpeg, DEFAULT_EDGE).unwrap_err(), PreviewError::TooLarge);
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif).encode(&[0; 4], 1, 1, ExtendedColorType::Rgba8).unwrap();
        gif[6..10].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);  // a 65535x65535 logical screen
        assert_eq!(render(&gif, DEFAULT_EDGE).unwrap_err(), PreviewError::TooLarge);
    }

    /// The offset of a GIF's first image descriptor, past its screen descriptor, palette and extensions.
    fn gif_image_descriptor(gif: &[u8]) -> usize {
        let mut at = 13 + if gif[10] & 0x80 != 0 { 3 << ((gif[10] & 7) + 1) } else { 0 };
        while gif[at] == 0x21 {
            at += 2;
            while gif[at] != 0 { at += usize::from(gif[at]) + 1; }
            at += 1;
        }
        assert_eq!(gif[at], 0x2c);
        at
    }

    #[test]
    fn decoders_that_ignore_allocation_limits_are_refused_from_their_headers() {
        // Each header passes the pixel and decoded-byte caps; decoding them took up to 355 MiB.
        let mut jpeg = jpeg(&halves(16, 16));
        let start = jpeg.windows(2).position(|marker| marker == [0xff, 0xc0]).unwrap();
        jpeg[start + 5..start + 9].copy_from_slice(&[0x10, 0x00, 0x20, 0x00]);  // 8192x4096
        assert_eq!(render(&jpeg, DEFAULT_EDGE).unwrap_err(), PreviewError::TooLarge);
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&[200; 3 * 16 * 16], 16, 16, ExtendedColorType::Rgb8).unwrap();
        assert_eq!((&webp[12..16], webp[20]), (&b"VP8L"[..], 0x2f));
        let header = u32::from_le_bytes(webp[21..25].try_into().unwrap()) & !0x0fff_ffff | 8191 | (4095 << 14);
        webp[21..25].copy_from_slice(&header.to_le_bytes());  // 8192x4096
        assert_eq!(render(&webp, DEFAULT_EDGE).unwrap_err(), PreviewError::TooLarge);
        // A first frame far larger than its 2x2 screen is held to the decoder limit.
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif).encode(&[0; 16], 2, 2, ExtendedColorType::Rgba8).unwrap();
        let descriptor = gif_image_descriptor(&gif);
        gif[descriptor + 5..descriptor + 9].copy_from_slice(&[0x70, 0x17, 0x70, 0x17]);  // 6000x6000
        assert_eq!(render(&gif, DEFAULT_EDGE).unwrap_err(), PreviewError::TooLarge);
    }

    #[test]
    fn decoder_buffers_and_the_shrink_count_against_one_decode_budget() {
        const MIB: u64 = 1024 * 1024;
        let peak = |format, (width, height): (u32, u32), bytes_per_pixel: u64, input_bytes: u64, edge: u32| {
            let (fit_width, fit_height) = fit_within((width, height), edge);
            peak_memory(format, (width, height), u64::from(width) * u64::from(height) * bytes_per_pixel, input_bytes,
                u64::from(fit_width) * u64::from(fit_height))
        };
        // A 12 MP photo previews however it is coded; larger colour JPEGs and WebPs need their thumbnail.
        assert!(peak(ImageFormat::Jpeg, (4032, 3024), 3, 8 * MIB, MAX_EDGE) <= MAX_DECODE_MEMORY);
        assert!(peak(ImageFormat::WebP, (4000, 3000), 4, 8 * MIB, MAX_EDGE) <= MAX_DECODE_MEMORY);
        assert!(peak(ImageFormat::Jpeg, (4608, 3456), 3, MIB, DEFAULT_EDGE) > MAX_DECODE_MEMORY);
        for (format, bytes_per_pixel) in [(ImageFormat::Jpeg, 3), (ImageFormat::WebP, 3), (ImageFormat::WebP, 4), (ImageFormat::Gif, 4)] {
            assert!(peak(format, (8192, 4096), bytes_per_pixel, 4096, DEFAULT_EDGE) > MAX_DECODE_MEMORY, "{format:?}");
        }
        // PNG decodes straight into its output, so a picture at the decoded-bytes cap fits a small preview,
        assert!(peak(ImageFormat::Png, (8192, 4096), 4, MIB, DEFAULT_EDGE) <= MAX_DECODE_MEMORY);
        assert!(peak(ImageFormat::Png, (4096, 4096), 8, MIB, DEFAULT_EDGE) <= MAX_DECODE_MEMORY);
        // but not a large one, whose shrink buffers come on top.
        assert!(peak(ImageFormat::Png, (4096, 4096), 8, MIB, MAX_EDGE) > MAX_DECODE_MEMORY);
    }

    #[test]
    fn sixteen_bit_and_grey_pictures_shrink_in_their_own_pixel_type() {
        // A translucent 16-bit grey square on a transparent ground keeps clean edges.
        let square = ImageBuffer::<LumaA<u16>, Vec<u16>>::from_fn(400, 400, |x, y| {
            if (100..300).contains(&x) && (100..300).contains(&y) { LumaA([50_000, u16::MAX]) } else { LumaA([0, 0]) }
        });
        let preview = render(&png(&DynamicImage::ImageLumaA16(square.clone())), 100).unwrap();
        assert_eq!((preview.mime_type, preview.width, preview.height), ("image/png", 100, 100));
        let pixels = decoded(&preview).to_rgba8();
        assert_eq!(pixels.get_pixel(5, 5).0[3], 0);
        assert!(close(&pixels.get_pixel(50, 50).0, &[194, 194, 194, 255]));
        for pixel in pixels.pixels().filter(|pixel| pixel[3] >= 16) {
            assert!(pixel[0] > 170, "edge pixel {:?} was darkened by hidden color", pixel.0);
        }
        assert!(matches!(shrink(DynamicImage::ImageLumaA16(square), (50, 50)), DynamicImage::ImageLumaA16(_)),
            "the full-size source is never widened to 8-bit RGBA");
        // Opaque 16-bit colour and 8-bit grey become JPEG.
        let wide = |value: u8| u16::from(value) * 257;
        let colour = ImageBuffer::<Rgb<u16>, Vec<u16>>::from_fn(300, 200, |x, _| {
            let [r, g, b] = if x < 150 { RED } else { BLUE };
            Rgb([wide(r), wide(g), wide(b)])
        });
        let preview = render(&png(&DynamicImage::ImageRgb16(colour)), 128).unwrap();
        assert_eq!((preview.mime_type, preview.width, preview.height), ("image/jpeg", 128, 85));
        let pixels = decoded(&preview).to_rgb8();
        assert!(close(&pixels.get_pixel(20, 40).0, &RED) && close(&pixels.get_pixel(108, 40).0, &BLUE));
        let grey = DynamicImage::ImageLuma8(image::GrayImage::from_fn(200, 100, |x, _| image::Luma([if x < 100 { 40 } else { 220 }])));
        let preview = render(&png(&grey), 64).unwrap();
        assert_eq!((preview.mime_type, preview.width, preview.height), ("image/jpeg", 64, 32));
        assert!(close(&decoded(&preview).to_rgb8().get_pixel(8, 16).0, &[40, 40, 40]));
    }

    #[test]
    fn garbage_truncated_and_unsupported_bytes_have_no_preview() {
        assert_eq!(render(b"", DEFAULT_EDGE).unwrap_err(), PreviewError::Unsupported);
        assert_eq!(render(b"definitely not a picture", DEFAULT_EDGE).unwrap_err(), PreviewError::Unsupported);
        assert_eq!(render(b"BM\x36\0\0\0\0\0\0\0\x36\0\0\0", DEFAULT_EDGE).unwrap_err(), PreviewError::Unsupported, "BMP is not decoded");
        assert_eq!(render(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>", DEFAULT_EDGE).unwrap_err(), PreviewError::Unsupported);
        let mut corrupt = b"\x89PNG\r\n\x1a\n".to_vec();
        corrupt.extend_from_slice(&[0x5a; 64]);
        assert_eq!(render(&corrupt, DEFAULT_EDGE).unwrap_err(), PreviewError::Invalid);
        let whole = png(&DynamicImage::ImageRgb8(halves(64, 64)));
        assert_eq!(render(&whole[..whole.len() / 2], DEFAULT_EDGE).unwrap_err(), PreviewError::Invalid);
    }

    #[test]
    fn noise_shrinks_until_its_encoding_fits_the_byte_cap() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let noise = RgbaImage::from_fn(128, 128, |_, _| {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            Rgba(seed.to_le_bytes()[..4].try_into().unwrap())
        });
        let (image, bytes) = encode_within(DynamicImage::ImageRgba8(noise), 8 * 1024).unwrap();
        assert!(bytes.len() <= 8 * 1024);
        assert!(image.width() < 128 && image.width() == image.height());
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), image.width());
    }

    #[test]
    fn declared_types_skip_only_pictures_this_decoder_cannot_read() {
        for mime in [None, Some("image/jpeg"), Some("image/PNG"), Some("image/gif; charset=binary"), Some("image/webp"),
            Some("application/octet-stream"), Some("video/mp4")] {
            assert!(may_decode(mime), "{mime:?}");
        }
        for mime in ["image/heic", "image/avif", "image/svg+xml", "image/tiff", "image/bmp"] {
            assert!(!may_decode(Some(mime)), "{mime}");
        }
    }
}
