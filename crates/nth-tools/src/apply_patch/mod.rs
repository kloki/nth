mod parse;

use std::{
    collections::BTreeMap,
    io::ErrorKind,
    ops::Range,
    path::{Path, PathBuf},
};

use futures::{
    FutureExt,
    future::{BoxFuture, join_all},
};
use nth_lsp::report;
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use parse::{Hunk, Section};
use serde::Deserialize;
use serde_json::json;

use crate::{
    PostWrite,
    bom::BOM,
    edit::{
        EDITS,
        matcher::{self, MatchError},
        with_line_endings,
    },
    post_write,
    write::write_with_dirs,
};

pub struct ApplyPatch {
    post_write: PostWrite,
}

impl ApplyPatch {
    pub fn new(post_write: PostWrite) -> Self {
        Self { post_write }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    patch_text: String,
}

impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patchText": { "type": "string", "description": "The full patch text that describes all changes to be made" }
                },
                "required": ["patchText"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::parse_args(args)?;
            let sections =
                parse::parse(&args.patch_text).map_err(|e| format!("invalid patch: {e}"))?;
            let edits = EDITS.lock().await;
            let mut plan = Plan {
                cwd: &ctx.cwd,
                files: BTreeMap::new(),
            };
            let mut summary = Vec::with_capacity(sections.len());
            for section in &sections {
                let line = plan
                    .add(section)
                    .await
                    .map_err(|e| format!("{e}\nNo files were changed."))?;
                summary.push(line);
            }
            // Checked before anything is written, so a refused patch
            // changes no file at all.
            for path in plan.files.keys() {
                ctx.writable
                    .check(path)
                    .map_err(|e| format!("{e}\nNo files were changed."))?;
            }
            let changed = plan.changed();
            plan.write().await?;
            // The formatters rewrite the files, so they run under the lock;
            // waiting on the language servers need not.
            let mut notes = Vec::new();
            for path in &changed {
                let name = relative(path, &ctx.cwd);
                let ran = self.post_write.format(path, &ctx.cwd).await;
                notes.extend(ran.lines().map(|note| format!("{name}: {note}")));
            }
            drop(edits);
            let diagnostics =
                join_all(changed.iter().map(|p| self.post_write.diagnostics(p))).await;
            let errors: Vec<String> = changed
                .iter()
                .zip(&diagnostics)
                .map(|(path, d)| report::after_patch(&relative(path, &ctx.cwd), path, d))
                .collect();
            let done = format!(
                "Success. Updated the following files:\n{}",
                summary.join("\n")
            );
            let notes = notes.join("\n");
            let mut sections = vec![notes.as_str()];
            sections.extend(errors.iter().map(String::as_str));
            Ok(post_write::append(done, &sections))
        }
        .boxed()
    }
}

/// `path` as the model named it, relative to the working directory.
fn relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// What a patch does to each file, worked out in memory before any file is
/// written, so a hunk that fails to apply leaves every file as it was.
struct Plan<'a> {
    cwd: &'a Path,
    /// The new content of each file, or `None` to delete it. Sections read
    /// what earlier ones planned, so a patch may touch a file twice.
    files: BTreeMap<PathBuf, Option<String>>,
}

