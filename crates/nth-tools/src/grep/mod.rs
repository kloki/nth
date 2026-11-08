use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use futures::{FutureExt, future::BoxFuture};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::json;

use crate::blocking::{self, Stop};

/// Matches shown before the output is cut.
const MAX_MATCHES: usize = 100;
const MAX_LINE_CHARS: usize = 2000;
/// Bytes looked at for a NUL to tell a binary file, as git does.
const BINARY_SNIFF: usize = 8192;
/// A file larger than this is skipped rather than read: it is a log or a
/// dump, not source, and reading it would hold the whole of it in memory.
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / (1024 * 1024);

/// How far one search goes before it stops, so a search from `/` or a
/// pattern that matches every line ends in bounded time and memory.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Files searched.
    files: usize,
    /// Matches counted, of which only `MAX_MATCHES` are shown.
    matches: usize,
}

const LIMITS: Limits = Limits {
    files: 50_000,
    matches: 10_000,
};

pub struct Grep;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    #[serde(default)]
    literal: bool,
    case_sensitive: Option<bool>,
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
                    "include": { "type": "string", "description": "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")" },
                    "literal": { "type": "boolean", "description": "Treat the pattern as literal text (default false)." },
                    "caseSensitive": { "type": "boolean", "description": "Match case (default true)." }
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
            if args.pattern.is_empty() {
                return Err("pattern is required".into());
            }
            let pattern = if args.literal {
                regex::escape(&args.pattern)
            } else {
                args.pattern
            };
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(!args.case_sensitive.unwrap_or(true))
                .build()
                .map_err(|e| format!("invalid pattern: {e}"))?;
            // Collecting the components drops `.`s, which would otherwise
            // show up in every path printed.
            let root: PathBuf = crate::path::resolve(&ctx.cwd, args.path.as_deref().unwrap_or("."))
                .components()
                .collect();
            if !root.exists() {
                return Err(format!("no such file or directory: {}", root.display()));
            }
            let include = args.include;
            blocking::run(move |stop| search(&root, &regex, include.as_deref(), LIMITS, stop))
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

/// What a walk found, and why it ended early when it did.
struct Searched {
    files: Vec<FileMatches>,
    total: usize,
    /// Files passed over for being larger than `MAX_FILE_BYTES`.
    oversized: usize,
    /// The file the search was pointed at, when it was a file: skipped for
    /// its size, it gets a note on how to search it after all.
    named: Option<PathBuf>,
    stopped: Option<Limit>,
}

/// The limit a walk reached, leaving its results partial.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Limit {
    Files,
    Matches,
}

fn search(
    root: &Path,
    regex: &Regex,
    include: Option<&str>,
    limits: Limits,
    stop: &Stop,
) -> ToolResult {
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

    let mut found = Searched {
        files: Vec::new(),
        total: 0,
        oversized: 0,
        named: root.is_file().then(|| root.to_path_buf()),
        stopped: None,
    };
    let mut searched = 0;
    // One buffer for every file, so a walk allocates for its largest file
    // once rather than for each.
    let mut bytes = Vec::new();
    for entry in walker.build().filter_map(Result::ok) {
        if stop.is_set() {
            return Err("interrupted".into());
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if searched == limits.files {
            found.stopped = Some(Limit::Files);
            break;
        }
        searched += 1;
        // Unreadable files are skipped, as ripgrep does, rather than failing
        // the whole search.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            found.oversized += 1;
            continue;
        }
        bytes.clear();
        // The stat above can be stale: a log being written grows between it
        // and the read. Reading one byte past the cap tells such a file is
        // over without reading the rest of it, so memory stays bounded.
        let read = File::open(entry.path())
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes));
        if read.is_err() {
            continue;
        }
        if bytes.len() as u64 > MAX_FILE_BYTES {
            found.oversized += 1;
            continue;
        }
        if bytes.iter().take(BINARY_SNIFF).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                if found.total == limits.matches {
                    found.stopped = Some(Limit::Matches);
                    break;
                }
                found.total += 1;
                if lines.len() < MAX_MATCHES {
                    lines.push((i + 1, cut(line)));
                }
            }
        }
        if !lines.is_empty() {
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            found.files.push(FileMatches {
                path: entry.into_path(),
                modified,
                lines,
            });
        }
        if found.stopped.is_some() {
            break;
        }
    }

    found.files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(render(&found, limits))
}

