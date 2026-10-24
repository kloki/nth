use std::collections::HashSet;

use base64::Engine;
use scraper::{Html, Selector};

use crate::{Anchor, Error, Result};

pub const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

// Deliberately conservative: no CSS resource functions, escapes, or at-rules
// other than ordinary layout rules. CSP independently enforces this boundary.
fn css(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    if lower.contains('\\')
        || lower.contains("url")
        || lower.contains("@import")
        || lower.contains("@font-face")
        || lower.contains("expression")
        || lower.contains("behavior")
        || lower.contains("image-set")
        || lower.contains("/*")
    {
        return Err(invalid(
            "CSS resources, escapes and executable forms are unsupported",
        ));
    }
    Ok(())
}

fn raster(header: &str, data: &str) -> Result<()> {
    use image::{ImageFormat, ImageReader, Limits};
    let expected = match header {
        "data:image/png;base64" => ImageFormat::Png,
        "data:image/jpeg;base64" => ImageFormat::Jpeg,
        "data:image/gif;base64" => ImageFormat::Gif,
        "data:image/webp;base64" => ImageFormat::WebP,
        _ => return Err(invalid("Only embedded raster images are allowed")),
    };
    if data.len() > 2 * 1024 * 1024 {
        return Err(invalid("Embedded image exceeds byte limit"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| invalid("Invalid image base64"))?;
    let actual = image::guess_format(&bytes).map_err(|_| invalid("Invalid raster header"))?;
    if actual != expected {
        return Err(invalid("Image MIME type does not match its bytes"));
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    let mut reader = ImageReader::with_format(std::io::Cursor::new(&bytes), actual);
    reader.limits(limits.clone());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| invalid("Invalid image dimensions"))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 16 * 1024 * 1024 {
        return Err(invalid("Image exceeds pixel limit"));
    }
    let mut reader = ImageReader::with_format(std::io::Cursor::new(&bytes), actual);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|_| invalid("Invalid or oversized raster image"))?;
    Ok(())
}

fn presentation_resource(value: &str) -> Result<()> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if value.contains(['\\', '\0']) || lower.contains("/*") {
        return Err(invalid("Escaped SVG resource forms are unsupported"));
    }
    if lower.contains("url") {
        // Exactly one local reference, with no fallback, extra function or token.
        let local = value
            .strip_prefix("url(#")
            .and_then(|v| v.strip_suffix(')'));
        if !local.is_some_and(|id| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        }) {
            return Err(invalid(
                "Only a single local SVG resource reference is allowed",
            ));
        }
    }
    Ok(())
}

pub fn validate(html: &str) -> Result<Vec<Anchor>> {
    if html.len() > MAX_HTML_BYTES || html.contains('\0') {
        return Err(invalid("HTML exceeds limit or contains NUL"));
    }
    let doc = Html::parse_document(html);
    let selector = Selector::parse("*").expect("static selector");
    let mut ids = HashSet::new();
    let mut anchors = Vec::new();
    for element in doc.select(&selector) {
        let name = element.value().name();
        if matches!(
            name,
            "script"
                | "iframe"
                | "frame"
                | "frameset"
                | "object"
                | "embed"
                | "applet"
                | "base"
                | "link"
                | "form"
                | "input"
                | "button"
                | "textarea"
                | "select"
                | "audio"
                | "video"
                | "source"
                | "track"
                | "foreignObject"
                | "foreignobject"
                | "animate"
                | "animateMotion"
                | "animateTransform"
                | "set"
        ) {
            return Err(invalid(
                "Executable, embedded or interactive HTML is unsupported",
            ));
        }
        if name == "meta" && element.value().attr("http-equiv").is_some() {
            return Err(invalid("HTTP-equiv metadata is unsupported"));
        }
        for (key, value) in element.value().attrs() {
            let key = key.to_ascii_lowercase();
            if key.starts_with("on")
                || matches!(
                    key.as_str(),
                    "srcdoc"
                        | "srcset"
                        | "action"
                        | "formaction"
                        | "ping"
                        | "autofocus"
                        | "contenteditable"
                        | "is"
                )
            {
                return Err(invalid("Active attributes are unsupported"));
            }
            if key == "style" {
                css(value)?;
            }
            if matches!(
                key.as_str(),
                "href" | "xlink:href" | "src" | "poster" | "background"
            ) {
                if value.starts_with('#') && key != "src" {
                    continue;
                }
                if name == "img" && key == "src" {
                    let Some((header, data)) = value.split_once(',') else {
                        return Err(invalid("Only embedded raster images are allowed"));
                    };
                    raster(header, data)?;
                } else {
                    return Err(invalid("External resources and navigation are unsupported"));
                }
            }
            if matches!(
                key.as_str(),
                "fill"
                    | "stroke"
                    | "filter"
                    | "clip-path"
                    | "mask"
                    | "cursor"
                    | "marker"
                    | "marker-start"
                    | "marker-mid"
                    | "marker-end"
                    | "color-profile"
                    | "shape-inside"
                    | "shape-outside"
                    | "offset-path"
                    | "motion-path"
                    | "background-image"
                    | "border-image-source"
                    | "list-style-image"
            ) || (key != "src" && key != "style" && value.to_ascii_lowercase().contains("url"))
            {
                presentation_resource(value)?;
            }
        }
        if name == "style" {
            css(&element.text().collect::<String>())?;
        }
        if let Some(id) = element.value().attr("id") {
            if id.is_empty() || id.len() > 160 || !ids.insert(id.to_owned()) {
                return Err(invalid(
                    "Target IDs must be unique, nonempty and at most 160 bytes",
                ));
            }
            let label = element
                .value()
                .attr("aria-label")
                .map(str::to_owned)
                .unwrap_or_else(|| element.text().collect::<Vec<_>>().join(" "));
            anchors.push(Anchor {
                id: id.into(),
                label: label
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(120)
                    .collect(),
            });
        }
    }
    if anchors.len() > 4096 {
        return Err(invalid("Too many anchors"));
    }
    Ok(anchors)
}
