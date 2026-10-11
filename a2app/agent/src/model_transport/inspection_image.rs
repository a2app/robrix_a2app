//! Bounded provider inspection copies. Host-owned originals never change.
use std::{borrow::Cow, io::{Cursor, Write}};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};

const MAX_INSPECTION_BYTES: usize = 3 * 1024 * 1024;
const MAX_SOURCE_DIMENSION: u32 = 16384;
const MAX_SOURCE_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PROVIDER_DIMENSION: u32 = 8000;
const INSPECTION_LONG_EDGE: u32 = 1568;
const INSPECTION_PIXELS: u64 = 1_200_000;

#[derive(Debug)]
pub(super) struct InspectionImage<'a> {
    pub bytes: Cow<'a, [u8]>,
    pub resized: Option<(u32, u32)>,
}

pub(super) fn prepare<'a>(bytes: &'a [u8], mime: &str) -> Result<InspectionImage<'a>, String> {
    let format = match mime {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        _ => return Err("This inspection codec supports only JPEG and PNG.".into()),
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| "Invalid image or image dimensions exceed the inspection limit.")?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width).checked_mul(u64::from(height)).is_none_or(|pixels| pixels > MAX_SOURCE_PIXELS)
        || decoder.total_bytes() > MAX_DECODED_BYTES
    { return Err("Image exceeds the inspection limit of 32 million pixels or 128 MiB decoded data.".into()); }
    if bytes.len() <= MAX_INSPECTION_BYTES && width <= MAX_PROVIDER_DIMENSION && height <= MAX_PROVIDER_DIMENSION {
        return Ok(InspectionImage { bytes: Cow::Borrowed(bytes), resized: None });
    }
    let orientation = decoder.orientation().map_err(|_| "Could not read the image orientation.")?;
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|_| "Could not decode the inspection image within its memory limit.")?;
    decoded.apply_orientation(orientation);
    let (mut width, mut height) = inspection_dimensions(decoded.width(), decoded.height());
    loop {
        let resized = decoded.resize_exact(width, height, image::imageops::FilterType::Triangle);
        if let Some(encoded) = encode_bounded(&resized, format)? {
            return Ok(InspectionImage { bytes: Cow::Owned(encoded), resized: Some((width, height)) });
        }
        // Lossless PNG preserves alpha. If its encoded copy is still too
        // large, reduce the dimensions instead of changing its background.
        if width == 1 && height == 1 { return Err("Could not fit the inspection image within its encoded size limit.".into()); }
        width = (width * 3 / 4).max(1);
        height = (height * 3 / 4).max(1);
    }
}

fn inspection_dimensions(width: u32, height: u32) -> (u32, u32) {
    let scale = (f64::from(INSPECTION_LONG_EDGE) / f64::from(width.max(height)))
        .min((INSPECTION_PIXELS as f64 / (f64::from(width) * f64::from(height))).sqrt()).min(1.0);
    ((f64::from(width) * scale).floor().max(1.0) as u32,
        (f64::from(height) * scale).floor().max(1.0) as u32)
}

