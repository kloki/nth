use std::path::Path;

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
/// Wraps an instruction file attached to what was read.
const INSTRUCTION: &str = include_str!("instruction.md");

pub struct Read;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read",
            description: include_str!("description.txt"),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "The path to the file or directory to read" },
                    "offset": { "type": "integer", "minimum": 1, "description": "The line number to start reading from (1-indexed)" },
                    "limit": { "type": "integer", "minimum": 1, "description": "The maximum number of lines to read (defaults to 2000)" }
                },
                "required": ["filePath"]
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
            let offset = args.offset.unwrap_or(1).max(1);
            let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            if meta.is_dir() {
                let content = list_dir(&path, offset, limit).await?;
                ctx.output.send(content.clone()).await;
                return Ok(content);
            }
            let mut content = read_file(&path, offset, limit).await?;
            ctx.output.send(content.clone()).await;
            // Only the model sees these; they are not part of the file.
            let nested =
                nth_context::instructions::nested(path, ctx.cwd.clone(), ctx.instructions.clone())
                    .await;
            for instruction in nested {
                content.push('\n');
                content.push_str(
                    &INSTRUCTION
                        .replace("{path}", &instruction.path.display().to_string())
                        .replace("{content}", instruction.content.trim_end()),
                );
            }
            Ok(content)
        }
        .boxed()
    }
}

async fn list_dir(path: &Path, offset: usize, limit: usize) -> ToolResult {
    let mut dir = tokio::fs::read_dir(path).await.map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    while let Some(entry) = dir.next_entry().await.map_err(|e| e.to_string())? {
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            name.push('/');
        }
        entries.push(name);
    }
    entries.sort();
    let total = entries.len();
    let shown: Vec<_> = entries.into_iter().skip(offset - 1).take(limit).collect();
    let mut out = shown.join("\n");
    let end = offset - 1 + shown.len();
    if end < total {
        out.push_str(&format!(
            "\n\n(Showing entries {offset}-{end} of {total}. Use offset={} to continue.)",
            end + 1
        ));
    }
    Ok(out)
}

async fn read_file(path: &Path, offset: usize, limit: usize) -> ToolResult {
    let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(format!("cannot read binary file: {}", path.display()));
    }
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    if offset > total.max(1) {
        return Err(format!(
            "offset {offset} is past the end of the file ({total} lines)"
        ));
    }

    let mut out = String::new();
    let mut last = offset - 1;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let line = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((cut, _)) => format!(
                "{}... (line truncated to {MAX_LINE_CHARS} chars)",
                &line[..cut]
            ),
            None => line.to_string(),
        };
        let entry = format!("{}: {line}\n", i + 1);
        if out.len() + entry.len() > MAX_BYTES {
            break;
        }
        out.push_str(&entry);
        last = i + 1;
    }
    if last < total {
        out.push_str(&format!(
            "\n(Showing lines {offset}-{last} of {total}. Use offset={} to continue.)",
            last + 1
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Read.call(args, &ctx).await
    }

    #[tokio::test]
    async fn numbers_lines_and_resolves_relative_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "foo\nbar\n").expect("write");
        let out = read(dir.path(), json!({ "filePath": "a.txt" })).await;
        assert_eq!(out, Ok("1: foo\n2: bar\n".to_string()));
    }

    #[tokio::test]
    async fn offset_and_limit_add_a_continue_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n4\n").expect("write");
        let out = read(
            dir.path(),
            json!({ "filePath": "a.txt", "offset": 2, "limit": 2 }),
        )
        .await
        .expect("read");
        assert_eq!(
            out,
            "2: 2\n3: 3\n\n(Showing lines 2-3 of 4. Use offset=4 to continue.)"
        );
    }

    #[tokio::test]
    async fn lists_directories_with_trailing_slash() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("b.txt"), "").expect("write");
        let out = read(dir.path(), json!({ "filePath": "." })).await;
        assert_eq!(out, Ok("b.txt\nsub/".to_string()));
    }

    #[tokio::test]
    async fn attaches_a_nested_agents_md_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("git dir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/AGENTS.md"), "Be brief.\n").expect("write");
        std::fs::write(dir.path().join("sub/a.txt"), "foo\n").expect("write");
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let first = Read.call(json!({ "filePath": "sub/a.txt" }), &ctx).await;
        let again = Read.call(json!({ "filePath": "sub/a.txt" }), &ctx).await;

        let agents = dir.path().join("sub/AGENTS.md");
        assert_eq!(
            first,
            Ok(format!(
                "1: foo\n\n<system-reminder>\nInstructions from: {}\nBe brief.\n</system-reminder>\n",
                agents.display()
            ))
        );
        assert_eq!(again, Ok("1: foo\n".to_string()));
    }

    #[tokio::test]
    async fn missing_file_and_bad_args_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            read(dir.path(), json!({ "filePath": "nope" }))
                .await
                .is_err()
        );
        assert!(read(dir.path(), json!({ "path": "a" })).await.is_err());
    }
}
