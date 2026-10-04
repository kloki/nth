mod matcher;

use std::io::ErrorKind;

use futures::{FutureExt, future::BoxFuture};
use matcher::MatchError;
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::write::{BOM, write_with_dirs};

/// The calls of a turn run in parallel, and two edits of one file would
/// each read it before the other writes, losing the first. Edits are quick,
/// so one lock for all of them is enough.
static EDITS: Mutex<()> = Mutex::const_new(());

pub struct Edit;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "The path to the file to modify" },
                    "oldString": { "type": "string", "description": "The text to replace" },
                    "newString": { "type": "string", "description": "The text to replace it with (must be different from oldString)" },
                    "replaceAll": { "type": "boolean", "description": "Replace all occurrences of oldString (default false)" }
                },
                "required": ["filePath", "oldString", "newString"]
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
            if args.old_string == args.new_string {
                return Err("no changes to apply: oldString and newString are identical".into());
            }
            let path = ctx.cwd.join(&args.file_path);
            let _edit = EDITS.lock().await;
            let source = match tokio::fs::read_to_string(&path).await {
                Ok(source) => source,
                Err(e) if e.kind() == ErrorKind::NotFound && args.old_string.is_empty() => {
                    write_with_dirs(&path, args.new_string.as_bytes())
                        .await
                        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                    return Ok(format!("Created file: {}", path.display()));
                }
                Err(e) => return Err(format!("cannot edit {}: {e}", path.display())),
            };
            if args.old_string.is_empty() {
                return Err(format!(
                    "oldString is empty but {} already exists. Give the text to replace, or use write to replace the whole file.",
                    path.display()
                ));
            }

            // Matching ignores the BOM, which the model never sees, and
            // speaks the file's line endings, which it rarely reproduces.
            let text = source.strip_prefix(BOM).unwrap_or(&source);
            let crlf = text.contains("\r\n");
            let old = with_line_endings(&args.old_string, crlf);
            let new = with_line_endings(&args.new_string, crlf);
            let spans = if args.replace_all {
                matcher::find_all(text, &old)
            } else {
                matcher::find_unique(text, &old).map(|span| vec![span])
            }
            .map_err(|e| match e {
                MatchError::NotFound => format!(
                    "oldString not found in {}. It must match the file, including whitespace and indentation; read it again if unsure.",
                    path.display()
                ),
                MatchError::Ambiguous(n) => format!(
                    "oldString matches {n} places in {}. Add surrounding lines to make it unique, or set replaceAll to change every one.",
                    path.display()
                ),
            })?;

            let mut edited = String::with_capacity(source.len());
            edited.push_str(&source[..source.len() - text.len()]);
            let mut from = 0;
            for span in &spans {
                edited.push_str(&text[from..span.start]);
                edited.push_str(&new);
                from = span.end;
            }
            edited.push_str(&text[from..]);
            tokio::fs::write(&path, edited)
                .await
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(match spans.len() {
                1 => format!("Edited file: {}", path.display()),
                n => format!("Edited file: {} ({n} replacements)", path.display()),
            })
        }
        .boxed()
    }
}

