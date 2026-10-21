//! Bounded, decoded clipboard images and interoperable PNG/JPEG encodings.
//!
//! A compressed-file limit alone does not bound a bitmap allocation. Dimensions
//! and decoder allocations are checked before pixels are decoded. The image is
//! immutable and normally shared through Arc; Debug never contains image data.

use ::image::{
    codecs::{bmp::BmpDecoder, jpeg::JpegEncoder, png::PngEncoder},
    metadata::Orientation,
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits,
};
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::TryReserveError;
use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::sync::Arc;

pub const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RGBA_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DIMENSION: u32 = 16_384;
const MAX_DECODER_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone)]
pub struct ClipboardImage {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
    encoded: Arc<[u8]>,
    png: bool,
    digest: [u8; 32],
}

impl std::fmt::Debug for ClipboardImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("encoded_bytes", &self.encoded.len())
            .finish()
    }
}

impl PartialEq for ClipboardImage {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height && self.digest == other.digest
    }
}
impl Eq for ClipboardImage {}

impl ClipboardImage {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    /// An existing PNG can additionally be published in Windows' registered
    /// PNG format. JPEGs still have the same native DIBV5 bitmap representation.
    pub fn png(&self) -> Option<&[u8]> {
        self.png.then_some(&self.encoded)
    }

    pub fn rgba(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(self.rgba.len())?;
        bytes.extend_from_slice(&self.rgba);
        Ok(bytes)
    }

    pub fn from_encoded(bytes: &[u8]) -> Result<Self> {
        check_encoded_length(bytes.len())?;
        let format = ::image::guess_format(bytes).context("无法识别图片格式")?;
        ensure!(
            matches!(format, ImageFormat::Png | ImageFormat::Jpeg),
            "图片同步目前支持 PNG 和 JPEG"
        );
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(decoder_limits());
        let mut decoder = reader.into_decoder().context("无法读取图片头")?;
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let mut decoded = decode_bounded(decoder)?;
        decoded.apply_orientation(orientation);
        let rgba = decoded.into_rgba8();
        let (width, height) = rgba.dimensions();
        let rgba = rgba.into_raw();
        // Preserve JPEG bytes instead of inflating photographs into a PNG that
        // may exceed the network limit. The native bitmap uses oriented pixels.
        if format == ImageFormat::Png && orientation != Orientation::NoTransforms {
            return Self::from_rgba(width, height, rgba);
        }
        Self::assemble(
            width,
            height,
            rgba,
            bytes.to_vec(),
            format == ImageFormat::Png,
        )
    }

    pub fn from_downloaded(image: &wx_ime_sdk::DownloadedImage) -> Result<Self> {
        check_encoded_length(image.bytes)?;
        let file = File::open(&image.path).context("打开接收图片缓存失败")?;
        let length = file.metadata().context("读取图片缓存长度失败")?.len();
        ensure!(length == image.bytes as u64, "接收图片缓存长度不匹配");
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(image.bytes)?;
        file.take((MAX_ENCODED_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .context("读取接收图片缓存失败")?;
        ensure!(bytes.len() == image.bytes, "接收图片缓存长度发生变化");
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == image.sha256,
            "接收图片缓存校验失败"
        );
        Self::from_encoded(&bytes)
    }

    /// Windows CF_DIB / CF_DIBV5 have no BMP file header. The BMP decoder handles
    /// row direction, palettes and channel masks without handwritten pixel parsing.
    pub fn from_dib(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_RGBA_BYTES + 1024 * 1024,
            "剪贴板位图数据过大或为空"
        );
        let decoder = BmpDecoder::new_without_file_header(Cursor::new(bytes))
            .context("无法读取剪贴板位图头")?;
        let rgba = decode_bounded(decoder)?.into_rgba8();
        let (width, height) = rgba.dimensions();
        Self::from_rgba(width, height, rgba.into_raw())
    }

    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self> {
        Self::from_rgba_with_limit(width, height, rgba, MAX_ENCODED_BYTES)
    }

