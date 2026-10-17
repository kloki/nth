//! An image file read for the model to see: downscaled to what models still
//! read text in, kept in the session's image folder under its hash, and
//! attached to the call beside a line saying what was read.

use std::{
    io::Cursor,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use base64::Engine;
use image::{
    AnimationDecoder, DynamicImage, GenericImageView, ImageFormat, ImageReader, Limits,
    codecs::{gif::GifDecoder, jpeg::JpegEncoder, webp::WebPDecoder},
    imageops::FilterType,
};
use nth_protocol::{Image, MAX_IMAGE_BYTES, ToolContext, image_unsupported};
use sha2::{Digest, Sha256};

/// The longest side an image is shown at: a 4K screen's size at 2× scale,
/// where every model tested still read normal text, at about 2,500 tokens.
/// Larger costs some models four times as much and reads no better.
const MAX_SIDE: u32 = 1920;

/// The most pixels an image may claim before it is decoded: a 50 MP photo
/// is about the largest camera's, and decoding past it only costs memory.
const MAX_PIXELS: u64 = 50_000_000;

/// How often an image still over `MAX_IMAGE_BYTES` is shrunk by a quarter
/// before giving up: four times leaves a third of the side.
const SHRINKS: usize = 4;

/// The JPEG quality a photo is encoded at: no artefacts a model notices,
/// about a tenth of the PNG.
const JPEG_QUALITY: u8 = 85;

/// The format of `bytes`, if they are an image the read tool shows.
pub(super) fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    match image::guess_format(bytes) {
        Ok(
            format @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP),
        ) => Some(format),
        _ => None,
    }
}

/// Shows the image in `bytes`, read from `path`, to the model: the text
/// result says what it is, the image goes on `ctx.attachments`.
pub(super) async fn read(
    path: &Path,
    bytes: Vec<u8>,
    format: ImageFormat,
    ctx: &ToolContext,
) -> Result<String, String> {
    if !ctx.takes_images {
        return Err(image_unsupported(&path.display().to_string()));
    }
    // An image kept nowhere is gone at the next load, so with no store to
    // keep it in (a test, or a session that is never saved) none is shown.
    let Some(images) = &ctx.images else {
        return Err("cannot show images: no session store to keep them in".into());
    };
    // Decoding and resizing are CPU work with no await in them, about
    // 100 ms for a 4K screenshot: too long to hold a runtime worker.
    let shown = tokio::task::spawn_blocking(move || prepare(bytes, format))
        .await
        .map_err(|e| format!("reading the image failed: {e}"))??;
    let file = images.join(format!("{}.{}", shown.sha256, shown.ext));
    keep(&file, &shown.bytes)
        .await
        .map_err(|e| format!("cannot keep the image at {}: {e}", file.display()))?;
    let mut text = format!(
        "Image read: {}, {}×{} {}",
        path.display(),
        shown.width,
        shown.height,
        shown.ext
    );
    if (shown.width, shown.height) != shown.original {
        text.push_str(&format!(
            " (from {}×{})",
            shown.original.0, shown.original.1
        ));
    }
    if shown.animated {
        text.push_str(", first frame of an animation");
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&shown.bytes);
    ctx.attachments.add(Image {
        mime: shown.mime.into(),
        sha256: shown.sha256,
        width: shown.width,
        height: shown.height,
        path: file,
        data: Some(data.into()),
    });
    Ok(text)
}

/// An image as it is shown: its bytes, what they are, and its size before.
struct Shown {
    bytes: Vec<u8>,
    mime: &'static str,
    ext: &'static str,
    width: u32,
    height: u32,
    original: (u32, u32),
    animated: bool,
    sha256: String,
}