fn with_line_endings(text: &str, crlf: bool) -> String {
    let lf = text.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn edit(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Edit.call(args, &ctx).await
    }

    fn file(dir: &Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("a.rs");
        std::fs::write(&path, content).expect("write");
        path
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read back")
    }

    #[tokio::test]
    async fn replaces_a_unique_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "let a = 1;\nlet b = 2;\n");
        let out = edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "b = 2", "newString": "b = 3" }),
        )
        .await
        .expect("edit");
        assert_eq!(out, format!("Edited file: {}", path.display()));
        assert_eq!(contents(&path), "let a = 1;\nlet b = 3;\n");
    }

    #[tokio::test]
    async fn replace_all_changes_every_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "foo(); foo();\nfoo();\n");
        let out = edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "foo", "newString": "bar", "replaceAll": true }),
        )
        .await
        .expect("edit");
        assert_eq!(
            out,
            format!("Edited file: {} (3 replacements)", path.display())
        );
        assert_eq!(contents(&path), "bar(); bar();\nbar();\n");
    }

    #[tokio::test]
    async fn several_matches_are_an_error_that_counts_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "x\nx\n");
        let err = edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "x", "newString": "y" }),
        )
        .await
        .expect_err("ambiguous");
        assert!(err.contains("matches 2 places"), "{err}");
        assert!(err.contains("replaceAll"), "{err}");
        assert_eq!(contents(&path), "x\nx\n");
    }

    #[tokio::test]
    async fn no_match_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        file(dir.path(), "x\n");
        let err = edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "y", "newString": "z" }),
        )
        .await
        .expect_err("not found");
        assert!(err.contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn identical_strings_are_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        file(dir.path(), "x\n");
        let err = edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "x", "newString": "x" }),
        )
        .await
        .expect_err("identical");
        assert!(err.contains("identical"), "{err}");
    }

    #[tokio::test]
    async fn falls_back_to_a_fuzzy_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "fn f() {\n\tlet x = 1;\n}\n");
        edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "    let x = 1;\n", "newString": "\tlet x = 2;\n" }),
        )
        .await
        .expect("edit");
        assert_eq!(contents(&path), "fn f() {\n\tlet x = 2;\n}\n");
    }

    #[tokio::test]
    async fn empty_old_string_creates_a_missing_file_and_its_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = edit(
            dir.path(),
            json!({ "filePath": "a/b/c.txt", "oldString": "", "newString": "hi\n" }),
        )
        .await
        .expect("create");
        let path = dir.path().join("a/b/c.txt");
        assert_eq!(out, format!("Created file: {}", path.display()));
        assert_eq!(contents(&path), "hi\n");
    }

    #[tokio::test]
    async fn empty_old_string_on_an_existing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "keep\n");
        let err = edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "", "newString": "x" }),
        )
        .await
        .expect_err("exists");
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(contents(&path), "keep\n");
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            edit(
                dir.path(),
                json!({ "filePath": "nope.rs", "oldString": "a", "newString": "b" }),
            )
            .await
            .is_err()
        );
        assert!(!dir.path().join("nope.rs").exists());
    }

    #[tokio::test]
    async fn keeps_crlf_line_endings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a\r\nb\r\nc\r\n");
        edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "a\nb\n", "newString": "a\nx\ny\n" }),
        )
        .await
        .expect("edit");
        assert_eq!(contents(&path), "a\r\nx\r\ny\r\nc\r\n");
    }

    #[tokio::test]
    async fn keeps_a_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), &format!("{BOM}using Old;\n"));
        edit(
            dir.path(),
            json!({ "filePath": "a.rs", "oldString": "using Old;", "newString": "using New;" }),
        )
        .await
        .expect("edit");
        assert_eq!(contents(&path), format!("{BOM}using New;\n"));
    }

    #[tokio::test]
    async fn parallel_edits_of_one_file_all_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a\nb\nc\nd\n");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let edits = ["a", "b", "c", "d"].map(|line| {
            Edit.call(
                json!({ "filePath": "a.rs", "oldString": format!("{line}\n"), "newString": format!("{line}{line}\n") }),
                &ctx,
            )
        });
        for result in futures::future::join_all(edits).await {
            result.expect("edit");
        }
        assert_eq!(contents(&path), "aa\nbb\ncc\ndd\n");
    }

    #[tokio::test]
    async fn directory_and_bad_args_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            edit(
                dir.path(),
                json!({ "filePath": ".", "oldString": "a", "newString": "b" }),
            )
            .await
            .is_err()
        );
        assert!(
            edit(dir.path(), json!({ "filePath": "a.rs", "oldString": "a" }))
                .await
                .is_err()
        );
    }
}