struct BoundedWriter { bytes: Vec<u8>, exceeded: bool }
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_INSPECTION_BYTES {
            self.exceeded = true;
            return Err(std::io::Error::other("inspection image size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn encode_bounded(image: &DynamicImage, format: ImageFormat) -> Result<Option<Vec<u8>>, String> {
    let mut writer = BoundedWriter { bytes: Vec::new(), exceeded: false };
    let result = match format {
        ImageFormat::Jpeg => image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 85).encode_image(&image.to_rgb8()),
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut writer).write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
        }
        _ => unreachable!(),
    };
    if writer.exceeded { return Ok(None); }
    result.map_err(|_| "Could not encode the inspection image.".to_string())?;
    Ok(Some(writer.bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{parts, Format};
    use crate::tool_output::ModelAttachment;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use image::{Rgb, RgbImage, Rgba, RgbaImage};
    use sha2::{Digest, Sha256};

    fn colors() -> [[u8; 3]; 4] { [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]] }

    fn noise(seed: &mut u64) -> [u8; 4] {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        seed.to_le_bytes()[2..6].try_into().unwrap()
    }

    fn marked_pixel(x: u32, y: u32, width: u32, height: u32) -> Option<[u8; 3]> {
        let quadrant = usize::from(x >= width / 2) + 2 * usize::from(y >= height / 2);
        let center_x = if x < width / 2 { width / 4 } else { width * 3 / 4 };
        let center_y = if y < height / 2 { height / 4 } else { height * 3 / 4 };
        (x.abs_diff(center_x) < 32 && y.abs_diff(center_y) < 32).then(|| colors()[quadrant])
    }

    fn large_jpeg() -> Vec<u8> {
        let mut seed = 0x8193ac24b857d602;
        let image = RgbImage::from_fn(2048, 1536, |x, y| Rgb(marked_pixel(x, y, 2048, 1536).unwrap_or_else(|| {
            let pixel = noise(&mut seed); [pixel[0], pixel[1], pixel[2]]
        })));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100).encode_image(&image).unwrap();
        assert!(bytes.len() > MAX_INSPECTION_BYTES && bytes.len() < crate::tool_output::MAX_ATTACHMENT_BYTES);
        bytes
    }

    #[test]
    fn small_native_images_are_byte_exact_and_keep_their_originals() {
        for (mime, name, bytes) in [
            ("image/png", "small.png", include_bytes!("../../tests/fixtures/vision-colors.png").as_slice()),
            ("image/jpeg", "small.jpg", include_bytes!("../../tests/fixtures/vision-colors.jpg").as_slice()),
        ] {
            let result = prepare(bytes, mime).unwrap();
            assert!(matches!(result.bytes, Cow::Borrowed(_)));
            assert_eq!(result.bytes.as_ref(), bytes);
            assert_eq!(result.resized, None);
            let attachment = ModelAttachment::new(bytes.to_vec(), mime.into(), name.into()).unwrap();
            let wire = parts(std::slice::from_ref(&attachment), Format::Anthropic, "Claude");
            let image = wire.iter().find(|part| part["type"] == "image").unwrap();
            assert_eq!(image["source"]["media_type"], mime);
            assert_eq!(STANDARD.decode(image["source"]["data"].as_str().unwrap()).unwrap(), bytes);
            assert_eq!(attachment.bytes(), bytes);
        }
    }

    #[test]
    fn large_jpeg_wire_copy_preserves_pixels_and_original_bytes_without_changing_other_providers() {
        let original = large_jpeg();
        let original_digest = Sha256::digest(&original);
        let attachment = ModelAttachment::new(original.clone(), "image/jpeg".into(), "photo.jpg".into()).unwrap();
        let wire = parts(std::slice::from_ref(&attachment), Format::Anthropic, "Claude");
        let image = wire.iter().find(|part| part["type"] == "image").unwrap();
        assert_eq!(image["source"]["media_type"], "image/jpeg");
        let encoded = image["source"]["data"].as_str().unwrap();
        assert!(encoded.len() <= MAX_INSPECTION_BYTES.div_ceil(3) * 4);
        let bytes = STANDARD.decode(encoded).unwrap();
        assert!(bytes.len() <= MAX_INSPECTION_BYTES);
        let pixels = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert!(pixels.width().max(pixels.height()) <= INSPECTION_LONG_EDGE);
        assert!(u64::from(pixels.width()) * u64::from(pixels.height()) <= INSPECTION_PIXELS);
        for (index, expected) in colors().into_iter().enumerate() {
            let x = pixels.width() * if index % 2 == 0 { 1 } else { 3 } / 4;
            let y = pixels.height() * if index < 2 { 1 } else { 3 } / 4;
            for (actual, expected) in pixels.get_pixel(x, y).0.into_iter().zip(expected) {
                assert!(actual.abs_diff(expected) < 30, "the inspection copy must retain the source's colored regions");
            }
        }
        assert_eq!(attachment.bytes(), original);
        assert_eq!(Sha256::digest(attachment.bytes()), original_digest);
        let openai = parts(std::slice::from_ref(&attachment), Format::OpenAi, "OpenAI");
        assert_eq!(openai.iter().find(|part| part["type"] == "image_url").unwrap()["image_url"]["url"], format!("data:image/jpeg;base64,{}", STANDARD.encode(&original)));
        let gemini = parts(std::slice::from_ref(&attachment), Format::Gemini, "Gemini");
        assert_eq!(gemini.iter().find(|part| part.get("inlineData").is_some()).unwrap()["inlineData"]["data"], STANDARD.encode(&original));
    }

    #[test]
    fn oversized_png_reduces_until_bounded_and_preserves_alpha_and_original_bytes() {
        let mut seed = 0x8295da14c761b308;
        let image = RgbaImage::from_fn(960, 960, |x, y| {
            if let Some([r, g, b]) = marked_pixel(x, y, 960, 960) { Rgba([r, g, b, 80]) }
            else { Rgba(noise(&mut seed)) }
        });
        let mut original = Vec::new();
        image::codecs::png::PngEncoder::new(&mut original).write_image(image.as_raw(), 960, 960, image::ExtendedColorType::Rgba8).unwrap();
        assert!(original.len() > MAX_INSPECTION_BYTES);
        let input = prepare(&original, "image/png").unwrap();
        assert!(input.bytes.len() <= MAX_INSPECTION_BYTES);
        let pixels = image::load_from_memory(&input.bytes).unwrap().to_rgba8();
        assert!(pixels.width() < 960 && pixels.height() < 960, "an oversized lossless encode must reduce its dimensions again");
        for (index, [r, g, b]) in colors().into_iter().enumerate() {
            let x = pixels.width() * if index % 2 == 0 { 1 } else { 3 } / 4;
            let y = pixels.height() * if index < 2 { 1 } else { 3 } / 4;
            assert_eq!(pixels.get_pixel(x, y).0, [r, g, b, 80], "PNG inspection must preserve alpha rather than flatten its background");
        }
        let attachment = ModelAttachment::new(original.clone(), "image/png".into(), "alpha.png".into()).unwrap();
        let wire = parts(std::slice::from_ref(&attachment), Format::Anthropic, "Claude");
        assert_eq!(wire.iter().find(|part| part["type"] == "image").unwrap()["source"]["media_type"], "image/png");
        assert_eq!(attachment.bytes(), original);
    }

    #[test]
    fn jpeg_inspection_applies_exif_orientation_before_resizing() {
        let image = RgbImage::from_fn(320, 160, |x, _| Rgb(if x < 160 { [255, 0, 0] } else { [0, 0, 255] }));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95).encode_image(&image).unwrap();
        // Little-endian TIFF orientation 6 means rotate clockwise 90 degrees.
        let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut original = jpeg[..2].to_vec();
        original.extend_from_slice(&[0xff, 0xe1]);
        original.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        original.extend_from_slice(exif);
        original.extend_from_slice(&jpeg[2..]);
        original.resize(MAX_INSPECTION_BYTES + 1, 0);
        let result = prepare(&original, "image/jpeg").unwrap();
        assert_eq!(result.resized, Some((160, 320)));
        let pixels = image::load_from_memory(&result.bytes).unwrap().to_rgb8();
        assert!(pixels.get_pixel(80, 80).0[0] > 240);
        assert!(pixels.get_pixel(80, 240).0[2] > 240);
    }

    #[test]
    fn inspection_checks_header_limits_before_decoding_and_resizes_overwide_images() {
        let image = RgbImage::from_pixel(8001, 1, Rgb([255, 0, 0]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes).encode_image(&image).unwrap();
        assert!(bytes.len() < MAX_INSPECTION_BYTES);
        let result = prepare(&bytes, "image/jpeg").unwrap();
        assert_eq!(result.resized, Some((1568, 1)));
        let start = bytes.windows(2).position(|bytes| bytes == [0xff, 0xc0]).unwrap();
        bytes[start + 5..start + 7].copy_from_slice(&6000u16.to_be_bytes());
        bytes[start + 7..start + 9].copy_from_slice(&6000u16.to_be_bytes());
        assert!(prepare(&bytes, "image/jpeg").unwrap_err().contains("32 million pixels"));
        bytes[start + 5..start + 7].copy_from_slice(&1u16.to_be_bytes());
        bytes[start + 7..start + 9].copy_from_slice(&16385u16.to_be_bytes());
        assert!(prepare(&bytes, "image/jpeg").is_err());
        assert!(prepare(b"not an image", "image/png").is_err());
    }
}
