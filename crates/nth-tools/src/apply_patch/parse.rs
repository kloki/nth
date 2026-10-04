//! Parses the patch format of opencode's `patch/index.ts`, which comes from
//! Codex: file sections between `*** Begin Patch` and `*** End Patch`.
//! Where opencode skips a line it does not understand, this fails instead,
//! since a skipped line is a change the model believes it made.

#[derive(Debug, PartialEq)]
pub(crate) enum Section {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Hunk {
    /// The text after `@@`: a line the hunk comes after.
    pub context: Option<String>,
    /// The context and removed lines, as the file has them.
    pub old: Vec<String>,
    /// The context and added lines, as the file will have them.
    pub new: Vec<String>,
    /// `*** End of File` followed the hunk, so its old lines end the file.
    pub end_of_file: bool,
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File:";
const DELETE: &str = "*** Delete File:";
const UPDATE: &str = "*** Update File:";
const MOVE: &str = "*** Move to:";
const END_OF_FILE: &str = "*** End of File";

pub(crate) fn parse(patch: &str) -> Result<Vec<Section>, String> {
    let lines: Vec<&str> = strip_heredoc(patch.trim()).lines().collect();
    let begin = lines
        .iter()
        .position(|line| line.trim() == BEGIN)
        .ok_or_else(|| format!("it must start with `{BEGIN}`"))?;
    let end = lines[begin..]
        .iter()
        .position(|line| line.trim() == END)
        .map(|i| begin + i)
        .ok_or_else(|| format!("it must end with `{END}`"))?;

    let mut lines = &lines[begin + 1..end];
    let mut sections = Vec::new();
    while let Some((&line, rest)) = lines.split_first() {
        lines = rest;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(path) = header(line, ADD)? {
            let content = added(take_body(&mut lines, false))?;
            sections.push(Section::Add { path, content });
        } else if let Some(path) = header(line, DELETE)? {
            if !take_body(&mut lines, false).is_empty() {
                return Err(format!("`{line}` must not be followed by lines"));
            }
            sections.push(Section::Delete { path });
        } else if let Some(path) = header(line, UPDATE)? {
            let move_to = match lines.split_first() {
                Some((&next, rest)) if next.starts_with(MOVE) => {
                    lines = rest;
                    header(next, MOVE)?
                }
                _ => None,
            };
            let hunks = hunks(take_body(&mut lines, true))?;
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("`{line}` has no hunks"));
            }
            sections.push(Section::Update {
                path,
                move_to,
                hunks,
            });
        } else {
            return Err(format!(
                "expected `{ADD}`, `{UPDATE}` or `{DELETE}`, got `{line}`"
            ));
        }
    }
    if sections.is_empty() {
        return Err("it changes no files".into());
    }
    Ok(sections)
}

/// The path after `prefix`, if `line` starts with it.
fn header(line: &str, prefix: &str) -> Result<Option<String>, String> {
    let Some(path) = line.strip_prefix(prefix) else {
        return Ok(None);
    };
    match path.trim() {
        "" => Err(format!("`{line}` names no file")),
        path => Ok(Some(path.to_string())),
    }
}

/// The lines up to the next `***` header, without trailing empty ones,
/// which are spacing between sections rather than content.
fn take_body<'a, 'b>(lines: &mut &'a [&'b str], update: bool) -> &'a [&'b str] {
    let n = lines
        .iter()
        .position(|line| line.starts_with("***") && !(update && *line == END_OF_FILE))
        .unwrap_or(lines.len());
    let (mut body, rest) = lines.split_at(n);
    *lines = rest;
    while let Some((last, init)) = body.split_last()
        && last.is_empty()
    {
        body = init;
    }
    body
}

/// The content of an added file, whose lines each start with `+`. An empty
/// line is taken as an empty `+` line.
fn added(body: &[&str]) -> Result<String, String> {
    let mut content = String::new();
    for line in body {
        let line = match line.strip_prefix('+') {
            Some(line) => line,
            None if line.is_empty() => "",
            None => {
                return Err(format!(
                    "expected `+` at the start of `{line}` in an added file"
                ));
            }
        };
        content.push_str(line);
        content.push('\n');
    }
    Ok(content)
}

/// An empty line counts as an empty context line, as models often drop the
/// space in front of one. Lines before the first `@@` form a hunk without
/// context.
fn hunks(body: &[&str]) -> Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for &line in body {
        if let Some(context) = line.strip_prefix("@@") {
            hunks.extend(current.take());
            let context = context.trim();
            current = Some(Hunk {
                context: (!context.is_empty()).then(|| context.to_string()),
                ..Hunk::default()
            });
        } else if line == END_OF_FILE {
            let mut hunk = current
                .take()
                .ok_or_else(|| format!("`{END_OF_FILE}` must follow a hunk"))?;
            hunk.end_of_file = true;
            hunks.push(hunk);
        } else if line.is_empty() && current.is_none() {
            continue;
        } else {
            let hunk = current.get_or_insert_with(Hunk::default);
            // Slicing after the first byte is safe: it is ASCII.
            match line.as_bytes().first() {
                None => {
                    hunk.old.push(String::new());
                    hunk.new.push(String::new());
                }
                Some(b' ') => {
                    hunk.old.push(line[1..].to_string());
                    hunk.new.push(line[1..].to_string());
                }
                Some(b'-') => hunk.old.push(line[1..].to_string()),
                Some(b'+') => hunk.new.push(line[1..].to_string()),
                Some(_) => {
                    return Err(format!(
                        "expected `@@`, or a line starting with ` `, `-` or `+`, got `{line}`"
                    ));
                }
            }
        }
    }
    hunks.extend(current);
    if hunks
        .iter()
        .any(|hunk| hunk.old.is_empty() && hunk.new.is_empty())
    {
        return Err("a hunk has no lines".into());
    }
    Ok(hunks)
}