impl Plan<'_> {
    /// Plans one section and returns its line of the summary.
    async fn add(&mut self, section: &Section) -> Result<String, String> {
        match section {
            Section::Add { path, content } => {
                let full = self.cwd.join(path);
                if self.read(&full).await?.is_some() {
                    return Err(format!(
                        "cannot add {}: it already exists. Use `*** Update File:` to change it.",
                        full.display()
                    ));
                }
                self.files.insert(full, Some(content.clone()));
                Ok(format!("A {path}"))
            }
            Section::Delete { path } => {
                let full = self.cwd.join(path);
                if self.read(&full).await?.is_none() {
                    return Err(format!("cannot delete {}: no such file", full.display()));
                }
                self.files.insert(full, None);
                Ok(format!("D {path}"))
            }
            Section::Update {
                path,
                move_to,
                hunks,
            } => {
                let full = self.cwd.join(path);
                let source = self
                    .read(&full)
                    .await?
                    .ok_or_else(|| format!("cannot update {}: no such file", full.display()))?;
                let updated = apply(&source, hunks)
                    .map_err(|e| format!("cannot update {}: {e}", full.display()))?;
                let Some(move_to) = move_to else {
                    self.files.insert(full, Some(updated));
                    return Ok(format!("M {path}"));
                };
                let dest = self.cwd.join(move_to);
                if dest != full {
                    if self.read(&dest).await?.is_some() {
                        return Err(format!(
                            "cannot move {} to {}: it already exists",
                            full.display(),
                            dest.display()
                        ));
                    }
                    self.files.insert(full, None);
                }
                self.files.insert(dest, Some(updated));
                Ok(format!("M {move_to}"))
            }
        }
    }

    /// A file as the sections so far leave it; `None` if it does not exist.
    async fn read(&self, path: &Path) -> Result<Option<String>, String> {
        if let Some(planned) = self.files.get(path) {
            return Ok(planned.clone());
        }
        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// The files the patch leaves in place, added, changed or moved to.
    fn changed(&self) -> Vec<PathBuf> {
        let kept = self.files.iter().filter(|(_, content)| content.is_some());
        kept.map(|(path, _)| path.clone()).collect()
    }

    /// Deletes first, so a file may give way to a directory of its name.
    async fn write(self) -> Result<(), String> {
        for (path, _) in self.files.iter().filter(|(_, content)| content.is_none()) {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| format!("cannot delete {}: {e}", path.display()))?;
        }
        for (path, content) in &self.files {
            if let Some(content) = content {
                write_with_dirs(path, content.as_bytes())
                    .await
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            }
        }
        Ok(())
    }
}

/// `source` with `hunks` applied in order, each found after the one before.
/// Like edit it keeps a BOM and speaks the file's line endings, and like
/// opencode it ends the file with a newline.
fn apply(source: &str, hunks: &[Hunk]) -> Result<String, String> {
    let text = source.strip_prefix(BOM).unwrap_or(source);
    let crlf = text.contains("\r\n");
    let eol = if crlf { "\r\n" } else { "\n" };
    let text = if text.is_empty() || text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}{eol}")
    };

    let mut cursor = 0;
    let mut replacements = Vec::with_capacity(hunks.len());
    for (i, hunk) in hunks.iter().enumerate() {
        let n = i + 1;
        if let Some(context) = &hunk.context {
            let found = matcher::find_where(&text, context, |span| span.start >= cursor)
                .map_err(|_| format!("hunk {n}: the @@ line `{context}` is not in the file"))?;
            // Sorted, so the first is the nearest after the previous hunk.
            cursor = found
                .first()
                .map_or(cursor, |span| line_end(&text, span.end));
        }
        let new = joined(&hunk.new, eol);
        if hunk.old.is_empty() {
            // Pure additions go after the @@ line, or else at the end.
            let at = if hunk.context.is_some() {
                cursor
            } else {
                text.len()
            };
            replacements.push((at..at, new));
            continue;
        }
        let (span, new) = locate(&text, hunk, cursor, crlf).map_err(|e| {
            let expected = hunk.old.join("\n");
            match e {
                MatchError::NotFound => format!(
                    "hunk {n} not found. Its context and `-` lines must match the file, including indentation; read it again if unsure. Expected{}:\n{expected}",
                    if hunk.end_of_file { " at the end of the file" } else { "" }
                ),
                MatchError::Ambiguous(k) => format!(
                    "hunk {n} matches {k} places. Add an @@ line naming the enclosing function or class, or more context lines:\n{expected}"
                ),
            }
        })?;
        cursor = span.end;
        replacements.push((span, joined(new, eol)));
    }

    // Pure additions at the end may come before later hunks.
    replacements.sort_by_key(|(span, _)| span.start);
    let mut updated = String::with_capacity(source.len());
    if source.starts_with(BOM) {
        updated.push_str(BOM);
    }
    let mut from = 0;
    for (span, new) in &replacements {
        updated.push_str(&text[from..span.start]);
        updated.push_str(new);
        from = span.end;
    }
    updated.push_str(&text[from..]);
    Ok(updated)
}

