use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use futures::{FutureExt, future::BoxFuture};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;

/// Matches shown before the output is cut.
const MAX_MATCHES: usize = 100;
const MAX_LINE_CHARS: usize = 2000;
/// Bytes looked at for a NUL to tell a binary file, as git does.
const BINARY_SNIFF: usize = 8192;

pub struct Grep;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
}

impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The regex pattern to search for in file contents" },
                    "path": { "type": "string", "description": "The directory to search in. Defaults to the current working directory." },
                    "include": { "type": "string", "description": "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")" }
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
            if args.pattern.is_empty() {
                return Err("pattern is required".into());
            }
            let regex = Regex::new(&args.pattern).map_err(|e| format!("invalid pattern: {e}"))?;
            // Collecting the components drops `.`s, which would otherwise
            // show up in every path printed.
            let root: PathBuf = ctx
                .cwd
                .join(args.path.as_deref().unwrap_or("."))
                .components()
                .collect();
            if !root.exists() {
                return Err(format!("no such file or directory: {}", root.display()));
            }
            tokio::task::spawn_blocking(move || search(&root, &regex, args.include.as_deref()))
                .await
                .map_err(|e| format!("grep failed: {e}"))?
        }
        .boxed()
    }
}

struct FileMatches {
    path: PathBuf,
    modified: SystemTime,
    /// Line number and text, at most `MAX_MATCHES` of them.
    lines: Vec<(usize, String)>,
}

fn search(root: &Path, regex: &Regex, include: Option<&str>) -> ToolResult {
    let mut walker = WalkBuilder::new(root);
    // Dotfiles hold config worth finding; `.git` holds nothing worth reading.
    walker
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    if let Some(include) = include {
        let overrides = OverrideBuilder::new(root)
            .add(include)
            .and_then(|o| o.build())
            .map_err(|e| format!("invalid include pattern: {e}"))?;
        walker.overrides(overrides);
    }

    let mut files = Vec::new();
    let mut total = 0;
    for entry in walker.build().filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // Unreadable files are skipped, as ripgrep does, rather than failing
        // the whole search.
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.iter().take(BINARY_SNIFF).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                total += 1;
                if lines.len() < MAX_MATCHES {
                    lines.push((i + 1, cut(line)));
                }
            }
        }
        if lines.is_empty() {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push(FileMatches {
            path: entry.into_path(),
            modified,
            lines,
        });
    }

    if files.is_empty() {
        return Ok("No files found".into());
    }
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(render(&files, total))
}

fn render(files: &[FileMatches], total: usize) -> String {
    let truncated = total > MAX_MATCHES;
    let mut out = vec![if truncated {
        format!("Found {total} matches (showing first {MAX_MATCHES})")
    } else {
        format!("Found {total} matches")
    }];
    let mut left = MAX_MATCHES;
    for file in files {
        if left == 0 {
            break;
        }
        if out.len() > 1 {
            out.push(String::new());
        }
        out.push(format!("{}:", file.path.display()));
        for (n, text) in file.lines.iter().take(left) {
            out.push(format!("  Line {n}: {text}"));
            left -= 1;
        }
    }
    if truncated {
        out.push(String::new());
        out.push("(Results truncated. Consider using a more specific path or pattern.)".into());
    }
    out.join("\n")
}

fn cut(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}...", &line[..at]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};

    use super::*;

    async fn grep(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Grep.call(args, &ctx).await
    }

    fn write(dir: &Path, name: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, content).expect("write");
        path
    }

    fn age(path: &Path, secs: u64) {
        let time = SystemTime::now() - std::time::Duration::from_secs(secs);
        File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_times(FileTimes::new().set_modified(time)))
            .expect("set mtime");
    }

    #[tokio::test]
    async fn groups_by_file_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = write(dir.path(), "old.txt", "fn a\nnothing\nfn b\n");
        let new = write(dir.path(), "sub/new.txt", "x\nfn c\n");
        age(&old, 100);
        let out = grep(dir.path(), json!({ "pattern": r"fn \w" }))
            .await
            .expect("grep");
        assert_eq!(
            out,
            format!(
                "Found 3 matches\n{}:\n  Line 2: fn c\n\n{}:\n  Line 1: fn a\n  Line 3: fn b",
                new.display(),
                old.display()
            )
        );
    }

    #[tokio::test]
    async fn include_filters_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.rs", "needle\n");
        write(dir.path(), "b.ts", "needle\n");
        write(dir.path(), "deep/c.tsx", "needle\n");
        let out = grep(
            dir.path(),
            json!({ "pattern": "needle", "include": "*.{ts,tsx}" }),
        )
        .await
        .expect("grep");
        assert!(out.starts_with("Found 2 matches"), "{out}");
        assert!(!out.contains("a.rs"), "{out}");
    }

    #[tokio::test]
    async fn path_narrows_the_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a/x.txt", "needle\n");
        write(dir.path(), "b/y.txt", "needle\n");
        let out = grep(dir.path(), json!({ "pattern": "needle", "path": "b" }))
            .await
            .expect("grep");
        assert!(out.starts_with("Found 1 matches"), "{out}");
        assert!(out.contains("y.txt"), "{out}");
    }

    #[tokio::test]
    async fn respects_gitignore_and_skips_dot_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
        write(dir.path(), ".gitignore", "target/\n");
        write(dir.path(), "target/out.txt", "needle\n");
        write(dir.path(), ".git/config", "needle\n");
        write(dir.path(), ".env", "needle\n");
        let out = grep(dir.path(), json!({ "pattern": "needle" }))
            .await
            .expect("grep");
        assert_eq!(
            out,
            format!(
                "Found 1 matches\n{}:\n  Line 1: needle",
                dir.path().join(".env").display()
            )
        );
    }

    #[tokio::test]
    async fn caps_matches_with_a_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.txt", "hit\n".repeat(80));
        write(dir.path(), "b.txt", "hit\n".repeat(80));
        let out = grep(dir.path(), json!({ "pattern": "hit" }))
            .await
            .expect("grep");
        assert!(
            out.starts_with("Found 160 matches (showing first 100)"),
            "{out}"
        );
        assert_eq!(out.matches("  Line ").count(), MAX_MATCHES);
        assert!(
            out.ends_with("(Results truncated. Consider using a more specific path or pattern.)")
        );
    }

    #[tokio::test]
    async fn cuts_long_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.txt", format!("needle{}\n", "é".repeat(3000)));
        let out = grep(dir.path(), json!({ "pattern": "needle" }))
            .await
            .expect("grep");
        let line = out.lines().last().expect("a line");
        assert!(line.ends_with("..."), "{line}");
        assert_eq!(
            line.trim_start_matches("  Line 1: ").chars().count(),
            MAX_LINE_CHARS + 3
        );
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.bin", b"needle\0\x01\x02");
        let out = grep(dir.path(), json!({ "pattern": "needle" })).await;
        assert_eq!(out, Ok("No files found".to_string()));
    }

    #[tokio::test]
    async fn bad_arguments_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad_regex = grep(dir.path(), json!({ "pattern": "(" })).await;
        assert!(bad_regex.is_err_and(|e| e.starts_with("invalid pattern")));
        assert!(grep(dir.path(), json!({ "pattern": "" })).await.is_err());
        assert!(grep(dir.path(), json!({})).await.is_err());
        assert!(
            grep(dir.path(), json!({ "pattern": "x", "path": "missing" }))
                .await
                .is_err()
        );
    }
}