/// Downscales an image past `MAX_SIDE`, shrinks one past `MAX_IMAGE_BYTES`,
/// turns a GIF into a PNG, since not every provider takes GIF (xAI refuses
/// it), and an animated WebP into its first frame. What is re-encoded goes
/// out as a JPEG when it came as a photo format (JPEG or WebP), else as a
/// PNG; anything else goes as it came.
fn prepare(bytes: Vec<u8>, format: ImageFormat) -> Result<Shown, String> {
    // The header says the size, so a small file claiming a huge image is
    // refused before its pixels are allocated.
    let (width, height) = reader(&bytes, format)
        .into_dimensions()
        .map_err(|e| format!("cannot decode the image: {e}"))?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "cannot show the image: {width}×{height} is over {} megapixels",
            MAX_PIXELS / 1_000_000
        ));
    }
    let animated = animated(&bytes, format);
    let mut reader = reader(&bytes, format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| format!("cannot decode the image: {e}"))?;
    let original = image.dimensions();
    let too_big = original.0.max(original.1) > MAX_SIDE;
    let out = match format {
        ImageFormat::Jpeg | ImageFormat::WebP => ImageFormat::Jpeg,
        _ => ImageFormat::Png,
    };
    // A WebP's own frames are a still's, so only an animated one changes.
    let reencode =
        too_big || format == ImageFormat::Gif || (format == ImageFormat::WebP && animated);
    let mut image = if too_big {
        image.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle)
    } else {
        image
    };
    let (mut bytes, mut format) = if reencode {
        (encode(&image, out)?, out)
    } else {
        (bytes, format)
    };
    let mut shrinks = 0;
    while bytes.len() > MAX_IMAGE_BYTES {
        if shrinks == SHRINKS {
            let (width, height) = image.dimensions();
            return Err(format!(
                "cannot show the image: still over {MAX_IMAGE_BYTES} bytes at {width}×{height}"
            ));
        }
        shrinks += 1;
        let (width, height) = image.dimensions();
        image = image.resize(
            (width * 3 / 4).max(1),
            (height * 3 / 4).max(1),
            FilterType::Triangle,
        );
        bytes = encode(&image, out)?;
        format = out;
    }
    let (mime, ext) = match format {
        ImageFormat::Jpeg => ("image/jpeg", "jpeg"),
        ImageFormat::WebP => ("image/webp", "webp"),
        _ => ("image/png", "png"),
    };
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (width, height) = image.dimensions();
    Ok(Shown {
        bytes,
        mime,
        ext,
        width,
        height,
        original,
        animated,
        sha256,
    })
}

fn reader(bytes: &[u8], format: ImageFormat) -> ImageReader<Cursor<&[u8]>> {
    ImageReader::with_format(Cursor::new(bytes), format)
}

/// Whether `bytes` hold more than one frame; only the first is shown.
fn animated(bytes: &[u8], format: ImageFormat) -> bool {
    match format {
        ImageFormat::Gif => GifDecoder::new(Cursor::new(bytes))
            .map(|gif| gif.into_frames().take(2).filter(Result::is_ok).count() > 1)
            .unwrap_or(false),
        ImageFormat::WebP => WebPDecoder::new(Cursor::new(bytes))
            .map(|webp| webp.has_animation())
            .unwrap_or(false),
        _ => false,
    }
}

/// `image` as `format`, a JPEG or a PNG. JPEG has no alpha, so it is
/// dropped first.
fn encode(image: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    let encoded = if format == ImageFormat::Jpeg {
        let encoder = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        DynamicImage::ImageRgb8(image.to_rgb8()).write_with_encoder(encoder)
    } else {
        image.write_to(&mut out, format)
    };
    encoded.map_err(|e| format!("cannot encode the image: {e}"))?;
    Ok(out.into_inner())
}

