//! An image file read for the model to see: downscaled to what models still
//! read text in, kept in the session's image folder under its hash, and
//! attached to the call beside a line saying what was read.

use std::{
    io::Cursor,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use base64::Engine;
use image::{GenericImageView, ImageFormat, imageops::FilterType};
use nth_protocol::{Image, ToolContext, image_unsupported};
use sha2::{Digest, Sha256};

/// The longest side an image is shown at: a 4K screen's size at 2× scale,
/// where every model tested still read normal text, at about 2,500 tokens.
/// Larger costs some models four times as much and reads no better.
const MAX_SIDE: u32 = 1920;

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
    // Decoding and resizing are CPU work with no await in them, about
    // 100 ms for a 4K screenshot: too long to hold a runtime worker.
    let shown = tokio::task::spawn_blocking(move || prepare(bytes, format))
        .await
        .map_err(|e| format!("reading the image failed: {e}"))??;
    let file = ctx.images.join(format!("{}.{}", shown.sha256, shown.ext));
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
    sha256: String,
}

/// Downscales an image past `MAX_SIDE`, and turns a GIF into a PNG, since
/// not every provider takes GIF (xAI refuses it). Anything else goes as it
/// came.
fn prepare(bytes: Vec<u8>, format: ImageFormat) -> Result<Shown, String> {
    let image = image::load_from_memory_with_format(&bytes, format)
        .map_err(|e| format!("cannot decode the image: {e}"))?;
    let original = image.dimensions();
    let too_big = original.0.max(original.1) > MAX_SIDE;
    let (bytes, format, size) = if too_big || format == ImageFormat::Gif {
        let image = if too_big {
            image.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle)
        } else {
            image
        };
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|e| format!("cannot encode the image: {e}"))?;
        (png.into_inner(), ImageFormat::Png, image.dimensions())
    } else {
        (bytes, format, original)
    };
    let (mime, ext) = match format {
        ImageFormat::Jpeg => ("image/jpeg", "jpeg"),
        ImageFormat::WebP => ("image/webp", "webp"),
        _ => ("image/png", "png"),
    };
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Shown {
        bytes,
        mime,
        ext,
        width: size.0,
        height: size.1,
        original,
        sha256,
    })
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
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, file).await
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
            images: dir.path().join("images"),
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
        assert!(text.ends_with("1920×480 png (from 4000×1000)"), "{text}");
        assert_eq!((images[0].width, images[0].height), (1920, 480));
        assert_eq!(images[0].mime, "image/png");
    }

    #[tokio::test]
    async fn a_gif_becomes_a_png() {
        let gif = picture(64, 32, ImageFormat::Gif);
        let (result, images, _dir) = read("anim.gif", &gif, true).await;

        assert!(result.expect("read").ends_with("64×32 png"));
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
            images: dir.path().join("images"),
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
