//! Images a tool shows the model, as the read tool does a screenshot. The
//! bytes live in a file of their own beside the session (`path`), never in
//! the session's JSON; in memory they ride along base64-encoded (`data`),
//! filled by the tool that read them or by the store that loaded them.

use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

/// What stands where an image was when the request had too many, or its
/// file is gone: opencode's words, so the model neither pretends to see it
/// nor gives up on getting it back.
pub const REMOVED: &str = "[This image was removed to reduce the request size and is no longer visible. Do not make claims about its contents from memory. If needed, retrieve it again with an available tool or ask the user to attach it again.]";

/// What a model that takes no images gets instead of `name`, an image:
/// opencode's words, which make it tell you rather than guess.
pub fn unsupported(name: &str) -> String {
    format!(
        "ERROR: Cannot read \"{name}\" (this model does not support image input). Inform the user."
    )
}

/// What a model that takes images gets instead of `name`, an image too
/// large to send: it can still look at part of it.
pub fn too_large(name: &str) -> String {
    format!(
        "ERROR: Cannot show \"{name}\" (the image is too large to send). Read a smaller region or crop of it instead."
    )
}

/// The most bytes one image may have: base64 makes 3,750,000 bytes 5 MB,
/// the most Anthropic takes of one image, and the others take more.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// `image/png`, `image/jpeg` or `image/webp`: what every provider takes.
    pub mime: String,
    /// Of the bytes, hex; names the file, so an image read twice is kept once.
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    /// The file holding the bytes.
    pub path: PathBuf,
    /// The bytes, base64; empty when the file was gone at load.
    #[serde(skip)]
    pub data: Option<Arc<str>>,
}

impl Image {
    /// The bytes as a `data:` URL, as OpenAI's protocols take an image.
    pub fn data_url(&self) -> Option<String> {
        let data = self.data.as_deref()?;
        Some(format!("data:{};base64,{data}", self.mime))
    }
}

/// The bytes are left out: they run to megabytes.
impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("mime", &self.mime)
            .field("sha256", &self.sha256)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("path", &self.path)
            .field("data", &self.data.as_ref().map(|d| d.len()))
            .finish()
    }
}

/// The images one tool call shows the model, beside its text result. Each
/// call gets its own, so calls running in parallel never mix theirs. The
/// lock is held only to push or take, never across an await.
#[derive(Debug, Clone, Default)]
pub struct Attachments(Arc<Mutex<Vec<Image>>>);

impl Attachments {
    pub fn add(&self, image: Image) {
        self.0
            .lock()
            .expect("attachments lock poisoned")
            .push(image);
    }

    /// The images added so far, leaving none.
    pub fn take(&self) -> Vec<Image> {
        std::mem::take(&mut *self.0.lock().expect("attachments lock poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Image {
        Image {
            mime: "image/png".into(),
            sha256: "ab".into(),
            width: 2,
            height: 1,
            path: "/data/ab.png".into(),
            data: Some("iVBO".into()),
        }
    }

    #[test]
    fn saves_without_the_bytes() {
        let json = serde_json::to_value(image()).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "mime": "image/png", "sha256": "ab", "width": 2, "height": 1,
                "path": "/data/ab.png",
            })
        );
        let back: Image = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back.data, None, "filled again by whoever loads it");
    }

    #[test]
    fn carries_its_bytes_as_a_data_url() {
        assert_eq!(
            image().data_url().as_deref(),
            Some("data:image/png;base64,iVBO")
        );
    }

    #[test]
    fn a_call_takes_what_it_attached_once() {
        let attachments = Attachments::default();
        attachments.clone().add(image());
        assert_eq!(attachments.take(), [image()]);
        assert_eq!(attachments.take(), []);
    }
}
