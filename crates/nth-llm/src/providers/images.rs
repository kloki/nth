//! What a request's images become before it goes out: text where the
//! model cannot see them, and fewer of them where there are too many. The
//! history is never changed, only the request built from it.

use nth_protocol::{IMAGE_REMOVED, Image, Message, image_unsupported};

/// Past either, the oldest images go: what one request may carry before
/// some endpoints time out or refuse it (52 MiB failed in testing, 26 MiB
/// passed everywhere), and the count past which models were seen to lose
/// track of them.
const TRIGGER_BYTES: usize = 25 * 1024 * 1024;
const MAX_IMAGES: usize = 50;
/// Down to this, so the next few images do not trigger it again at once.
const TARGET_BYTES: usize = 15 * 1024 * 1024;

/// The messages to send in place of `messages`, or `None` when they carry
/// no image and go as they are. `takes_images` is what the catalogue says
/// of the model; a model it does not know is sent its images.
pub(super) fn prepare(messages: &[Message], takes_images: Option<bool>) -> Option<Vec<Message>> {
    let any = messages
        .iter()
        .any(|m| matches!(m, Message::ToolResult { images, .. } if !images.is_empty()));
    if !any {
        return None;
    }
    let mut messages = messages.to_vec();
    replace(&mut messages, |image| match (&image.data, takes_images) {
        // Its file was gone when the session loaded.
        (None, _) => Some(IMAGE_REMOVED.to_string()),
        (Some(_), Some(false)) => Some(image_unsupported(&image.path.display().to_string())),
        _ => None,
    });
    bound(&mut messages);
    Some(messages)
}

/// Replaces the oldest images with the removal note while the request has
/// more than `TRIGGER_BYTES` or `MAX_IMAGES`, until it is within
/// `TARGET_BYTES` and `MAX_IMAGES`.
fn bound(messages: &mut [Message]) {
    let sizes: Vec<usize> = images(messages).map(size).collect();
    let (mut bytes, mut count) = (sizes.iter().sum::<usize>(), sizes.len());
    if bytes <= TRIGGER_BYTES && count <= MAX_IMAGES {
        return;
    }
    replace(messages, |image| {
        if bytes <= TARGET_BYTES && count <= MAX_IMAGES {
            return None;
        }
        bytes -= size(image);
        count -= 1;
        Some(IMAGE_REMOVED.to_string())
    });
}

fn size(image: &Image) -> usize {
    image.data.as_deref().map_or(0, str::len)
}

fn images(messages: &[Message]) -> impl Iterator<Item = &Image> {
    messages.iter().flat_map(|m| match m {
        Message::ToolResult { images, .. } => images.as_slice(),
        _ => &[],
    })
}

/// Goes through the images oldest first; each `text` returns is put in the
/// result's text in the image's place, and the image is dropped.
fn replace(messages: &mut [Message], mut text: impl FnMut(&Image) -> Option<String>) {
    for message in messages {
        let Message::ToolResult {
            content, images, ..
        } = message
        else {
            continue;
        };
        images.retain(|image| match text(image) {
            Some(note) => {
                content.push_str("\n\n");
                content.push_str(&note);
                false
            }
            None => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(name: &str, bytes: usize) -> Image {
        Image {
            mime: "image/png".into(),
            sha256: name.into(),
            width: 1,
            height: 1,
            path: format!("/images/{name}.png").into(),
            data: Some("A".repeat(bytes).into()),
        }
    }

    fn result(images: Vec<Image>) -> Message {
        Message::ToolResult {
            call_id: "1".into(),
            content: "Image read".into(),
            images,
        }
    }

    fn kept(messages: &[Message]) -> Vec<String> {
        images(messages).map(|i| i.sha256.clone()).collect()
    }

    #[test]
    fn a_request_without_images_goes_as_it_is() {
        let messages = [Message::User("hi".into()), result(Vec::new())];
        assert_eq!(prepare(&messages, Some(false)), None);
    }

    #[test]
    fn a_model_that_takes_no_images_is_told_so() {
        let messages = prepare(&[result(vec![image("a", 4)])], Some(false)).expect("rewritten");
        assert_eq!(
            messages,
            [Message::ToolResult {
                call_id: "1".into(),
                content: format!("Image read\n\n{}", image_unsupported("/images/a.png")),
                images: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_model_that_takes_them_or_is_unknown_gets_them() {
        for takes in [Some(true), None] {
            let messages = prepare(&[result(vec![image("a", 4)])], takes).expect("rewritten");
            assert_eq!(kept(&messages), ["a"], "{takes:?}");
        }
    }

    #[test]
    fn an_image_whose_file_was_gone_becomes_the_note() {
        let mut gone = image("a", 4);
        gone.data = None;
        let messages = prepare(&[result(vec![gone])], Some(true)).expect("rewritten");
        let Message::ToolResult {
            content, images, ..
        } = &messages[0]
        else {
            panic!("a tool result");
        };
        assert_eq!(content, &format!("Image read\n\n{IMAGE_REMOVED}"));
        assert!(images.is_empty());
    }

    #[test]
    fn too_many_bytes_drop_the_oldest_down_to_the_target() {
        let mib = 1024 * 1024;
        let messages = [
            result(vec![image("old", 10 * mib)]),
            Message::User("again".into()),
            result(vec![image("mid", 10 * mib), image("new", 10 * mib)]),
        ];
        let messages = prepare(&messages, Some(true)).expect("rewritten");
        assert_eq!(kept(&messages), ["new"], "30 MiB down to 10");
    }

    #[test]
    fn too_many_images_drop_the_oldest() {
        let images: Vec<_> = (0..MAX_IMAGES + 2)
            .map(|i| image(&i.to_string(), 1))
            .collect();
        let messages = prepare(&[result(images)], Some(true)).expect("rewritten");
        let kept = kept(&messages);
        assert_eq!(kept.len(), MAX_IMAGES);
        assert_eq!(kept[0], "2", "the two oldest went");
    }

    #[test]
    fn within_both_bounds_nothing_goes() {
        let images: Vec<_> = (0..MAX_IMAGES).map(|i| image(&i.to_string(), 1)).collect();
        let messages = prepare(&[result(images)], Some(true)).expect("rewritten");
        assert_eq!(kept(&messages).len(), MAX_IMAGES);
    }
}