/// The whole lines at or after `cursor` that hold the hunk's old lines,
/// and the new lines to put there. The old lines must match one place,
/// unless an @@ line anchors the hunk, which then takes the nearest.
fn locate<'h>(
    text: &str,
    hunk: &'h Hunk,
    cursor: usize,
    crlf: bool,
) -> Result<(Range<usize>, &'h [String]), MatchError> {
    // Only whole lines, give or take indentation, so `x = 1` never matches
    // the end of `max = 1`.
    let keep = |span: &Range<usize>| {
        span.start >= cursor
            && text[line_start(text, span.start)..span.start]
                .trim()
                .is_empty()
            && text[span.end..line_end(text, span.end)].trim().is_empty()
            && (!hunk.end_of_file || text[span.end..].trim().is_empty())
    };
    let (mut old, mut new) = (hunk.old.as_slice(), hunk.new.as_slice());
    let found = loop {
        let needle = with_line_endings(&joined(old, "\n"), crlf);
        match matcher::find_where(text, &needle, keep) {
            // A trailing empty line is often spacing before the next hunk
            // rather than a line the file has, so try without it.
            Err(MatchError::NotFound)
                if old.len() > 1 && old.last().is_some_and(String::is_empty) =>
            {
                old = &old[..old.len() - 1];
                if new.last().is_some_and(String::is_empty) {
                    new = &new[..new.len() - 1];
                }
            }
            result => break result?,
        }
    };
    let span = match found.as_slice() {
        [span] => span,
        [span, ..] if hunk.context.is_some() => span,
        many => return Err(MatchError::Ambiguous(many.len())),
    };
    Ok((line_start(text, span.start)..line_end(text, span.end), new))
}

