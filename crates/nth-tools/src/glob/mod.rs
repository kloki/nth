use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use futures::{FutureExt, future::BoxFuture};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

const LIMIT: usize = 100;

pub struct Glob;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
}

impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The glob pattern to match files against" },
                    "path": {
                        "type": "string",
                        "description": "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided."
                    }
                },
                "required": ["pattern"]
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
            let dir = match &args.path {
                Some(path) => ctx.cwd.join(path),
                None => ctx.cwd.clone(),
            };
            match tokio::fs::metadata(&dir).await {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(format!("glob path must be a directory: {}", dir.display())),
                Err(e) => return Err(format!("cannot search {}: {e}", dir.display())),
            }

            let pattern = args.pattern;
            let files = tokio::task::spawn_blocking(move || find(&dir, &pattern))
                .await
                .map_err(|e| format!("glob failed: {e}"))??;

            if files.is_empty() {
                return Ok("No files found".into());
            }
            let truncated = files.len() > LIMIT;
            let mut output: Vec<String> = files
                .iter()
                .take(LIMIT)
                .map(|path| path.display().to_string())
                .collect();
            if truncated {
                output.push(String::new());
                output.push(format!(
                    "(Results are truncated: showing first {LIMIT} results. Consider using a more specific path or pattern.)"
                ));
            }
            Ok(output.join("\n"))
        }
        .boxed()
    }
}

/// Every file under `dir` that `pattern` matches, newest first. Like
/// ripgrep's `--glob`, a pattern without a `/` matches a file name at any
/// depth. Hidden files and what `.gitignore` excludes are skipped.
fn find(dir: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let overrides = OverrideBuilder::new(dir)
        .add(pattern)
        .and_then(|builder| builder.build())
        .map_err(|e| format!("invalid glob pattern: {e}"))?;
    let mut files: Vec<(SystemTime, PathBuf)> = WalkBuilder::new(dir)
        .overrides(overrides)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| {
            // A file we cannot stat still matched; it sorts last.
            let modified = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, entry.into_path())
        })
        .collect();
    files.sort_by(|(a_time, a_path), (b_time, b_path)| {
        b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
    });
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::*;

    async fn glob(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Glob.call(args, &ctx).await
    }

    fn touch(path: &Path, age: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, "").expect("write");
        let modified = SystemTime::now() - Duration::from_secs(age);
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(modified))
            .expect("set mtime");
    }

    fn lines(root: &Path, out: &str) -> Vec<String> {
        out.lines()
            .map(|line| {
                Path::new(line)
                    .strip_prefix(root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| line.to_string())
            })
            .collect()
    }

    #[tokio::test]
    async fn newest_first_skipping_ignored_and_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join(".git")).expect("mkdir");
        fs::write(root.join(".gitignore"), "target/\n").expect("write");
        touch(&root.join("old.rs"), 300);
        touch(&root.join("src/new.rs"), 10);
        touch(&root.join("src/mid.rs"), 100);
        touch(&root.join("src/notes.txt"), 0);
        touch(&root.join("target/gen.rs"), 0);
        touch(&root.join(".hidden/secret.rs"), 0);

        let out = glob(root, json!({ "pattern": "*.rs" }))
            .await
            .expect("glob");
        assert_eq!(lines(root, &out), ["src/new.rs", "src/mid.rs", "old.rs"]);
        let out = glob(root, json!({ "pattern": "src/**/*.rs" }))
            .await
            .expect("glob");
        assert_eq!(lines(root, &out), ["src/new.rs", "src/mid.rs"]);
    }

    #[tokio::test]
    async fn path_is_relative_to_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("a/x.rs"), 0);
        touch(&root.join("b/y.rs"), 0);

        let out = glob(root, json!({ "pattern": "*.rs", "path": "b" }))
            .await
            .expect("glob");
        assert_eq!(out, root.join("b/y.rs").display().to_string());
    }

    #[tokio::test]
    async fn caps_results_with_a_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..=LIMIT {
            touch(&root.join(format!("{i}.txt")), i as u64);
        }

        let out = glob(root, json!({ "pattern": "*.txt" }))
            .await
            .expect("glob");
        let lines = lines(root, &out);
        assert_eq!(lines.len(), LIMIT + 2);
        assert_eq!(lines[0], "0.txt");
        assert_eq!(lines[LIMIT - 1], format!("{}.txt", LIMIT - 1));
        assert!(lines[LIMIT + 1].starts_with("(Results are truncated"));
    }

    #[tokio::test]
    async fn no_matches_and_bad_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("a.txt"), 0);

        let out = glob(root, json!({ "pattern": "*.rs" })).await;
        assert_eq!(out.as_deref(), Ok("No files found"));
        assert!(
            glob(root, json!({ "pattern": "*", "path": "a.txt" }))
                .await
                .is_err()
        );
        assert!(
            glob(root, json!({ "pattern": "*", "path": "missing" }))
                .await
                .is_err()
        );
        assert!(glob(root, json!({ "pattern": "a[" })).await.is_err());
        assert!(glob(root, json!({})).await.is_err());
    }
}
