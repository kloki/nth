use std::{io::ErrorKind, path::Path};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;

pub(crate) const BOM: &str = "\u{feff}";

pub struct Write;

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
            let existing = match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.is_dir() => {
                    return Err(format!("cannot write {}: is a directory", path.display()));
                }
                Ok(_) => Some(has_bom(&path).await?),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => return Err(format!("cannot write {}: {e}", path.display())),
            };

            // Editors that emit a BOM (Visual Studio, Notepad) expect to keep
            // it, but models rarely reproduce it, so an existing BOM survives;
            // one the model does send is kept rather than doubled.
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
            Ok(format!("{verb} file: {}", path.display()))
        }
        .boxed()
    }
}

async fn has_bom(path: &Path) -> Result<bool, String> {
    let read = async {
        let mut file = tokio::fs::File::open(path).await?;
        let mut head = [0u8; BOM.len()];
        let mut filled = 0;
        while filled < head.len() {
            match file.read(&mut head[filled..]).await? {
                0 => break,
                n => filled += n,
            }
        }
        Ok::<_, std::io::Error>(head[..filled] == *BOM.as_bytes())
    };
    read.await
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
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
        Write.call(args, &ctx).await
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read back")
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
}
