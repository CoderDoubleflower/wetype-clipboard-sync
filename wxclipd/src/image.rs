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
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Write};
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

    /// Windows CF_DIB / CF_DIBV5 have no BMP file header. Supply an explicit
    /// pixel offset: image 0.25.10's headerless decoder skips an extra 12 mask
    /// bytes for V4/V5 even though those masks are already inside the header.
    pub fn from_dib(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_RGBA_BYTES + 1024 * 1024,
            "剪贴板位图数据过大或为空"
        );
        let decoder = BmpDecoder::new(BufReader::new(DibFile::new(bytes)?))
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

/// Present the small BMP file header and borrowed packed DIB as one seekable
/// stream. This avoids another copy of a bitmap that can already be 64 MiB.
struct DibFile<'a> {
    header: [u8; 14],
    dib: &'a [u8],
    position: u64,
}

impl<'a> DibFile<'a> {
    fn new(dib: &'a [u8]) -> Result<Self> {
        let pixel_offset = dib_pixel_offset(dib)?;
        let file_size = u32::try_from(dib.len() + 14).context("位图文件过大")?;
        let mut header = [0; 14];
        header[..2].copy_from_slice(b"BM");
        header[2..6].copy_from_slice(&file_size.to_le_bytes());
        header[10..14].copy_from_slice(&(pixel_offset as u32 + 14).to_le_bytes());
        Ok(Self {
            header,
            dib,
            position: 0,
        })
    }
}

impl Read for DibFile<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut copied = 0;
        if self.position < self.header.len() as u64 {
            let start = self.position as usize;
            let count = buffer.len().min(self.header.len() - start);
            buffer[..count].copy_from_slice(&self.header[start..start + count]);
            self.position += count as u64;
            copied += count;
        }
        let length = (self.header.len() + self.dib.len()) as u64;
        if self.position >= self.header.len() as u64 && self.position < length {
            let start = self.position as usize - self.header.len();
            let count = (buffer.len() - copied).min(self.dib.len() - start);
            buffer[copied..copied + count].copy_from_slice(&self.dib[start..start + count]);
            self.position += count as u64;
            copied += count;
        }
        Ok(copied)
    }
}

impl Seek for DibFile<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => {
                (self.header.len() + self.dib.len()) as i128 + i128::from(offset)
            }
        };
        self.position = u64::try_from(position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid bitmap seek"))?;
        Ok(self.position)
    }
}

fn dib_field<const N: usize>(dib: &[u8], offset: usize) -> Result<[u8; N]> {
    dib.get(offset..offset + N)
        .and_then(|bytes| bytes.try_into().ok())
        .context("剪贴板位图头不完整")
}

