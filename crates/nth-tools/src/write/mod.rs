use std::{io::ErrorKind, path::Path};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::{
    PostWrite,
    bom::{BOM, has_bom},
    edit::EDITS,
    post_write,
};

pub struct Write {
    post_write: PostWrite,
}

impl Write {
    pub fn new(post_write: PostWrite) -> Self {
        Self { post_write }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    file_path: String,
    content: String,
}

impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "The path to the file to write" },
                    "content": { "type": "string", "description": "The content to write to the file" }
                },
                "required": ["filePath", "content"]
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
            let path = ctx.cwd.join(&args.file_path);
            ctx.writable.check(&path)?;
            // Under the same lock as edit and apply_patch: a write that lands
            // while an edit's formatter rewrites the file would be undone.
            let edits = EDITS.lock().await;
            let existing = match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.is_dir() => {
                    return Err(format!("cannot write {}: is a directory", path.display()));
                }
                Ok(_) => Some(has_bom(&path).await?),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => return Err(format!("cannot write {}: {e}", path.display())),
            };

            // Models rarely reproduce a BOM, so an existing one survives; one
            // the model does send is kept rather than doubled.
            let content = args.content.strip_prefix(BOM).unwrap_or(&args.content);
            let bom = existing == Some(true) || content.len() != args.content.len();
            let content = if bom {
                format!("{BOM}{content}")
            } else {
                content.to_string()
            };

            write_with_dirs(&path, content.as_bytes())
                .await
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            let verb = if existing.is_some() {
                "Wrote"
            } else {
                "Created"
            };
            let wrote = format!("{verb} file: {}", path.display());
            // The formatter rewrites the file, so it runs under the lock;
            // waiting on the language servers need not.
            let notes = self.post_write.format(&path, &ctx.cwd).await;
            drop(edits);
            let errors = self.post_write.check(&path).await;
            Ok(post_write::append(wrote, &[&notes, &errors]))
        }
        .boxed()
    }
}

