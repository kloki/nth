//! The YAML block at the top of a `SKILL.md`, between two `---` lines.
//! Only `name` and `description` are read; other keys, such as Claude
//! Code's `allowed-tools`, are ignored.

use serde_norway::Value;

#[derive(Debug, Default, PartialEq)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// The frontmatter of `text` and the body after it. A file without one is
/// all body.
pub fn split(text: &str) -> Result<(Frontmatter, &str), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((Frontmatter::default(), text));
    };
    let (yaml, body) = close(rest).ok_or("frontmatter has no closing ---")?;
    let value = match serde_norway::from_str::<Value>(yaml) {
        Ok(value) => value,
        // Retry the way opencode does: plain values with a colon in them
        // are the usual reason hand-written frontmatter is not valid YAML.
        Err(first) => serde_norway::from_str::<Value>(&sanitize(yaml))
            .map_err(|_| format!("frontmatter is not valid YAML: {first}"))?,
    };
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let frontmatter = Frontmatter {
        name: field("name"),
        description: field("description"),
    };
    Ok((frontmatter, body))
}

/// Splits at the closing `---` line into the YAML and the body after it.
fn close(rest: &str) -> Option<(&str, &str)> {
    let mut start = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..start], &rest[start + line.len()..]));
        }
        start += line.len();
    }
    None
}

/// Turns `key: value: with colons` into a block scalar, which YAML takes
/// as plain text.
fn sanitize(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len());
    for line in yaml.lines() {
        match top_level_pair(line) {
            Some((key, value)) if value.contains(':') && !is_structured(value) => {
                out.push_str(&format!("{key}: |-\n  {value}\n"));
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// `key: value` at the start of a line, with a plain identifier as key.
fn top_level_pair(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let identifier = key.chars().enumerate().all(|(i, c)| {
        c == '_' || c == '-' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
    });
    (identifier && !key.is_empty()).then(|| (key, value.trim()))
}

/// Values YAML already reads as something other than a plain string.
fn is_structured(value: &str) -> bool {
    value.starts_with(['"', '\'', '|', '>', '[', '{'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(name: &str, description: &str) -> Frontmatter {
        Frontmatter {
            name: Some(name.into()),
            description: Some(description.into()),
        }
    }

    #[test]
    fn reads_name_description_and_body() {
        let text = "---\nname: review\ndescription: Review a diff\nallowed-tools: [Read]\n---\n# Review\n\nSteps.\n";

        assert_eq!(
            split(text),
            Ok((front("review", "Review a diff"), "# Review\n\nSteps.\n"))
        );
    }

    #[test]
    fn block_scalars_and_crlf_are_fine() {
        let text = "---\r\nname: review\r\ndescription: >-\r\n  Review a diff\r\n  carefully\r\n---\r\nbody";

        assert_eq!(
            split(text),
            Ok((front("review", "Review a diff carefully"), "body"))
        );
    }

    #[test]
    fn unquoted_colons_are_rescued() {
        let text =
            "---\nname: research\ndescription: Use when: the user asks about opencode\n---\nbody";

        assert_eq!(
            split(text),
            Ok((
                front("research", "Use when: the user asks about opencode"),
                "body"
            ))
        );
    }

    #[test]
    fn without_frontmatter_it_is_all_body() {
        assert_eq!(
            split("# Just markdown\n"),
            Ok((Frontmatter::default(), "# Just markdown\n"))
        );
        assert_eq!(
            split("---\n---\nbody"),
            Ok((Frontmatter::default(), "body"))
        );
    }

    #[test]
    fn broken_frontmatter_is_an_error() {
        assert!(split("---\nname: x\n").is_err());
        assert!(split("---\nname: [unclosed\n---\nbody").is_err());
    }
}