fn dib_pixel_offset(dib: &[u8]) -> Result<usize> {
    let header = u32::from_le_bytes(dib_field(dib, 0)?) as usize;
    ensure!(
        matches!(header, 12 | 40 | 52 | 56 | 108 | 124) && header <= dib.len(),
        "不支持或不完整的剪贴板位图头"
    );
    let (width, height, bits, compression, image_size, colors, color_bytes, masks) = if header == 12
    {
        let width = u16::from_le_bytes(dib_field(dib, 4)?) as u32;
        let height = u16::from_le_bytes(dib_field(dib, 6)?) as u32;
        ensure!(
            u16::from_le_bytes(dib_field(dib, 8)?) == 1,
            "位图平面数无效"
        );
        let bits = u16::from_le_bytes(dib_field(dib, 10)?);
        ensure!(matches!(bits, 1 | 4 | 8 | 24), "不支持的核心位图位深");
        let colors = if bits <= 8 { 1usize << bits } else { 0 };
        (width, height, bits, 0, 0, colors, 3, 0)
    } else {
        let width = i32::from_le_bytes(dib_field(dib, 4)?);
        let height = i32::from_le_bytes(dib_field(dib, 8)?);
        ensure!(width > 0 && height != 0, "位图尺寸无效");
        ensure!(
            u16::from_le_bytes(dib_field(dib, 12)?) == 1,
            "位图平面数无效"
        );
        let bits = u16::from_le_bytes(dib_field(dib, 14)?);
        let compression = u32::from_le_bytes(dib_field(dib, 16)?);
        ensure!(
            matches!(
                (compression, bits),
                (0, 1 | 2 | 4 | 8 | 16 | 24 | 32) | (1, 8) | (2, 4) | (3, 16 | 32)
            ),
            "不支持的位图压缩或位深"
        );
        ensure!(
            height > 0 || matches!(compression, 0 | 3),
            "压缩位图不能自顶向下排列"
        );
        let image_size = u32::from_le_bytes(dib_field(dib, 20)?) as usize;
        let used = u32::from_le_bytes(dib_field(dib, 32)?) as usize;
        let colors = if bits <= 8 {
            let maximum = 1usize << bits;
            ensure!(used <= maximum, "位图调色板长度无效");
            if used == 0 {
                maximum
            } else {
                used
            }
        } else {
            used
        };
        // Only BITMAPINFOHEADER has external RGB masks. V2/V3/V4/V5
        // include them in their declared header size.
        let masks = if header == 40 && compression == 3 {
            12
        } else {
            0
        };
        (
            width as u32,
            height.unsigned_abs(),
            bits,
            compression,
            image_size,
            colors,
            4,
            masks,
        )
    };
    check_dimensions(width, height)?;
    let pixel_offset = colors
        .checked_mul(color_bytes)
        .and_then(|palette| header.checked_add(masks)?.checked_add(palette))
        .filter(|&offset| offset <= dib.len())
        .context("位图调色板或掩码不完整")?;
    let pixel_bytes = if matches!(compression, 0 | 3) {
        let stride = (width as usize * bits as usize).div_ceil(32) * 4;
        let required = stride
            .checked_mul(height as usize)
            .context("位图像素长度溢出")?;
        ensure!(
            image_size == 0 || image_size >= required,
            "位图像素长度无效"
        );
        required.max(image_size)
    } else {
        ensure!(image_size > 0, "压缩位图缺少像素长度");
        image_size
    };
    let pixel_end = pixel_offset
        .checked_add(pixel_bytes)
        .filter(|&end| end <= dib.len())
        .context("剪贴板位图像素不完整")?;
    if header == 124 {
        let color_space = u32::from_le_bytes(dib_field(dib, 56)?);
        // In a packed clipboard DIB, an embedded/linked profile follows the
        // pixels. Validate its range; never open a linked profile filename.
        if matches!(color_space, 0x4d42_4544 | 0x4c49_4e4b) {
            let profile_offset = u32::from_le_bytes(dib_field(dib, 112)?) as usize;
            let profile_size = u32::from_le_bytes(dib_field(dib, 116)?) as usize;
            ensure!(
                profile_size > 0
                    && profile_offset >= pixel_end
                    && profile_offset
                        .checked_add(profile_size)
                        .is_some_and(|end| end <= dib.len()),
                "位图颜色配置范围无效或与像素重叠"
            );
        }
    }
    Ok(pixel_offset)
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

    fn dib_info_header(
        size: usize,
        width: i32,
        height: i32,
        bits: u16,
        compression: u32,
    ) -> Vec<u8> {
        let mut dib = vec![0; size];
        dib[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        dib[4..8].copy_from_slice(&width.to_le_bytes());
        dib[8..12].copy_from_slice(&height.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&bits.to_le_bytes());
        dib[16..20].copy_from_slice(&compression.to_le_bytes());
        dib
    }

    fn dib_alpha_header(size: usize, width: i32, height: i32) -> Vec<u8> {
        let mut dib = dib_info_header(size, width, height, 32, 3);
        dib[40..44].copy_from_slice(&0x00ff_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_ff00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00ffu32.to_le_bytes());
        dib[52..56].copy_from_slice(&0xff00_0000u32.to_le_bytes());
        dib[56..60].copy_from_slice(&0x7352_4742u32.to_le_bytes());
        dib
    }

    #[test]
    fn packed_dib_v4_v5_preserve_alpha_without_skipping_embedded_masks() {
        for header_size in [108, 124] {
            let mut dib = dib_alpha_header(header_size, 2, -2);
            // BGRA pixels immediately follow the complete V4/V5 header. There
            // are no additional masks between that header and the pixel data.
            dib.extend_from_slice(&[0, 0, 255, 255, 0, 255, 0, 128, 7, 8, 9, 0, 255, 0, 0, 255]);
            let decoded = ClipboardImage::from_dib(&dib).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
            assert_eq!(
                decoded.rgba().unwrap(),
                [255, 0, 0, 255, 0, 255, 0, 128, 9, 8, 7, 0, 0, 0, 255, 255],
                "BITMAP header size {header_size}"
            );
            dib.pop();
            assert!(ClipboardImage::from_dib(&dib).is_err());
        }
    }

    #[test]
    fn packed_dib_info24_preserves_bottom_up_rows_and_padding() {
        let mut dib = dib_info_header(40, 2, 2, 24, 0);
        dib.extend_from_slice(&[
            255, 0, 0, 255, 255, 255, 0, 0, // bottom: blue, white, padding
            0, 0, 255, 0, 255, 0, 0, 0, // top: red, green, padding
        ]);
        assert_eq!(
            ClipboardImage::from_dib(&dib).unwrap().rgba().unwrap(),
            [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn packed_dib_info16_uses_external_rgb565_masks() {
        let mut dib = dib_info_header(40, 2, 1, 16, 3);
        for mask in [0xf800u32, 0x07e0, 0x001f] {
            dib.extend_from_slice(&mask.to_le_bytes());
        }
        dib.extend_from_slice(&[0x00, 0xf8, 0xe0, 0x07]); // red, green
        assert_eq!(
            ClipboardImage::from_dib(&dib).unwrap().rgba().unwrap(),
            [255, 0, 0, 255, 0, 255, 0, 255]
        );
        dib.truncate(48); // missing part of the external blue mask
        assert!(ClipboardImage::from_dib(&dib).is_err());
    }

    #[test]
    fn packed_dib_palettes_use_header_specific_entry_sizes_and_reject_overflow() {
        let mut core = Vec::new();
        core.extend_from_slice(&12u32.to_le_bytes());
        for field in [1u16, 1, 1, 1] {
            core.extend_from_slice(&field.to_le_bytes());
        }
        core.extend_from_slice(&[0, 0, 0, 0, 0, 255]); // two RGBTRIPLE entries
        core.extend_from_slice(&[0x80, 0, 0, 0]);
        assert_eq!(
            ClipboardImage::from_dib(&core).unwrap().rgba().unwrap(),
            [255, 0, 0, 255]
        );

        let mut info = dib_info_header(40, 1, 1, 8, 0);
        info[32..36].copy_from_slice(&2u32.to_le_bytes());
        info.extend_from_slice(&[0, 0, 0, 0, 0, 255, 0, 0]); // two RGBQUAD entries
        info.extend_from_slice(&[1, 0, 0, 0]);
        assert_eq!(
            ClipboardImage::from_dib(&info).unwrap().rgba().unwrap(),
            [0, 255, 0, 255]
        );
        info[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ClipboardImage::from_dib(&info).is_err());
        info[14..16].copy_from_slice(&32u16.to_le_bytes());
        assert!(ClipboardImage::from_dib(&info).is_err());
    }

    #[test]
    fn packed_dib_v5_profiles_follow_pixels_and_never_open_linked_paths() {
        for profile_type in [0x4d42_4544u32, 0x4c49_4e4b] {
            let mut dib = dib_alpha_header(124, 1, -1);
            let profile = b"missing-color-profile.icc\0";
            dib[56..60].copy_from_slice(&profile_type.to_le_bytes());
            dib[112..116].copy_from_slice(&128u32.to_le_bytes());
            dib[116..120].copy_from_slice(&(profile.len() as u32).to_le_bytes());
            dib.extend_from_slice(&[3, 2, 1, 128]);
            dib.extend_from_slice(profile);
            assert_eq!(
                ClipboardImage::from_dib(&dib).unwrap().rgba().unwrap(),
                [1, 2, 3, 128]
            );
            dib[112..116].copy_from_slice(&124u32.to_le_bytes());
            assert!(ClipboardImage::from_dib(&dib).is_err()); // overlaps pixels
            dib[112..116].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(ClipboardImage::from_dib(&dib).is_err()); // overflows/range
        }
    }

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
