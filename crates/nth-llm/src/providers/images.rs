//! What a request's images become before it goes out: text where the
//! model cannot see them or one is too large, and fewer of them where
//! there are too many. The
//! history is never changed, only the request built from it.

use nth_protocol::{
    IMAGE_REMOVED, Image, MAX_IMAGE_BYTES, Message, image_too_large, image_unsupported,
};

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
        (Some(data), _) if data.len() * 3 / 4 > MAX_IMAGE_BYTES => {
            Some(image_too_large(&image.path.display().to_string()))
        }
        _ => None,
    });
    bound(&mut messages);
    Some(messages)
}

/// Replaces the oldest images with the removal note while the request has
/// more than `TRIGGER_BYTES` or `MAX_IMAGES`, until it is within
/// `TARGET_BYTES` and `MAX_IMAGES`. The latest run of tool results is what
/// the model just asked to see, so its images go only when they alone are
/// past either bound, the largest first, until they are within.
fn bound(messages: &mut [Message]) {
    let sizes: Vec<usize> = images(messages).map(size).collect();
    let (mut bytes, mut count) = (sizes.iter().sum::<usize>(), sizes.len());
    let within = |bytes: usize, count: usize| bytes <= TRIGGER_BYTES && count <= MAX_IMAGES;
    if within(bytes, count) {
        return;
    }
    let latest = messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant(_)))
        .map_or(0, |at| at + 1);
    let (older, latest) = messages.split_at_mut(latest);
    replace(older, |image| {
        if bytes <= TARGET_BYTES && count <= MAX_IMAGES {
            return None;
        }
        bytes -= size(image);
        count -= 1;
        Some(IMAGE_REMOVED.to_string())
    });
    if within(bytes, count) {
        return;
    }
    let mut largest: Vec<(usize, usize)> = images(latest).map(size).enumerate().collect();
    // Stable, so of two the same size the older goes first.
    largest.sort_by_key(|&(_, size)| std::cmp::Reverse(size));
    let mut gone = vec![false; largest.len()];
    for (at, size) in largest {
        if within(bytes, count) {
            break;
        }
        gone[at] = true;
        bytes -= size;
        count -= 1;
    }
    let mut at = 0;
    replace(latest, |_| {
        at += 1;
        gone[at - 1].then(|| IMAGE_REMOVED.to_string())
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
    fn an_image_over_the_cap_becomes_the_note() {
        let bytes = MAX_IMAGE_BYTES * 4 / 3 + 4;
        let messages = prepare(&[result(vec![image("a", bytes)])], Some(true)).expect("rewritten");
        let Message::ToolResult {
            content, images, ..
        } = &messages[0]
        else {
            panic!("a tool result");
        };
        assert_eq!(
            content,
            &format!("Image read\n\n{}", image_too_large("/images/a.png"))
        );
        assert!(images.is_empty());
    }

    fn assistant() -> Message {
        Message::Assistant(Default::default())
    }

    fn named(prefix: &str, count: usize, bytes: usize) -> Vec<Image> {
        (0..count)
            .map(|i| image(&format!("{prefix}{i}"), bytes))
            .collect()
    }

    const MIB: usize = 1024 * 1024;

    #[test]
    fn too_many_bytes_drop_the_oldest_down_to_the_target() {
        let messages = [
            result(named("old", 6, 3 * MIB)),
            assistant(),
            result(named("new", 3, 3 * MIB)),
        ];
        let messages = prepare(&messages, Some(true)).expect("rewritten");
        assert_eq!(
            kept(&messages),
            ["old4", "old5", "new0", "new1", "new2"],
            "27 MiB down to 15"
        );
    }

    #[test]
    fn the_latest_run_survives_what_came_before() {
        let messages = [
            result(named("old", 4, 2560 * 1024)),
            assistant(),
            result(named("new", 6, 3 * MIB)),
        ];
        let messages = prepare(&messages, Some(true)).expect("rewritten");
        assert_eq!(
            kept(&messages),
            ["new0", "new1", "new2", "new3", "new4", "new5"],
            "the old 10 MiB go, the new 18 MiB stay though past the target"
        );
    }

    #[test]
    fn a_latest_run_too_large_alone_loses_its_largest() {
        let mut new = named("big", 9, 3 * MIB);
        new.insert(4, image("small", MIB));
        let messages = [result(named("old", 1, MIB)), assistant(), result(new)];
        let messages = prepare(&messages, Some(true)).expect("rewritten");
        assert_eq!(
            kept(&messages),
            [
                "big1", "big2", "big3", "small", "big4", "big5", "big6", "big7", "big8"
            ],
            "the old one, then the oldest of the largest: 25 MiB left"
        );
    }

    #[test]
    fn too_many_images_drop_the_oldest() {
        let messages = [
            result(named("old", 2, 1)),
            assistant(),
            result(named("new", MAX_IMAGES, 1)),
        ];
        let messages = prepare(&messages, Some(true)).expect("rewritten");
        let kept = kept(&messages);
        assert_eq!(kept.len(), MAX_IMAGES);
        assert_eq!(kept[0], "new0", "the two old ones went");
    }

    #[test]
    fn too_many_images_in_the_latest_run_drop_its_oldest() {
        let messages =
            prepare(&[result(named("", MAX_IMAGES + 2, 1))], Some(true)).expect("rewritten");
        let kept = kept(&messages);
        assert_eq!(kept.len(), MAX_IMAGES);
        assert_eq!(kept[0], "2", "the two oldest went");
    }

    #[test]
    fn within_both_bounds_nothing_goes() {
        let messages = prepare(&[result(named("", MAX_IMAGES, 1))], Some(true)).expect("rewritten");
        assert_eq!(kept(&messages).len(), MAX_IMAGES);
    }
}