fn render(found: &Searched, limits: Limits) -> String {
    let mut out = matches(&found.files, found.total);
    let mut notes = Vec::new();
    match (found.oversized, &found.named) {
        (0, _) => {}
        (_, Some(file)) => notes.push(format!(
            "(Skipped {}: it is larger than {MAX_FILE_MIB} MiB. Search it with `rg` through bash.)",
            file.display()
        )),
        (1, None) => notes.push(format!("(Skipped 1 file larger than {MAX_FILE_MIB} MiB.)")),
        (n, None) => notes.push(format!(
            "(Skipped {n} files larger than {MAX_FILE_MIB} MiB.)"
        )),
    }
    match found.stopped {
        None => {}
        Some(Limit::Files) => notes.push(format!(
            "(Stopped after searching {} files, so these results are partial. Use a more specific path or include.)",
            limits.files
        )),
        Some(Limit::Matches) => notes.push(format!(
            "(Stopped after {} matches, so the count is partial. Use a more specific pattern.)",
            limits.matches
        )),
    }
    if !notes.is_empty() {
        out.push(String::new());
        out.append(&mut notes);
    }
    out.join("\n")
}

fn matches(files: &[FileMatches], total: usize) -> Vec<String> {
    if files.is_empty() {
        return vec!["No files found".into()];
    }
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
    out
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
    async fn literal_and_case_insensitive_search() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.txt", "A.B\naxb\n");
        let out = grep(
            dir.path(),
            json!({"pattern":"a.b", "literal":true, "caseSensitive":false}),
        )
        .await
        .unwrap();
        assert!(out.contains("Line 1: A.B"));
        assert!(!out.contains("axb"));
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
    async fn grep_skips_files_over_the_size_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut big = "needle\n".repeat(1024).into_bytes();
        big.resize(11 * 1024 * 1024, b'x');
        write(dir.path(), "big.log", big);
        let small = write(dir.path(), "small.txt", "needle\n");
        let out = grep(dir.path(), json!({ "pattern": "needle" }))
            .await
            .expect("grep");
        assert_eq!(
            out,
            format!(
                "Found 1 matches\n{}:\n  Line 1: needle\n\n(Skipped 1 file larger than 10 MiB.)",
                small.display()
            )
        );
    }

    #[tokio::test]
    async fn a_named_file_over_the_cap_points_to_rg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut big = b"needle\n".to_vec();
        big.resize(11 * 1024 * 1024, b'x');
        let path = write(dir.path(), "big.log", big);
        let out = grep(
            dir.path(),
            json!({ "pattern": "needle", "path": "big.log" }),
        )
        .await
        .expect("grep");
        assert_eq!(
            out,
            format!(
                "No files found\n\n(Skipped {}: it is larger than 10 MiB. Search it with `rg` through bash.)",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn grep_stops_when_the_call_is_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..20 {
            write(dir.path(), &format!("d{i}/e/f/g.txt"), "needle\n");
        }
        let root = dir.path().to_path_buf();
        let regex = Regex::new("needle").expect("regex");
        let (ended, end) = std::sync::mpsc::channel();
        // The walk starts only once the call is gone, so it sees the stop
        // on its first entry whatever the machine's speed.
        let call = Box::pin(blocking::run(move |stop| {
            blocking::wait_for(stop);
            let _ = ended.send(search(&root, &regex, None, LIMITS, stop));
        }));
        let call = tokio::time::timeout(std::time::Duration::from_millis(10), call).await;
        assert!(call.is_err(), "the call ended before it was cancelled");
        let result = tokio::task::spawn_blocking(move || {
            end.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .expect("join")
        .expect("the walk never ended");
        assert_eq!(result, Err("interrupted".to_string()));
    }

    #[test]
    fn stops_at_the_file_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            write(dir.path(), &format!("{i}.txt"), "needle\n");
        }
        let regex = Regex::new("needle").expect("regex");
        let limits = Limits { files: 3, ..LIMITS };
        let out = search(dir.path(), &regex, None, limits, &Stop::default()).expect("grep");
        assert!(out.starts_with("Found 3 matches"), "{out}");
        assert!(
            out.ends_with(
                "\n\n(Stopped after searching 3 files, so these results are partial. Use a more specific path or include.)"
            ),
            "{out}"
        );
        // Exactly at the cap nothing was left out.
        let limits = Limits { files: 5, ..LIMITS };
        let out = search(dir.path(), &regex, None, limits, &Stop::default()).expect("grep");
        assert!(!out.contains("Stopped"), "{out}");
    }

    #[test]
    fn stops_at_the_match_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.txt", "hit\n".repeat(10));
        let regex = Regex::new("hit").expect("regex");
        let limits = Limits {
            matches: 4,
            ..LIMITS
        };
        let out = search(dir.path(), &regex, None, limits, &Stop::default()).expect("grep");
        assert!(out.starts_with("Found 4 matches\n"), "{out}");
        assert_eq!(out.matches("  Line ").count(), 4);
        assert!(
            out.ends_with(
                "\n\n(Stopped after 4 matches, so the count is partial. Use a more specific pattern.)"
            ),
            "{out}"
        );
        let limits = Limits {
            matches: 10,
            ..LIMITS
        };
        let out = search(dir.path(), &regex, None, limits, &Stop::default()).expect("grep");
        assert!(!out.contains("Stopped"), "{out}");
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