    fn from_rgba_with_limit(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        encoded_limit: usize,
    ) -> Result<Self> {
        let required = check_dimensions(width, height)?;
        ensure!(rgba.len() == required, "图片像素长度与尺寸不匹配");
        let mut png = LimitedWriter::new(encoded_limit);
        let encoded =
            PngEncoder::new(&mut png).write_image(&rgba, width, height, ExtendedColorType::Rgba8);
        if let Some(error) = png.allocation_failure.take() {
            return Err(error).context("为 PNG 编码分配内存失败");
        }
        match encoded {
            // The PNG crate finalizes IEND in Drop; an error in that final write
            // may not be returned by ImageEncoder. Check the writer as well so
            // a cap hit at the end never publishes a truncated PNG.
            Ok(()) if !png.exceeded => Self::assemble(width, height, rgba, png.bytes, true),
            Err(error) if !png.exceeded => Err(error).context("PNG 编码失败"),
            _ => {
                // A high-detail photograph can be large as PNG even when a JPEG
                // fits easily. Never discard transparency to make it fit.
                ensure!(
                    rgba.chunks_exact(4).all(|pixel| pixel[3] == 255),
                    "透明图片编码超过 16 MiB；请缩小图片后重试"
                );
                drop(png);
                let mut rgb = Vec::new();
                rgb.try_reserve_exact(required / 4 * 3)?;
                for pixel in rgba.chunks_exact(4) {
                    rgb.extend_from_slice(&pixel[..3]);
                }
                for quality in [95, 90] {
                    let mut jpeg = LimitedWriter::new(encoded_limit);
                    let encoded = JpegEncoder::new_with_quality(&mut jpeg, quality).encode(
                        &rgb,
                        width,
                        height,
                        ExtendedColorType::Rgb8,
                    );
                    if let Some(error) = jpeg.allocation_failure.take() {
                        return Err(error).context("为 JPEG 编码分配内存失败");
                    }
                    match encoded {
                        Ok(()) if !jpeg.exceeded => {
                            eprintln!(
                                "[wxclipd] 图片的 PNG 超过 16 MiB，已改用 JPEG（质量 {quality}）"
                            );
                            return Self::assemble(width, height, rgba, jpeg.bytes, false);
                        }
                        Err(error) if !jpeg.exceeded => return Err(error).context("JPEG 编码失败"),
                        _ => {}
                    }
                }
                bail!("图片编码超过 16 MiB；请缩小图片后重试")
            }
        }
    }

    fn assemble(
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        encoded: Vec<u8>,
        png: bool,
    ) -> Result<Self> {
        ensure!(
            rgba.len() == check_dimensions(width, height)?,
            "图片像素长度不匹配"
        );
        check_encoded_length(encoded.len())?;
        let mut hash = Sha256::new();
        hash.update(width.to_le_bytes());
        hash.update(height.to_le_bytes());
        hash.update(&rgba);
        Ok(Self {
            width,
            height,
            rgba: rgba.into(),
            encoded: encoded.into(),
            png,
            digest: hash.finalize().into(),
        })
    }
}

pub fn check_dimensions(width: u32, height: u32) -> Result<usize> {
    ensure!(
        width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "图片每边必须为 1～16384 像素"
    );
    let bytes = u64::from(width) * u64::from(height) * 4;
    ensure!(
        bytes <= MAX_RGBA_BYTES as u64,
        "图片超过 16777216 像素（64 MiB 位图）限制"
    );
    Ok(bytes as usize)
}

fn check_encoded_length(bytes: usize) -> Result<()> {
    ensure!(
        bytes > 0 && bytes <= MAX_ENCODED_BYTES,
        "图片编码为空或超过 16 MiB"
    );
    Ok(())
}

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODER_BYTES);
    limits
}

fn decode_bounded(mut decoder: impl ImageDecoder) -> Result<DynamicImage> {
    let (width, height) = decoder.dimensions();
    check_dimensions(width, height)?;
    ensure!(
        decoder.total_bytes() <= MAX_DECODER_BYTES,
        "图片解码缓冲区过大"
    );
    decoder
        .set_limits(decoder_limits())
        .context("设置图片解码限制失败")?;
    DynamicImage::from_decoder(decoder).context("图片数据无效或不完整")
}