/// Writes `bytes` to `file` unless it is there: named by the hash, the same
/// name means the same bytes. Written aside and renamed over, so a read
/// running beside never sees half a file.
async fn keep(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if tokio::fs::try_exists(file).await? {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    // Unique per write, as two calls may keep the same image at once.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let tmp = file.with_extension(format!("{}.{n}.tmp", std::process::id()));
    let kept = match tokio::fs::write(&tmp, bytes).await {
        Ok(()) => tokio::fs::rename(&tmp, file).await,
        Err(e) => Err(e),
    };
    if kept.is_err() {
        // A full disk or a folder gone mid-write leaves no stray half.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    kept
}

#[cfg(test)]
mod tests {
    use nth_protocol::Tool;
    use serde_json::json;

    use super::*;
    use crate::read::{Read, ReadConfig};

    /// A `width`×`height` image in `format`, red on the left half and blue
    /// on the right.
    fn picture(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, _| {
            if x < width / 2 {
                image::Rgb([220, 0, 0])
            } else {
                image::Rgb([0, 0, 220])
            }
        });
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).expect("encodes");
        out.into_inner()
    }

    /// Reads `name`, holding `bytes`, in a fresh folder: the result and
    /// what the call attached, with the folder kept for looking in.
    async fn read(
        name: &str,
        bytes: &[u8],
        takes_images: bool,
    ) -> (Result<String, String>, Vec<Image>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(name), bytes).expect("writes");
        let ctx = ToolContext {
            takes_images,
            images: Some(dir.path().join("images")),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let tool = Read::new(ReadConfig::default(), crate::post_write::lsp_off());
        let result = tool.call(json!({ "filePath": name }), &ctx).await;
        (result, ctx.attachments.take(), dir)
    }

    #[tokio::test]
    async fn an_image_is_shown_and_kept_under_its_hash() {
        let png = picture(512, 512, ImageFormat::Png);
        let (result, images, dir) = read("shot.png", &png, true).await;

        let shot = dir.path().join("shot.png");
        assert_eq!(
            result,
            Ok(format!("Image read: {}, 512×512 png", shot.display()))
        );
        let [image] = &images[..] else {
            panic!("one image, got {images:?}");
        };
        assert_eq!(
            (image.mime.as_str(), image.width, image.height),
            ("image/png", 512, 512)
        );
        assert_eq!(
            image.path,
            dir.path()
                .join("images")
                .join(format!("{}.png", image.sha256))
        );
        assert_eq!(std::fs::read(&image.path).expect("kept"), png, "as it came");
        let data = image.data.as_deref().expect("its bytes");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .expect("base64"),
            png
        );
    }

    #[tokio::test]
    async fn a_large_image_is_scaled_down_to_fit() {
        let jpeg = picture(4000, 1000, ImageFormat::Jpeg);
        let (result, images, _dir) = read("wide.jpg", &jpeg, true).await;

        let text = result.expect("read");
        assert!(text.ends_with("1920×480 jpeg (from 4000×1000)"), "{text}");
        assert_eq!((images[0].width, images[0].height), (1920, 480));
        assert_eq!(images[0].mime, "image/jpeg");
    }

    /// A `width`×`height` image of noise, which compresses about as badly
    /// as an image can.
    fn noise(width: u32, height: u32) -> image::RgbImage {
        let mut state = 0x2545_f491_u32;
        image::RgbImage::from_fn(width, height, |_, _| {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            };
            image::Rgb([next(), next(), next()])
        })
    }

    fn encoded(image: &image::RgbImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).expect("encodes");
        out.into_inner()
    }

    #[tokio::test]
    async fn a_large_photo_goes_out_as_a_jpeg_under_the_cap() {
        let jpeg = encoded(&noise(4000, 3000), ImageFormat::Jpeg);
        let (result, images, _dir) = read("photo.jpg", &jpeg, true).await;

        let text = result.expect("read");
        assert!(text.contains("jpeg (from 4000×3000)"), "{text}");
        assert_eq!(images[0].mime, "image/jpeg");
        let kept = std::fs::read(&images[0].path).expect("kept");
        assert!(kept.len() <= MAX_IMAGE_BYTES, "{} bytes", kept.len());
        assert_eq!(sniff(&kept), Some(ImageFormat::Jpeg));
    }

    #[tokio::test]
    async fn an_image_that_fits_but_weighs_too_much_is_shrunk() {
        let png = encoded(&noise(1920, 1080), ImageFormat::Png);
        assert!(png.len() > MAX_IMAGE_BYTES, "{} bytes", png.len());
        let (result, images, _dir) = read("noise.png", &png, true).await;

        let text = result.expect("read");
        assert!(text.contains("png (from 1920×1080)"), "{text}");
        assert!(images[0].width < 1920);
        let kept = std::fs::read(&images[0].path).expect("kept");
        assert!(kept.len() <= MAX_IMAGE_BYTES, "{} bytes", kept.len());
        assert_eq!(sniff(&kept), Some(ImageFormat::Png));
    }

    #[tokio::test]
    async fn an_image_claiming_too_many_pixels_is_refused_before_decoding() {
        // A real PNG with its header patched to claim 20000×20000: decoding
        // would fail on the pixels, so refusing on the size alone shows the
        // header was all that was read.
        let mut png = picture(8, 8, ImageFormat::Png);
        png[16..20].copy_from_slice(&20_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&20_000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let (result, images, _dir) = read("huge.png", &png, true).await;

        assert_eq!(
            result,
            Err("cannot show the image: 20000×20000 is over 50 megapixels".into())
        );
        assert!(images.is_empty());
    }

    /// The CRC a PNG chunk ends with, over its type and data.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[tokio::test]
    async fn a_huge_image_file_is_refused_unread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.png");
        let file = std::fs::File::create(&path).expect("creates");
        use std::io::Write;
        (&file)
            .write_all(&picture(8, 8, ImageFormat::Png)[..16])
            .expect("writes");
        // Sparse: 65 MiB on paper, a block on disk.
        file.set_len(65 * 1024 * 1024).expect("grows");
        let ctx = ToolContext {
            images: Some(dir.path().join("images")),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let tool = Read::new(ReadConfig::default(), crate::post_write::lsp_off());
        let result = tool.call(json!({ "filePath": "huge.png" }), &ctx).await;

        assert_eq!(
            result,
            Err(format!(
                "cannot show {}: the image file is 65 MiB, over the 64 MiB the read tool shows",
                path.display()
            ))
        );
    }

    #[tokio::test]
    async fn an_animated_gif_shows_its_first_frame() {
        use image::{Delay, Frame, codecs::gif::GifEncoder};
        let mut gif = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut gif);
            let frames = [[220, 0, 0, 255], [0, 0, 220, 255]].map(|colour| {
                Frame::from_parts(
                    image::RgbaImage::from_pixel(16, 16, image::Rgba(colour)),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                )
            });
            encoder.encode_frames(frames).expect("encodes");
        }
        let (result, images, _dir) = read("anim.gif", &gif, true).await;

        let text = result.expect("read");
        assert!(
            text.ends_with("16×16 png, first frame of an animation"),
            "{text}"
        );
        assert_eq!(images[0].mime, "image/png");
    }

    #[tokio::test]
    async fn without_a_store_no_image_is_shown() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("shot.png"), picture(8, 8, ImageFormat::Png))
            .expect("writes");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = Read::new(ReadConfig::default(), crate::post_write::lsp_off());
        let result = tool.call(json!({ "filePath": "shot.png" }), &ctx).await;

        assert_eq!(
            result,
            Err("cannot show images: no session store to keep them in".into())
        );
        assert!(ctx.attachments.take().is_empty());
    }

    #[tokio::test]
    async fn a_gif_becomes_a_png() {
        let gif = picture(64, 32, ImageFormat::Gif);
        let (result, images, _dir) = read("anim.gif", &gif, true).await;

        let text = result.expect("read");
        assert!(text.ends_with("64×32 png"), "a still: {text}");
        assert_eq!(images[0].mime, "image/png");
        assert!(images[0].path.extension().is_some_and(|e| e == "png"));
    }

    #[tokio::test]
    async fn a_jpeg_or_webp_that_fits_keeps_its_format() {
        for (format, mime) in [
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let (result, images, _dir) = read("pic", &picture(100, 50, format), true).await;
            assert!(result.is_ok(), "{format:?}: {result:?}");
            assert_eq!(images[0].mime, mime);
        }
    }

    #[tokio::test]
    async fn the_same_image_read_twice_is_kept_once() {
        let png = picture(10, 10, ImageFormat::Png);
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.png"), &png).expect("writes");
        std::fs::write(dir.path().join("b.png"), &png).expect("writes");
        let ctx = ToolContext {
            images: Some(dir.path().join("images")),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let tool = Read::new(ReadConfig::default(), crate::post_write::lsp_off());
        for name in ["a.png", "b.png"] {
            tool.call(json!({ "filePath": name }), &ctx)
                .await
                .expect("read");
        }
        let kept = std::fs::read_dir(dir.path().join("images"))
            .expect("folder")
            .count();
        assert_eq!(kept, 1);
    }

    #[tokio::test]
    async fn a_model_that_takes_no_images_is_refused() {
        let (result, images, dir) = read("shot.png", &picture(8, 8, ImageFormat::Png), false).await;

        let shot = dir.path().join("shot.png");
        assert_eq!(result, Err(image_unsupported(&shot.display().to_string())));
        assert!(images.is_empty());
        assert!(!dir.path().join("images").exists(), "nothing kept");
    }

    #[tokio::test]
    async fn a_broken_image_is_an_error_not_a_panic() {
        let mut png = picture(64, 64, ImageFormat::Png);
        png.truncate(60);
        let (result, images, _dir) = read("cut.png", &png, true).await;

        let error = result.expect_err("cannot decode");
        assert!(error.starts_with("cannot decode the image"), "{error}");
        assert!(images.is_empty());
    }
}