/// Models used to running apply_patch from a shell wrap the patch in a
/// heredoc: `cat <<'EOF'`, the patch, then `EOF`.
fn strip_heredoc(text: &str) -> &str {
    let Some((first, rest)) = text.split_once('\n') else {
        return text;
    };
    let first = first.trim();
    let first = first.strip_prefix("cat").map_or(first, str::trim_start);
    let Some(tag) = first.strip_prefix("<<") else {
        return text;
    };
    let tag = tag.trim().trim_matches(['\'', '"']);
    if tag.is_empty() || !tag.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return text;
    }
    match rest.rsplit_once('\n') {
        Some((body, last)) if last.trim() == tag => body,
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(context: Option<&str>, old: &[&str], new: &[&str]) -> Hunk {
        Hunk {
            context: context.map(str::to_string),
            old: old.iter().map(|line| line.to_string()).collect(),
            new: new.iter().map(|line| line.to_string()).collect(),
            end_of_file: false,
        }
    }

    #[test]
    fn parses_every_kind_of_section() {
        let patch = "\
*** Begin Patch
*** Add File: hello.txt
+Hello
+
+world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print(\"Hi\")
+print(\"Hello, world!\")
*** Delete File: obsolete.txt
*** End Patch";
        assert_eq!(
            parse(patch),
            Ok(vec![
                Section::Add {
                    path: "hello.txt".into(),
                    content: "Hello\n\nworld\n".into(),
                },
                Section::Update {
                    path: "src/app.py".into(),
                    move_to: Some("src/main.py".into()),
                    hunks: vec![hunk(
                        Some("def greet():"),
                        &["print(\"Hi\")"],
                        &["print(\"Hello, world!\")"],
                    )],
                },
                Section::Delete {
                    path: "obsolete.txt".into(),
                },
            ])
        );
    }

    #[test]
    fn parses_several_hunks_and_end_of_file() {
        let patch = "\
*** Begin Patch
*** Update File: a.rs
 fn a() {}
-fn b() {}

@@ impl C {
     x
+    y
*** End of File
*** End Patch
";
        let Ok(sections) = parse(patch) else {
            panic!("parse failed")
        };
        let [Section::Update { hunks, .. }] = sections.as_slice() else {
            panic!("one update: {sections:?}")
        };
        assert_eq!(
            hunks,
            &[
                hunk(None, &["fn a() {}", "fn b() {}", ""], &["fn a() {}", ""]),
                Hunk {
                    end_of_file: true,
                    ..hunk(Some("impl C {"), &["    x"], &["    x", "    y"])
                },
            ]
        );
    }

    #[test]
    fn accepts_crlf_a_heredoc_and_spacing_between_sections() {
        let patch = "cat <<'EOF'\r\n*** Begin Patch\r\n\r\n*** Add File: a\r\n+x\r\n\r\n\r\n*** Delete File: b\r\n*** End Patch\r\nEOF\n";
        assert_eq!(
            parse(patch),
            Ok(vec![
                Section::Add {
                    path: "a".into(),
                    content: "x\n".into(),
                },
                Section::Delete { path: "b".into() },
            ])
        );
    }

    #[test]
    fn a_rename_needs_no_hunks() {
        assert_eq!(
            parse("*** Begin Patch\n*** Update File: a\n*** Move to: b\n*** End Patch"),
            Ok(vec![Section::Update {
                path: "a".into(),
                move_to: Some("b".into()),
                hunks: vec![],
            }])
        );
    }

    #[test]
    fn malformed_patches_are_errors() {
        let cases = [
            ("*** Add File: a\n+x\n*** End Patch", "must start with"),
            ("*** Begin Patch\n*** Add File: a\n+x", "must end with"),
            ("*** Begin Patch\n*** End Patch", "changes no files"),
            (
                "*** Begin Patch\nhello\n*** End Patch",
                "expected `*** Add File:`",
            ),
            (
                "*** Begin Patch\n*** Add File:  \n*** End Patch",
                "names no file",
            ),
            (
                "*** Begin Patch\n*** Add File: a\nx\n*** End Patch",
                "expected `+`",
            ),
            (
                "*** Begin Patch\n*** Delete File: a\n-x\n*** End Patch",
                "must not be followed",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n*** End Patch",
                "has no hunks",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n@@\n*** End Patch",
                "has no lines",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n@@\n x\n!y\n*** End Patch",
                "got `!y`",
            ),
            (
                "*** Begin Patch\n*** Update File: a\n*** End of File\n*** End Patch",
                "must follow a hunk",
            ),
            (
                "*** Begin Patch\n*** Rename File: a\n*** End Patch",
                "expected",
            ),
        ];
        for (patch, want) in cases {
            match parse(patch) {
                Err(e) => assert!(e.contains(want), "{patch:?}: {e}"),
                Ok(sections) => panic!("{patch:?} parsed: {sections:?}"),
            }
        }
    }

    #[test]
    fn strips_only_a_whole_heredoc() {
        assert_eq!(strip_heredoc("<<EOF\nbody\nEOF"), "body");
        assert_eq!(strip_heredoc("cat <<\"X_1\"\na\nb\nX_1"), "a\nb");
        assert_eq!(strip_heredoc("<<EOF\nbody\nEND"), "<<EOF\nbody\nEND");
        assert_eq!(strip_heredoc("*** Begin Patch"), "*** Begin Patch");
    }
}
