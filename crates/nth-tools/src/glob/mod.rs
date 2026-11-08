use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use futures::{FutureExt, future::BoxFuture};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::blocking::{self, Stop};

const LIMIT: usize = 100;
/// Matches collected before the walk stops, so a pattern such as `**` from
/// `/` ends in bounded time and memory.
const MAX_FILES: usize = 50_000;

pub struct Glob;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
    limit: Option<usize>,
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
                    },
                    "hidden": { "type": "boolean", "description": "Include hidden files (default false), excluding .git." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Maximum results (default 100)." }
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
            let args: Args = nth_protocol::parse_args(args)?;
            let limit = args.limit.unwrap_or(LIMIT);
            if limit == 0 {
                return Err("limit must be positive".into());
            }
            let mut dir = match &args.path {
                Some(path) => crate::path::resolve(&ctx.cwd, path),
                None => ctx.cwd.clone(),
            };
            let mut pattern = args.pattern;
            if Path::new(&pattern).is_absolute() {
                let wildcard = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
                let prefix = &pattern[..wildcard];
                let parent = prefix.rfind('/').unwrap_or(0);
                dir = PathBuf::from(if parent == 0 { "/" } else { &prefix[..parent] });
                // Keep the remainder anchored to this directory. A bare
                // basename otherwise matches at every depth in `find`.
                pattern = format!("/{}", &pattern[parent + 1..]);
            }
            match tokio::fs::metadata(&dir).await {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(format!("glob path must be a directory: {}", dir.display())),
                Err(e) => return Err(format!("cannot search {}: {e}", dir.display())),
            }

            let found =
                blocking::run(move |stop| find(&dir, &pattern, args.hidden, MAX_FILES, stop))
                    .await
                    .map_err(|e| format!("glob failed: {e}"))??;
            Ok(listing(&found, limit))
        }
        .boxed()
    }
}

/// The files a walk matched, newest first.
struct Found {
    files: Vec<PathBuf>,
    /// The walk stopped at its cap with matches left, so the newest files
    /// may be among those it never reached.
    partial: Option<usize>,
}

/// Every file under `dir` that `pattern` matches, newest first, up to `max`
/// of them. Like ripgrep's `--glob`, a pattern without a `/` matches a file
/// name at any depth. Hidden files and what `.gitignore` excludes are
/// skipped.
fn find(dir: &Path, pattern: &str, hidden: bool, max: usize, stop: &Stop) -> Result<Found, String> {
    let overrides = OverrideBuilder::new(dir)
        .add(pattern)
        .and_then(|builder| builder.build())
        .map_err(|e| format!("invalid glob pattern: {e}"))?;
    let walk = WalkBuilder::new(dir)
        .overrides(overrides)
        .hidden(!hidden)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut partial = None;
    for entry in walk.filter_map(Result::ok) {
        if stop.is_set() {
            return Err("interrupted".into());
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if files.len() == max {
            partial = Some(max);
            break;
        }
        // A file we cannot stat still matched; it sorts last.
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((modified, entry.into_path()));
    }
    files.sort_by(|(a_time, a_path), (b_time, b_path)| {
        b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
    });
    Ok(Found {
        files: files.into_iter().map(|(_, path)| path).collect(),
        partial,
    })
}

/// What the model reads: the first `limit` files and why any were left out.
fn listing(found: &Found, limit: usize) -> String {
    let files = &found.files;
    if files.is_empty() {
        return "No files found".into();
    }
    let mut output: Vec<String> = files
        .iter()
        .take(limit)
        .map(|path| path.display().to_string())
        .collect();
    if files.len() > limit {
        output.push(String::new());
        output.push(format!(
            "(Results are truncated: showing first {limit} results. Consider using a more specific path or pattern.)"
        ));
    }
    if let Some(max) = found.partial {
        output.push(String::new());
        output.push(format!(
            "(The listing is partial: the search stopped after {max} matches, so newer files may be missing. Use a more specific path or pattern.)"
        ));
    }
    output.join("\n")
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
    async fn hidden_and_limit_apply_after_sorting_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("old.txt"), 100);
        touch(&root.join(".hidden/new.txt"), 10);
        touch(&root.join(".git/secret.txt"), 0);
        let out = glob(root, json!({"pattern":"*.txt", "hidden":true, "limit":1}))
            .await
            .unwrap();
        assert!(
            out.starts_with(&root.join(".hidden/new.txt").display().to_string()),
            "{out}"
        );
        assert!(!out.contains("secret.txt"));
        assert!(glob(root, json!({"pattern":"*", "limit":0})).await.is_err());
    }

    #[tokio::test]
    async fn absolute_pattern_matches_files_under_the_search_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("src/a.rs"), 0);
        touch(&root.join("src/a.txt"), 0);
        let pattern = root.join("src/*.rs");
        let out = glob(root, json!({ "pattern": pattern.to_str().unwrap() }))
            .await
            .expect("glob");
        assert_eq!(out, root.join("src/a.rs").display().to_string());
    }

    #[tokio::test]
    async fn absolute_patterns_keep_their_depth_and_literal_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        touch(&root.join("café/a.rs"), 0);
        touch(&root.join("café/nested/a.rs"), 0);
        touch(&root.join("cafeteria/a.rs"), 0);
        for suffix in ["café/*.rs", "café/a.rs"] {
            let pattern = root.join(suffix);
            let out = glob(root, json!({ "pattern": pattern.to_str().unwrap() }))
                .await
                .expect("glob");
            assert_eq!(out, root.join("café/a.rs").display().to_string());
        }
        let pattern = root.join("café/**/*.rs");
        let out = glob(root, json!({ "pattern": pattern.to_str().unwrap() }))
            .await
            .expect("glob");
        let mut matches = lines(root, &out);
        matches.sort();
        assert_eq!(matches, ["café/a.rs", "café/nested/a.rs"]);
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

    #[test]
    fn glob_says_when_the_listing_is_partial() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..5 {
            touch(&root.join(format!("{i}.txt")), i as u64);
        }
        let found = find(root, "*.txt", false, 3, &Stop::default()).expect("glob");
        assert_eq!(found.files.len(), 3);
        let out = listing(&found, LIMIT);
        assert!(
            out.ends_with(
                "\n\n(The listing is partial: the search stopped after 3 matches, so newer files may be missing. Use a more specific path or pattern.)"
            ),
            "{out}"
        );
        // Exactly at the cap nothing was left out.
        let found = find(root, "*.txt", false, 5, &Stop::default()).expect("glob");
        assert!(!listing(&found, LIMIT).contains("partial"));
    }

    #[tokio::test]
    async fn glob_stops_when_the_call_is_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        touch(&root.join("a/b/c.txt"), 0);
        let (ended, end) = std::sync::mpsc::channel();
        // The walk starts only once the call is gone, so it sees the stop
        // on its first entry whatever the machine's speed.
        let call = Box::pin(blocking::run(move |stop| {
            blocking::wait_for(stop);
            let _ = ended.send(find(&root, "*.txt", false, MAX_FILES, stop).map(|f| f.files));
        }));
        let call = tokio::time::timeout(Duration::from_millis(10), call).await;
        assert!(call.is_err(), "the call ended before it was cancelled");
        let result = tokio::task::spawn_blocking(move || end.recv_timeout(Duration::from_secs(5)))
            .await
            .expect("join")
            .expect("the walk never ended");
        assert_eq!(result, Err("interrupted".to_string()));
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
