//! The message ctrl+g sends after the user edited a copy of the plan: the
//! diff of their edits, and what to do with them. Rendered and parsed
//! here together, so the chat can show it as one row.

use std::path::Path;

use similar::{ChangeTag, TextDiff};

const INSTRUCTION: &str = include_str!("plan_edits.md");
const OPEN: &str = "<plan-edits ";

/// What the chat shows of a plan-edits message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanEdits {
    pub added: usize,
    pub removed: usize,
}

/// The message for the user's edits of `plan`, from `original` to
/// `edited`.
pub fn render(plan: &Path, original: &str, edited: &str) -> String {
    let diff = TextDiff::from_lines(original, edited);
    let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
    let (added, removed) = (count(ChangeTag::Insert), count(ChangeTag::Delete));
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header("plan", "edited copy")
        .to_string();
    let path = plan.display().to_string();
    format!(
        "{OPEN}path=\"{path}\" added=\"{added}\" removed=\"{removed}\">\n{}\n</plan-edits>\n\n{}",
        unified.trim_end(),
        INSTRUCTION.replace("{path}", &path).trim_end()
    )
}

/// The counts of a message [`render`] made; `None` for any other text.
pub fn parse(text: &str) -> Option<PlanEdits> {
    let rest = text.strip_prefix(OPEN)?;
    let tag = &rest[..rest.find('>')?];
    Some(PlanEdits {
        added: value(tag, "added")?.parse().ok()?,
        removed: value(tag, "removed")?.parse().ok()?,
    })
}

/// The value of `name="…"` among `attrs`.
fn value<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let start = attrs.find(&format!(" {name}=\""))? + name.len() + 3;
    let len = attrs[start..].find('"')?;
    Some(&attrs[start..start + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_the_diff_and_the_instruction_and_parses_back() {
        let text = render(
            Path::new("/repo/.nth/plans/1.md"),
            "# Plan\n1. Add retry.\n2. Test it.\n",
            "# Plan\n1. Add retry.\n<!-- why not backoff? -->\n2. Test it.\n",
        );

        assert!(text.starts_with(
            "<plan-edits path=\"/repo/.nth/plans/1.md\" added=\"1\" removed=\"0\">\n--- plan\n+++ edited copy\n"
        ));
        assert!(text.contains("\n+<!-- why not backoff? -->\n"), "{text}");
        assert!(text.contains("Edit the plan file at /repo/.nth/plans/1.md with the edit tool"));
        assert!(!text.contains("{path}"));
        assert_eq!(
            parse(&text),
            Some(PlanEdits {
                added: 1,
                removed: 0
            })
        );
    }

    #[test]
    fn other_text_is_not_plan_edits() {
        assert_eq!(parse("fix it"), None);
        assert_eq!(parse("<plan-edits path=\"x\">"), None);
    }
}