/// Stop the encoder as soon as it exceeds the upload limit, rather than first
/// allocating an arbitrarily large output and checking its length afterwards.
struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
    allocation_failure: Option<TryReserveError>,
}
impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
            allocation_failure: None,
        }
    }
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.allocation_failure.is_some() {
            return Err(io::Error::other("encoded image allocation failed"));
        }
        if self.exceeded || bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("encoded image exceeds limit"));
        }
        if let Err(error) = self.bytes.try_reserve(bytes.len()) {
            // Preserve a typed error even if PNG's Drop suppresses the I/O
            // result. Callers must retry memory pressure, not consume the image.
            self.allocation_failure = Some(error);
            return Err(io::Error::other("encoded image allocation failed"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_preserves_transparency_and_pixel_identity() {
        let pixels = vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 4, 5, 6, 255];
        let original = ClipboardImage::from_rgba(2, 2, pixels.clone()).unwrap();
        let received = ClipboardImage::from_encoded(original.encoded()).unwrap();
        assert_eq!(received.rgba().unwrap(), pixels);
        assert_eq!(original, received);
        assert!(original.png().is_some());
    }

    #[test]
    fn jpeg_receipt_keeps_original_encoding_and_compares_by_decoded_pixels() {
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode(&[220, 80, 20, 10, 150, 30], 2, 1, ExtendedColorType::Rgb8)
            .unwrap();
        let photo = ClipboardImage::from_encoded(&jpeg).unwrap();
        assert_eq!(photo.encoded(), jpeg);
        assert!(photo.png().is_none());
        let bitmap = ClipboardImage::from_rgba(2, 1, photo.rgba().unwrap()).unwrap();
        assert_eq!(photo, bitmap);
    }

    #[test]
    fn limits_reject_dimensions_before_allocating_pixels() {
        assert!(check_dimensions(0, 1).is_err());
        assert!(check_dimensions(1, MAX_DIMENSION + 1).is_err());
        assert!(check_dimensions(8192, 8192).is_err());
        assert!(check_dimensions(7680, 2160).is_ok());
        assert!(ClipboardImage::from_rgba(2, 2, vec![0; 4]).is_err());
        // A 40-byte DIB header claiming an enormous bitmap has no pixel data.
        let mut dib = vec![0; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&20_000i32.to_le_bytes());
        dib[8..12].copy_from_slice(&20_000i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        assert!(ClipboardImage::from_dib(&dib).is_err());
    }

    #[test]
    fn truncated_and_unrecognized_images_are_rejected() {
        assert!(ClipboardImage::from_encoded(b"GIF89a").is_err());
        assert!(ClipboardImage::from_encoded(b"\x89PNG\r\n\x1a\n").is_err());
        assert!(ClipboardImage::from_encoded(b"").is_err());
    }

    #[test]
    fn encoded_writer_never_grows_past_limit() {
        let mut writer = LimitedWriter::new(5);
        writer.write_all(b"1234").unwrap();
        assert!(writer.write_all(b"56").is_err());
        assert!(writer.write_all(b"7").is_err());
        assert_eq!(writer.bytes, b"1234");
        assert!(writer.exceeded);
    }

    #[test]
    fn large_opaque_png_uses_jpeg_without_dropping_transparency() {
        let mut random = 0x12345678u32;
        let mut pixels = Vec::new();
        for _ in 0..64 * 64 {
            for _ in 0..3 {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                pixels.push(random as u8);
            }
            pixels.push(255);
        }
        let png = ClipboardImage::from_rgba(64, 64, pixels.clone()).unwrap();
        let limit = png.encoded().len() - 1;
        let fallback = ClipboardImage::from_rgba_with_limit(64, 64, pixels.clone(), limit).unwrap();
        assert!(fallback.png().is_none());
        assert!(fallback.encoded().len() <= limit);
        assert_eq!(fallback.rgba().unwrap(), pixels);
        assert_eq!(
            ClipboardImage::from_encoded(fallback.encoded())
                .unwrap()
                .width(),
            64
        );
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 128;
        }
        let transparent = ClipboardImage::from_rgba(64, 64, pixels.clone()).unwrap();
        assert!(ClipboardImage::from_rgba_with_limit(
            64,
            64,
            pixels,
            transparent.encoded().len() - 1
        )
        .is_err());
    }

    #[test]
    fn received_cache_is_size_and_hash_checked() {
        let picture = ClipboardImage::from_rgba(1, 1, vec![1, 2, 3, 255]).unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(picture.encoded()).unwrap();
        let mut downloaded = wx_ime_sdk::DownloadedImage {
            path: file.path().into(),
            bytes: picture.encoded().len(),
            sha256: format!("{:x}", Sha256::digest(picture.encoded())),
        };
        assert_eq!(
            ClipboardImage::from_downloaded(&downloaded).unwrap(),
            picture
        );
        downloaded.sha256 = "0".repeat(64);
        assert!(ClipboardImage::from_downloaded(&downloaded).is_err());
        downloaded.bytes += 1;
        assert!(ClipboardImage::from_downloaded(&downloaded).is_err());
    }
}