pub(crate) async fn write_with_dirs(path: &Path, content: &[u8]) -> std::io::Result<()> {
    match tokio::fs::write(path, content).await {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, content).await
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Write::new(PostWrite::off()).call(args, &ctx).await
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read back")
    }

    #[tokio::test]
    async fn plan_mode_writes_only_the_plan_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = dir.path().join(".nth/plans/1.md");
        let ctx = ToolContext {
            writable: nth_protocol::Writable::Only(plan.clone()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let tool = Write::new(PostWrite::off());

        let refused = tool
            .call(json!({ "filePath": "a.txt", "content": "x" }), &ctx)
            .await
            .expect_err("outside the plan");
        assert!(refused.contains("plan mode is active"), "{refused}");
        assert!(!dir.path().join("a.txt").exists());

        tool.call(
            json!({ "filePath": ".nth/plans/1.md", "content": "# Plan" }),
            &ctx,
        )
        .await
        .expect("the plan file");
        assert_eq!(contents(&plan), "# Plan");
    }

    #[tokio::test]
    async fn creates_file_and_missing_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = write(
            dir.path(),
            json!({ "filePath": "a/b/c.txt", "content": "hi\n" }),
        )
        .await
        .expect("write");
        let path = dir.path().join("a/b/c.txt");
        assert_eq!(out, format!("Created file: {}", path.display()));
        assert_eq!(contents(&path), "hi\n");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "old content that is longer").expect("write");
        let out = write(dir.path(), json!({ "filePath": "a.txt", "content": "new" }))
            .await
            .expect("write");
        assert_eq!(out, format!("Wrote file: {}", path.display()));
        assert_eq!(contents(&path), "new");
    }

    #[tokio::test]
    async fn keeps_bom_of_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.cs");
        std::fs::write(&path, format!("{BOM}using Old;\n")).expect("write");
        write(
            dir.path(),
            json!({ "filePath": "a.cs", "content": "using Up;\n" }),
        )
        .await
        .expect("write");
        assert_eq!(contents(&path), format!("{BOM}using Up;\n"));
    }

    #[tokio::test]
    async fn keeps_bom_from_content_without_doubling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.cs");
        std::fs::write(&path, format!("{BOM}old")).expect("write");
        write(
            dir.path(),
            json!({ "filePath": "a.cs", "content": format!("{BOM}new") }),
        )
        .await
        .expect("write");
        assert_eq!(contents(&path), format!("{BOM}new"));
    }

    #[tokio::test]
    async fn keeps_bom_from_content_on_new_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            json!({ "filePath": "a.cs", "content": format!("{BOM}new") }),
        )
        .await
        .expect("write");
        assert_eq!(contents(&dir.path().join("a.cs")), format!("{BOM}new"));
    }

    #[tokio::test]
    async fn writes_empty_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), json!({ "filePath": "empty", "content": "" }))
            .await
            .expect("write");
        assert_eq!(contents(&dir.path().join("empty")), "");
    }

    #[tokio::test]
    async fn directory_and_bad_args_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            write(dir.path(), json!({ "filePath": ".", "content": "x" }))
                .await
                .is_err()
        );
        assert!(write(dir.path(), json!({ "filePath": "a" })).await.is_err());
    }

    #[tokio::test]
    async fn a_write_waits_for_a_parallel_edit_and_its_formatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").expect("write");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        // The edit's formatter holds the file for a while and then writes
        // back what it read, as a real one does; a write slipping in
        // between would be lost.
        let edit = crate::Edit::new(PostWrite::with_formatter(
            "slow",
            &[
                "sh",
                "-c",
                "c=$(cat $FILE); sleep 0.3; printf '%s!\\n' \"$c\" > $FILE",
            ],
        ));
        let write = Write::new(PostWrite::off());

        let (edited, wrote) = tokio::join!(
            edit.call(
                json!({ "filePath": "a.txt", "oldString": "x", "newString": "y" }),
                &ctx,
            ),
            write.call(json!({ "filePath": "a.txt", "content": "z\n" }), &ctx),
        );

        edited.expect("edit");
        wrote.expect("write");
        assert_eq!(contents(&path), "z\n");
    }

    #[tokio::test]
    async fn output_notes_the_formatter_that_ran() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = Write::new(PostWrite::with_formatter(
            "sed",
            &["perl", "-pi", "-e", "s/a/b/", "$FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(json!({ "filePath": "a.txt", "content": "a\n" }), &ctx)
            .await
            .expect("write");

        let path = dir.path().join("a.txt");
        assert_eq!(
            out,
            format!("Created file: {}\n\nFormatted with sed.", path.display())
        );
        assert_eq!(contents(&path), "b\n");
    }

    #[tokio::test]
    async fn a_language_server_not_on_path_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut lsp = nth_lsp::LspConfig::default();
        lsp.servers.insert(
            "ghost".into(),
            nth_lsp::ServerConfig {
                command: vec!["no-such-server-nth".into()],
                extensions: vec![".txt".into()],
                ..Default::default()
            },
        );
        let format = nth_format::FormatConfig {
            enabled: false,
            ..Default::default()
        };
        let tool = Write::new(PostWrite::new(
            std::sync::Arc::new(nth_format::Formatters::new(&format)),
            nth_lsp::Lsp::new(&lsp),
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(json!({ "filePath": "a.txt", "content": "a\n" }), &ctx)
            .await
            .expect("write");

        let path = dir.path().join("a.txt");
        assert_eq!(out, format!("Created file: {}", path.display()));
    }

    #[tokio::test]
    async fn bom_survives_the_formatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, format!("{BOM}old")).expect("write");
        let tool = Write::new(PostWrite::with_formatter(
            "rewrite",
            &["sh", "-c", "printf formatted > $FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        tool.call(json!({ "filePath": "a.txt", "content": "new" }), &ctx)
            .await
            .expect("write");

        assert_eq!(contents(&path), format!("{BOM}formatted"));
    }
}
