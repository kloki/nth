//! The notice a monitor's output becomes: a `<monitor>` element the model
//! reads, and what the transcript reads back out of it. Both sides are
//! here so they cannot drift apart.

use std::path::Path;

use super::{MonitorEnd, MonitorId};

/// At most this many lines of one monitor go into a notice; the rest are
/// counted, and the log has them.
pub const NOTICE_LINES: usize = 50;

/// The lines in a `<monitor>` element, then how it ended in an empty one.
pub(super) fn render(
    id: MonitorId,
    description: &str,
    log: &Path,
    lines: &[String],
    more: usize,
    ended: Option<(MonitorEnd, usize)>,
) -> String {
    let attributes = format!(
        "id=\"{id}\" description=\"{}\" log=\"{}\"",
        attribute(description),
        attribute(&log.display().to_string()),
    );
    let mut out = Vec::new();
    if !lines.is_empty() {
        out.push(format!("<monitor {attributes}>"));
        out.extend(lines.iter().cloned());
        if more > 0 {
            out.push(format!("… {more} more lines in the log"));
        }
        out.push("</monitor>".into());
    }
    if let Some((end, events)) = ended {
        out.push(format!(
            "<monitor {attributes} ended=\"{}\" events=\"{events}\"/>",
            attribute(&end.to_string())
        ));
    }
    out.join("\n")
}

fn attribute(text: &str) -> String {
    text.replace('&', "&amp;").replace('"', "&quot;")
}

/// A monitor's notice as the transcript shows it, from the text the model
/// read: which monitor, and how many lines or how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeSummary {
    pub id: MonitorId,
    pub description: String,
    pub lines: usize,
    pub ended: Option<String>,
}

/// Splits the notices off the front of a user message: what a monitor
/// said, then what the user typed after it, if anything.
pub fn split_notices(text: &str) -> (Vec<NoticeSummary>, &str) {
    let mut notices = Vec::new();
    let mut rest = text;
    while let Some(after) = rest.strip_prefix("<monitor ") {
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag = &after[..tag_end];
        let (attrs, closed) = match tag.strip_suffix('/') {
            Some(attrs) => (attrs, true),
            None => (tag, false),
        };
        let Some(id) = value(attrs, "id").and_then(|id| id.parse().ok()) else {
            break;
        };
        let description = value(attrs, "description").unwrap_or_default();
        let body = &after[tag_end + 1..];
        let (lines, next) = if closed {
            (0, body)
        } else {
            let Some(close) = body.find("\n</monitor>") else {
                break;
            };
            let lines = body[..close].lines().filter(|l| !l.is_empty()).count();
            (lines, &body[close + "\n</monitor>".len()..])
        };
        notices.push(NoticeSummary {
            id,
            description,
            lines,
            ended: value(attrs, "ended"),
        });
        rest = next.strip_prefix('\n').unwrap_or(next);
    }
    if notices.is_empty() {
        return (notices, text);
    }
    (notices, rest.trim_start_matches('\n'))
}

fn value(attrs: &str, name: &str) -> Option<String> {
    let start = attrs.find(&format!("{name}=\""))? + name.len() + 2;
    let len = attrs[start..].find('"')?;
    Some(
        attrs[start..start + len]
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_split_off_the_front_of_a_message() {
        let log = Path::new("/logs/1.log");
        let lines = ["step 1 ok".to_string(), "step 2 ok".to_string()];
        let output = render(1, "ci \"main\"", log, &lines, 0, None);
        let ended = render(
            1,
            "ci \"main\"",
            log,
            &[],
            0,
            Some((MonitorEnd::Exited(Some(0)), 2)),
        );
        let text = format!("{output}\n{ended}\n\nnow fix it");

        let (notices, rest) = split_notices(&text);
        assert_eq!(
            notices,
            [
                NoticeSummary {
                    id: 1,
                    description: "ci \"main\"".into(),
                    lines: 2,
                    ended: None,
                },
                NoticeSummary {
                    id: 1,
                    description: "ci \"main\"".into(),
                    lines: 0,
                    ended: Some("exited with code 0".into()),
                },
            ]
        );
        assert_eq!(rest, "now fix it");
        assert_eq!(split_notices("hi"), (vec![], "hi"));
    }
}
