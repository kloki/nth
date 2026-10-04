pub(crate) mod matcher;

use std::{io::ErrorKind, path::Path};

use futures::{FutureExt, future::BoxFuture};
use matcher::MatchError;
use nth_lsp::report;
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::{PostWrite, bom::BOM, post_write, write::write_with_dirs};

/// The calls of a turn run in parallel, and two edits of one file would
/// each read it before the other writes, losing the first. Edits are quick,
/// so one lock for all of them, and for apply_patch, is enough.
pub(crate) static EDITS: Mutex<()> = Mutex::const_new(());

pub struct Edit {
    post_write: PostWrite,
}

impl Edit {
    pub fn new(post_write: PostWrite) -> Self {
        Self { post_write }
    }
}

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
            ctx.writable.check(&path)?;
            let edits = EDITS.lock().await;
            let done = edit(&path, &args).await?;
            // The formatter rewrites the file, so it runs under the lock;
            // waiting on the language servers need not.
            let notes = self.post_write.format(&path, &ctx.cwd).await;
            drop(edits);
            let errors = report::after_edit(&path, &self.post_write.diagnostics(&path).await);
            Ok(post_write::append(done, &[&notes, &errors]))
        }
        .boxed()
    }
}

/// Makes the edit `args` asks for in `path` and returns what it did.
async fn edit(path: &Path, args: &Args) -> Result<String, String> {
    let source = match tokio::fs::read_to_string(path).await {
        Ok(source) => source,
        Err(e) if e.kind() == ErrorKind::NotFound && args.old_string.is_empty() => {
            write_with_dirs(path, args.new_string.as_bytes())
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
    tokio::fs::write(path, edited)
        .await
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(match spans.len() {
        1 => format!("Edited file: {}", path.display()),
        n => format!("Edited file: {} ({n} replacements)", path.display()),
    })
}

pub(crate) fn with_line_endings(text: &str, crlf: bool) -> String {
    let lf = text.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    async fn edit(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Edit::new(PostWrite::off()).call(args, &ctx).await
    }

    fn file(dir: &Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("a.rs");
        std::fs::write(&path, content).expect("write");
        path
    }

    #[tokio::test]
    async fn plan_mode_refuses_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a = 1");
        let ctx = ToolContext {
            writable: nth_protocol::Writable::Only(dir.path().join("plan.md")),
            ..ToolContext::new(dir.path().to_path_buf())
        };

        let out = Edit::new(PostWrite::off())
            .call(
                json!({ "filePath": "a.rs", "oldString": "1", "newString": "2" }),
                &ctx,
            )
            .await;

        assert!(out.expect_err("refused").contains("plan mode is active"));
        assert_eq!(contents(&path), "a = 1");
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
        let tool = Edit::new(PostWrite::off());
        let edits = ["a", "b", "c", "d"].map(|line| {
            tool.call(
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

    #[tokio::test]
    async fn output_notes_the_formatter_that_ran() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x = a\n").expect("write");
        let tool = Edit::new(PostWrite::with_formatter(
            "sed",
            &["sed", "-i", "s/ = /=/", "$FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "filePath": "a.txt", "oldString": "a", "newString": "b" }),
                &ctx,
            )
            .await
            .expect("edit");

        assert_eq!(
            out,
            format!("Edited file: {}\n\nFormatted with sed.", path.display())
        );
        assert_eq!(contents(&path), "x=b\n");
    }

    #[tokio::test]
    async fn a_created_file_is_formatted_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = Edit::new(PostWrite::with_formatter(
            "sed",
            &["sed", "-i", "s/a/b/", "$FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "filePath": "new.txt", "oldString": "", "newString": "a\n" }),
                &ctx,
            )
            .await
            .expect("edit");

        let path = dir.path().join("new.txt");
        assert_eq!(
            out,
            format!("Created file: {}\n\nFormatted with sed.", path.display())
        );
        assert_eq!(contents(&path), "b\n");
    }

    #[tokio::test]
    async fn bom_survives_the_formatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, format!("{BOM}old")).expect("write");
        let tool = Edit::new(PostWrite::with_formatter(
            "rewrite",
            &["sh", "-c", "printf formatted > $FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        tool.call(
            json!({ "filePath": "a.txt", "oldString": "old", "newString": "new" }),
            &ctx,
        )
        .await
        .expect("edit");

        assert_eq!(contents(&path), format!("{BOM}formatted"));
    }

    /// The whole path with a real rust-analyzer: run it by hand with
    /// `cargo test -p nth-tools -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs rust-analyzer on PATH"]
    async fn reports_rust_analyzer_errors_in_the_edited_file_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::post_write::broken_crate(dir.path());
        let tool = Edit::new(PostWrite::lsp_only());
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "filePath": "src/main.rs", "oldString": "= 1;", "newString": "= \"one\";" }),
                &ctx,
            )
            .await
            .expect("edit");

        println!("{out}");
        let main = dir.path().join("src/main.rs");
        assert!(
            out.starts_with(&format!(
                "Edited file: {}\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"{}\">\nERROR [4:",
                main.display(),
                main.display()
            )),
            "{out}"
        );
        assert!(!out.contains("other.rs"), "{out}");
    }
}