fn joined(lines: &[String], eol: &str) -> String {
    lines.iter().map(|line| format!("{line}{eol}")).collect()
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// Past the newline ending the line `at` is on, or `at` itself if it is
/// already at the start of a line.
fn line_end(text: &str, at: usize) -> usize {
    if at == 0 || text[..at].ends_with('\n') {
        return at;
    }
    text[at..].find('\n').map_or(text.len(), |i| at + i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn patch(dir: &Path, patch: &str) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        ApplyPatch::new(PostWrite::off())
            .call(json!({ "patchText": patch }), &ctx)
            .await
    }

    fn file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        path
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read back")
    }

    #[tokio::test]
    async fn plan_mode_refuses_the_whole_patch_when_one_file_is_not_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = dir.path().join("plan.md");
        let ctx = ToolContext {
            writable: nth_protocol::Writable::Only(plan.clone()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let text = "*** Begin Patch\n*** Add File: plan.md\n+# Plan\n*** Add File: b.txt\n+b\n*** End Patch";

        let out = ApplyPatch::new(PostWrite::off())
            .call(json!({ "patchText": text }), &ctx)
            .await;

        let err = out.expect_err("refused");
        assert!(err.contains("plan mode is active"), "{err}");
        assert!(err.ends_with("No files were changed."), "{err}");
        assert!(!plan.exists());
        assert!(!dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn adds_a_file_and_its_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: a/b.txt\n+one\n+\n+two\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nA a/b.txt");
        assert_eq!(contents(&dir.path().join("a/b.txt")), "one\n\ntwo\n");
    }

    #[tokio::test]
    async fn updates_with_several_hunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(
            dir.path(),
            "a.rs",
            "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n\nfn c() {\n    3\n}\n",
        );
        let out = patch(
            dir.path(),
            "\
*** Begin Patch
*** Update File: a.rs
@@ fn a() {
-    1
+    10
 }
@@ fn c() {
-    3
+    30
+    31
 }
*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nM a.rs");
        assert_eq!(
            contents(&path),
            "fn a() {\n    10\n}\n\nfn b() {\n    2\n}\n\nfn c() {\n    30\n    31\n}\n"
        );
    }

    #[tokio::test]
    async fn the_at_line_picks_between_repeated_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(
            dir.path(),
            "a.py",
            "def f():\n    pass\n\ndef g():\n    pass\n",
        );
        let ambiguous =
            "*** Begin Patch\n*** Update File: a.py\n@@\n-    pass\n+    return 1\n*** End Patch";
        let err = patch(dir.path(), ambiguous).await.expect_err("ambiguous");
        assert!(err.contains("hunk 1 matches 2 places"), "{err}");

        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.py\n@@ def g():\n-    pass\n+    return 1\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(
            contents(&path),
            "def f():\n    pass\n\ndef g():\n    return 1\n"
        );
    }

    #[tokio::test]
    async fn later_hunks_search_after_earlier_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "x\ny\nx\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-x\n+1\n y\n@@\n-x\n+2\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "1\ny\n2\n");
    }

    #[tokio::test]
    async fn matches_whole_lines_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "max = 1\n  x = 1\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-x = 1\n+  x = 2\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "max = 1\n  x = 2\n");
    }

    #[tokio::test]
    async fn pure_additions_go_after_the_at_line_or_at_the_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "head\nmid\ntail");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n+end\n@@ head\n+after head\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "head\nafter head\nmid\ntail\nend\n");
    }

    #[tokio::test]
    async fn end_of_file_anchors_to_the_last_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "}\nfn x() {\n}\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-}\n+} // x\n*** End of File\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "}\nfn x() {\n} // x\n");
    }

    #[tokio::test]
    async fn deletes_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "old.txt", "bye\n");
        let out = patch(
            dir.path(),
            "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nD old.txt");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn moves_and_updates_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = file(dir.path(), "app.py", "def greet():\n    print(\"Hi\")\n");
        let out = patch(
            dir.path(),
            "\
*** Begin Patch
*** Update File: app.py
*** Move to: src/main.py
@@ def greet():
-    print(\"Hi\")
+    print(\"Hello, world!\")
*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nM src/main.py");
        assert!(!from.exists());
        assert_eq!(
            contents(&dir.path().join("src/main.py")),
            "def greet():\n    print(\"Hello, world!\")\n"
        );
    }

    #[tokio::test]
    async fn moving_onto_an_existing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = file(dir.path(), "a", "a\n");
        let to = file(dir.path(), "b", "b\n");
        let err = patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n*** Move to: b\n*** End Patch",
        )
        .await
        .expect_err("exists");
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            (contents(&from), contents(&to)),
            ("a\n".into(), "b\n".into())
        );
    }

    #[tokio::test]
    async fn falls_back_to_fuzzy_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(
            dir.path(),
            "a.rs",
            "fn f() {\n\tlet x = 1;  \n\tlet y = 2;\n}\n",
        );
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.rs\n@@ fn f() {\n-    let x = 1;\n+\tlet x = 10;\n     let y = 2;\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(
            contents(&path),
            "fn f() {\n\tlet x = 10;\n    let y = 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn drops_a_trailing_blank_context_line_the_file_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "a\nb\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-b\n+c\n\n*** Add File: z\n+z\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "a\nc\n");
    }

    #[tokio::test]
    async fn a_failing_hunk_changes_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = file(dir.path(), "a", "one\n");
        let b = file(dir.path(), "b", "two\n");
        let gone = file(dir.path(), "gone", "still here\n");
        let err = patch(
            dir.path(),
            "\
*** Begin Patch
*** Add File: new
+new
*** Update File: a
@@
-one
+1
*** Delete File: gone
*** Update File: b
@@
 two
-three
+3
*** End Patch",
        )
        .await
        .expect_err("hunk fails");
        assert!(
            err.contains(&format!("cannot update {}", b.display())),
            "{err}"
        );
        assert!(err.contains("hunk 1 not found"), "{err}");
        assert!(err.contains("No files were changed"), "{err}");
        assert_eq!(contents(&a), "one\n");
        assert_eq!(contents(&b), "two\n");
        assert_eq!(contents(&gone), "still here\n");
        assert!(!dir.path().join("new").exists());
    }

    #[tokio::test]
    async fn missing_and_existing_targets_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "keep\n");
        let cases = [
            ("*** Add File: a\n+x", "already exists"),
            ("*** Update File: nope\n@@\n-x\n+y", "no such file"),
            ("*** Delete File: nope", "no such file"),
        ];
        for (section, want) in cases {
            let err = patch(
                dir.path(),
                &format!("*** Begin Patch\n{section}\n*** End Patch"),
            )
            .await
            .expect_err(section);
            assert!(err.contains(want), "{section}: {err}");
        }
        assert_eq!(contents(&path), "keep\n");
    }

    #[tokio::test]
    async fn a_malformed_patch_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = patch(dir.path(), "*** Add File: a\n+x")
            .await
            .expect_err("malformed");
        assert!(err.starts_with("invalid patch:"), "{err}");
        assert!(
            patch(dir.path(), "*** Begin Patch\n*** End Patch")
                .await
                .is_err()
        );
        let ctx = ToolContext::new(dir.path().to_path_buf());
        assert!(
            ApplyPatch::new(PostWrite::off())
                .call(json!({}), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sections_see_what_earlier_ones_planned() {
        let dir = tempfile::tempdir().expect("tempdir");
        file(dir.path(), "old", "x\n");
        patch(
            dir.path(),
            "\
*** Begin Patch
*** Add File: a
+one
*** Update File: a
@@
-one
+two
*** Delete File: old
*** Add File: old
+fresh
*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&dir.path().join("a")), "two\n");
        assert_eq!(contents(&dir.path().join("old")), "fresh\n");
    }

    #[tokio::test]
    async fn keeps_crlf_and_a_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.cs", &format!("{BOM}a\r\nb\r\nc\r\n"));
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.cs\n@@\n a\n-b\n+x\n+y\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), format!("{BOM}a\r\nx\r\ny\r\nc\r\n"));
    }

    #[tokio::test]
    async fn formats_each_file_it_leaves_and_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        file(dir.path(), "a.txt", "a\n");
        file(dir.path(), "gone.txt", "x\n");
        file(dir.path(), "from.txt", "m\n");
        let tool = ApplyPatch::new(PostWrite::with_formatter(
            "upper",
            &[
                "sh",
                "-c",
                "tr a-z A-Z < $FILE > $FILE.tmp && mv $FILE.tmp $FILE",
            ],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "patchText": "*** Begin Patch
*** Update File: a.txt
@@
-a
+b
*** Delete File: gone.txt
*** Add File: sub/new.txt
+n
*** Update File: from.txt
*** Move to: to.txt
@@
-m
+t
*** End Patch" }),
                &ctx,
            )
            .await
            .expect("patch");

        assert_eq!(
            out,
            "Success. Updated the following files:\nM a.txt\nD gone.txt\nA sub/new.txt\nM to.txt\n\n\
             a.txt: Formatted with upper.\n\
             sub/new.txt: Formatted with upper.\n\
             to.txt: Formatted with upper."
        );
        assert_eq!(contents(&dir.path().join("a.txt")), "B\n");
        assert_eq!(contents(&dir.path().join("sub/new.txt")), "N\n");
        assert_eq!(contents(&dir.path().join("to.txt")), "T\n");
        assert!(!dir.path().join("gone.txt").exists());
        assert!(!dir.path().join("from.txt").exists());
    }

    #[tokio::test]
    async fn bom_survives_the_formatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", &format!("{BOM}old\n"));
        let tool = ApplyPatch::new(PostWrite::with_formatter(
            "rewrite",
            &["sh", "-c", "printf formatted > $FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        tool.call(
            json!({ "patchText": "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch" }),
            &ctx,
        )
        .await
        .expect("patch");

        assert_eq!(contents(&path), format!("{BOM}formatted"));
    }

    /// The whole path with a real rust-analyzer: run it by hand with
    /// `cargo test -p nth-tools -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs rust-analyzer on PATH"]
    async fn reports_rust_analyzer_errors_per_changed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::post_write::broken_crate(dir.path());
        let tool = ApplyPatch::new(PostWrite::lsp_only());
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "patchText": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-    let x: u32 = 1;\n+    let x: u32 = \"one\";\n*** End Patch" }),
                &ctx,
            )
            .await
            .expect("patch");

        println!("{out}");
        let main = dir.path().join("src/main.rs");
        assert!(
            out.starts_with(&format!(
                "Success. Updated the following files:\nM src/main.rs\n\n\
                 LSP errors detected in src/main.rs, please fix:\n<diagnostics file=\"{}\">\nERROR [4:",
                main.display()
            )),
            "{out}"
        );
        assert!(!out.contains("other.rs"), "{out}");
    }
}
