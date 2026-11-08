mod image;

use std::{
    fs::File,
    io::{BufRead, BufReader, Cursor, Read as _},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use ::image::ImageFormat;
use futures::{FutureExt, future::BoxFuture};
use nth_lsp::Lsp;
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadConfig {
    /// Lines returned when the model gives no `limit`.
    pub default_limit: usize,
    pub max_line_chars: usize,
    /// Output stops before this many bytes, whatever the `limit`.
    pub max_bytes: usize,
}

/// The largest image file read: past it the file is refused before it is
/// read whole, as no screenshot or photo comes near.
const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;

/// How far into a text file the read tool reads: a log this deep is for
/// bash's tail or sed, not a numbered window.
const MAX_TEXT_BYTES: u64 = 64 * 1024 * 1024;

/// The first bytes of a file, which tell an image by its magic and a
/// binary file by a NUL.
const HEAD: u64 = 8192;

/// Wraps an instruction file attached to what was read.
const INSTRUCTION: &str = include_str!("instruction.md");

impl Default for ReadConfig {
    fn default() -> Self {
        Self {
            default_limit: 2000,
            max_line_chars: 2000,
            max_bytes: 50 * 1024,
        }
    }
}

pub struct Read {
    config: ReadConfig,
    lsp: Lsp,
}

impl Read {
    /// `lsp` is told about every file read, so its server is warm by the
    /// time the model writes there.
    pub fn new(config: ReadConfig, lsp: Lsp) -> Self {
        Self { config, lsp }
    }
}

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
            description: include_str!("description.txt")
                .replace("{default_limit}", &self.config.default_limit.to_string())
                .replace("{max_line_chars}", &self.config.max_line_chars.to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "The path to the file or directory to read" },
                    "offset": { "type": "integer", "minimum": 1, "description": "The line number to start reading from (1-indexed)" },
                    "limit": { "type": "integer", "minimum": 1, "description": format!("The maximum number of lines to read (defaults to {})", self.config.default_limit) }
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
            let args: Args = nth_protocol::parse_args(args)?;
            let path = crate::path::resolve(&ctx.cwd, &args.file_path);
            let offset = args.offset.unwrap_or(1).max(1);
            // A `limit` of 0 asks for nothing, which the model never means;
            // an empty read would only tell it to continue from where it is.
            let limit = match args.limit {
                None | Some(0) => self.config.default_limit,
                Some(limit) => limit,
            };
            let meta = match tokio::fs::metadata(&path).await {
                Ok(meta) => meta,
                Err(e) => {
                    let hint = if e.kind() == std::io::ErrorKind::NotFound {
                        path_hint(&path).await
                    } else {
                        String::new()
                    };
                    return Err(format!("cannot read {}: {e}{hint}", path.display()));
                }
            };
            // A listing's own instruction file counts, a file's is its
            // folder's.
            let (mut content, dir) = if meta.is_dir() {
                (list_dir(&path, offset, limit).await?, path)
            } else {
                // A FIFO or a device never ends, or never starts: reading
                // one would hang the call or run without bound.
                if !meta.is_file() {
                    return Err(not_regular(&path));
                }
                let (file, config) = (path.clone(), self.config.clone());
                let loaded = tokio::task::spawn_blocking(move || {
                    let (opened, len) = open_regular(&file)?;
                    load(&file, opened, len, offset, limit, &config)
                })
                .await
                .map_err(|e| format!("reading {} failed: {e}", path.display()))??;
                let content = match loaded {
                    Loaded::Text(content) => content,
                    // An image is shown, not read as text, and has no
                    // language server or instructions beside it to speak of.
                    Loaded::Image(bytes, format) => {
                        let text = image::read(&path, bytes, format, ctx).await?;
                        ctx.output.send(text.clone()).await;
                        return Ok(text);
                    }
                };
                // Detached so the read never waits on a server starting,
                // which can take seconds. Not cancelled with the turn: the
                // touch only opens the file, and a server it starts is
                // meant to outlive it.
                let lsp = self.lsp.clone();
                let file = path.clone();
                let caller = crate::post_write::caller(ctx);
                tokio::spawn(async move {
                    match caller {
                        Some(caller) => lsp.touch_attributed(&file, false, caller).await,
                        None => lsp.touch(&file, false).await,
                    }
                });
                let dir = path.parent().map(Path::to_path_buf).unwrap_or(path);
                (content, dir)
            };
            ctx.output.send(content.clone()).await;
            // Only the model sees these; they are not part of the file.
            let nested = nth_context::instructions::nested(
                dir,
                ctx.cwd.clone(),
                ctx.context.instruction_name,
                ctx.instructions.clone(),
            )
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

async fn path_hint(path: &Path) -> String {
    let mut missing = path;
    while let Some(parent) = missing.parent() {
        match tokio::fs::metadata(parent).await {
            Ok(meta) if meta.is_dir() => {
                let Some(name) = missing.file_name().and_then(|n| n.to_str()) else {
                    return String::new();
                };
                let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
                    return String::new();
                };
                let mut candidates = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let candidate = entry.file_name().to_string_lossy().into_owned();
                    let a = name.to_lowercase();
                    let b = candidate.to_lowercase();
                    let common = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
                    if common > 0 || a.contains(&b) || b.contains(&a) {
                        candidates.push((std::cmp::Reverse(common), candidate));
                    }
                }
                candidates.sort();
                let names: Vec<_> = candidates.into_iter().take(5).map(|(_, n)| n).collect();
                return if names.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nSimilar entries in {}: {}",
                        parent.display(),
                        names.join(", ")
                    )
                };
            }
            Ok(_) => return String::new(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing = parent,
            Err(_) => return String::new(),
        }
    }
    String::new()
}

fn not_regular(path: &Path) -> String {
    format!("cannot read {}: not a regular file", path.display())
}

/// Opens `path` if it is a regular file, with its length. The caller
/// checked already, but the path may have become a FIFO since: opened
/// non-blocking, the open returns at once instead of waiting for a
/// writer, and the check on the open file refuses it. Reads of a regular
/// file ignore the flag.
fn open_regular(path: &Path) -> Result<(File, u64), String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(not_regular(path));
    }
    Ok((file, meta.len()))
}

/// What a file read turned out to hold.
enum Loaded {
    Image(Vec<u8>, ImageFormat),
    Text(String),
}

/// Reads `file`, `len` bytes long, as the read tool shows it: an image
/// whole, text only as far as its window needs. Blocking.
fn load(
    path: &Path,
    mut file: impl std::io::Read,
    len: u64,
    offset: usize,
    limit: usize,
    config: &ReadConfig,
) -> Result<Loaded, String> {
    let mut head = Vec::new();
    file.by_ref()
        .take(HEAD)
        .read_to_end(&mut head)
        .map_err(|e| e.to_string())?;
    if let Some(format) = image::sniff(&head) {
        if len > MAX_IMAGE_FILE {
            return Err(format!(
                "cannot show {}: the image file is {} MiB, over the {} MiB the read tool shows",
                path.display(),
                len / (1024 * 1024),
                MAX_IMAGE_FILE / (1024 * 1024)
            ));
        }
        let mut bytes = head;
        // Bounded again in case the file grew since its length was read.
        let rest = MAX_IMAGE_FILE.saturating_sub(bytes.len() as u64);
        file.take(rest)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        return Ok(Loaded::Image(bytes, format));
    }
    if head.contains(&0) {
        return Err(format!("cannot read binary file: {}", path.display()));
    }
    let reader = BufReader::new(Cursor::new(head).chain(file));
    read_text(path, reader, offset, limit, config).map(Loaded::Text)
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

/// Why `Lines::next` returned.
enum Next {
    /// A line, possibly the last one with no ending.
    Line,
    /// The end of the input, with nothing after the last line.
    End,
    /// The input read reached `Lines::stop`; `true` when it cut a line,
    /// whose start is in the buffer.
    Stopped(bool),
}

/// The lines of a text, read only as far as asked and never past `stop`
/// bytes in, keeping as much of each line as its caller will show.
struct Lines<R> {
    reader: R,
    read: u64,
    stop: u64,
}

impl<R: BufRead> Lines<R> {
    /// Reads the next line into `line`, without its ending, keeping at
    /// most `keep` bytes of it: the rest is read past, so one line with no
    /// ending never takes more memory than the caller shows. A line ends
    /// as `str::lines` ends one, at `\n` or `\r\n`.
    fn next(&mut self, line: &mut Vec<u8>, keep: usize) -> std::io::Result<Next> {
        line.clear();
        let mut len = 0;
        loop {
            let available = match self.reader.fill_buf() {
                Ok(available) => available,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            // Asked before `stop`, so a text ending right at it is whole.
            if available.is_empty() {
                return Ok(if len > 0 { Next::Line } else { Next::End });
            }
            if self.read >= self.stop {
                return Ok(Next::Stopped(len > 0));
            }
            let room = usize::try_from(self.stop - self.read).unwrap_or(usize::MAX);
            let available = &available[..available.len().min(room)];
            let newline = available.iter().position(|&b| b == b'\n');
            let content = newline.unwrap_or(available.len());
            let kept = content.min(keep.saturating_sub(line.len()));
            line.extend_from_slice(&available[..kept]);
            len += content;
            let taken = newline.map_or(content, |i| i + 1);
            self.reader.consume(taken);
            self.read += taken as u64;
            if newline.is_some() {
                if len == line.len() && line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Next::Line);
            }
        }
    }
}

/// Numbers the lines from `offset` on, keeping only the window's, and
/// counts the rest to tell how long the file is, never reading past
/// `MAX_TEXT_BYTES`.
fn read_text(
    path: &Path,
    reader: impl BufRead,
    offset: usize,
    limit: usize,
    config: &ReadConfig,
) -> ToolResult {
    let failed = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let too_deep = || {
        format!(
            "cannot read {} from line {offset}: it starts past the first {} MiB, all the read tool reads; use bash (tail, sed) for that part",
            path.display(),
            MAX_TEXT_BYTES / (1024 * 1024)
        )
    };
    // A character is at most four bytes, and an invalid byte one
    // character, so this many bytes of a line longer than the cut are
    // always more than `max_line_chars` characters: it is cut as if read
    // whole.
    let keep = config.max_line_chars.saturating_mul(4).saturating_add(4);
    let mut lines = Lines {
        reader,
        read: 0,
        stop: MAX_TEXT_BYTES,
    };
    let mut line = Vec::new();
    // Whole lines read, and how many there are once the end is reached.
    let mut seen = 0;
    let mut total = None;
    while seen < offset - 1 {
        match lines.next(&mut line, 0).map_err(failed)? {
            Next::Line => seen += 1,
            Next::End => {
                total = Some(seen);
                break;
            }
            Next::Stopped(_) => return Err(too_deep()),
        }
    }

    let mut out = String::new();
    let mut last = offset - 1;
    let mut stopped = false;
    let mut cut_at_cap = false;
    let end = (offset - 1).saturating_add(limit);
    while total.is_none() && seen < end {
        let cut = match lines.next(&mut line, keep).map_err(failed)? {
            Next::Line => false,
            Next::End => {
                total = Some(seen);
                break;
            }
            Next::Stopped(cut) => {
                stopped = true;
                cut_at_cap = cut;
                if !cut {
                    break;
                }
                true
            }
        };
        seen += 1;
        let text = String::from_utf8_lossy(&line);
        let shown = match text.char_indices().nth(config.max_line_chars) {
            Some((cut, _)) => format!(
                "{}... (line truncated to {} chars)",
                &text[..cut],
                config.max_line_chars
            ),
            None => text.into_owned(),
        };
        let entry = format!("{seen}: {shown}\n");
        if out.len() + entry.len() > config.max_bytes {
            if !out.is_empty() {
                break;
            }
            // A first line past the whole budget still goes out, cut, or
            // the read makes no progress and tells the model to continue
            // from the line it is on.
            let cut = entry.floor_char_boundary(config.max_bytes);
            out.push_str(entry[..cut].trim_end_matches('\n'));
            out.push_str(&format!(
                "... (line truncated to {} bytes)\n",
                config.max_bytes
            ));
            last = seen;
            break;
        }
        out.push_str(&entry);
        last = seen;
        if cut {
            break;
        }
    }

    // Past the window lines are only counted, never kept, so the hint can
    // name the file's length, and its last line, at no cost in memory.
    if total.is_none() && !stopped {
        loop {
            match lines.next(&mut line, 0).map_err(failed)? {
                Next::Line => seen += 1,
                Next::End => {
                    total = Some(seen);
                    break;
                }
                Next::Stopped(_) => break,
            }
        }
    }
    if let Some(total) = total {
        if offset > total.max(1) {
            return Err(format!(
                "offset {offset} is past the end of the file ({total} lines)"
            ));
        }
        if last < total {
            out.push_str(&format!(
                "\n(Showing lines {offset}-{last} of {total}. Use offset={} to continue.)",
                last + 1
            ));
        }
        return Ok(out);
    }
    // The cap came before the end.
    if last < offset {
        return Err(too_deep());
    }
    if last < seen {
        // A line the window cut at the cap may be the file's last.
        let whole = seen - usize::from(cut_at_cap);
        out.push_str(&format!(
            "\n(Showing lines {offset}-{last} of more than {whole}. Use offset={} to continue.)",
            last + 1
        ));
    } else {
        // The next line starts at the cap, where no read reaches.
        out.push_str(&format!(
            "\n(Showing lines {offset}-{last}. The read tool stops {} MiB into a file; use bash (tail, sed) for what follows.)",
            MAX_TEXT_BYTES / (1024 * 1024)
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(dir: &Path, args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        Read::off().call(args, &ctx).await
    }

    impl Read {
        fn off() -> Self {
            Self::new(ReadConfig::default(), crate::post_write::lsp_off())
        }
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
    async fn a_zero_limit_means_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "1\n2\n").expect("write");
        let out = read(dir.path(), json!({ "filePath": "a.txt", "limit": 0 })).await;
        assert_eq!(out, Ok("1: 1\n2: 2\n".to_string()));
    }

    #[tokio::test]
    async fn a_line_over_the_byte_budget_still_moves_the_read_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "abcdefghijklmnop\nx\n").expect("write");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let read = Read::new(
            ReadConfig {
                max_bytes: 12,
                ..ReadConfig::default()
            },
            crate::post_write::lsp_off(),
        );
        let out = read.call(json!({ "filePath": "a.txt" }), &ctx).await;
        assert_eq!(
            out,
            Ok("1: abcdefghi... (line truncated to 12 bytes)\n\n(Showing lines 1-1 of 2. Use offset=2 to continue.)".to_string())
        );
    }

    #[tokio::test]
    async fn uses_configured_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n").expect("write");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let read = Read::new(
            ReadConfig {
                default_limit: 1,
                ..ReadConfig::default()
            },
            crate::post_write::lsp_off(),
        );
        assert!(read.spec().description.contains("up to 1 lines"));
        let out = read.call(json!({ "filePath": "a.txt" }), &ctx).await;
        assert_eq!(
            out,
            Ok("1: 1\n\n(Showing lines 1-1 of 3. Use offset=2 to continue.)".to_string())
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

        let first = Read::off()
            .call(json!({ "filePath": "sub/a.txt" }), &ctx)
            .await;
        let again = Read::off()
            .call(json!({ "filePath": "sub/a.txt" }), &ctx)
            .await;

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
    async fn listing_a_directory_attaches_its_agents_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("git dir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("AGENTS.md"), "Root.\n").expect("write");
        std::fs::write(dir.path().join("sub/AGENTS.md"), "Be brief.\n").expect("write");
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let root = Read::off().call(json!({ "filePath": "." }), &ctx).await;
        let sub = Read::off().call(json!({ "filePath": "sub" }), &ctx).await;

        assert_eq!(root, Ok(".git/\nAGENTS.md\nsub/".to_string()));
        let agents = dir.path().join("sub/AGENTS.md");
        assert_eq!(
            sub,
            Ok(format!(
                "AGENTS.md\n<system-reminder>\nInstructions from: {}\nBe brief.\n</system-reminder>\n",
                agents.display()
            ))
        );
    }

    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in the path");
        // SAFETY: `c` is a NUL-terminated path that outlives the call.
        let made = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
    }

    /// A writer coming and going lets a reader still blocked on the FIFO
    /// see its end, so a failing test never leaves a thread stuck.
    fn release(fifo: &Path) {
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(fifo),
        );
    }

    #[tokio::test]
    async fn read_refuses_a_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe");
        mkfifo(&fifo);

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read(dir.path(), json!({ "filePath": "pipe" })),
        )
        .await;
        release(&fifo);

        assert_eq!(
            out.expect("refused without waiting for a writer"),
            Err(format!(
                "cannot read {}: not a regular file",
                fifo.display()
            ))
        );
    }

    #[test]
    fn opening_a_fifo_never_waits_for_a_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe");
        mkfifo(&fifo);

        // The path checked as a regular file and swapped for a FIFO before
        // the open: the open itself must not wait.
        let (tx, rx) = std::sync::mpsc::channel();
        let opener = {
            let fifo = fifo.clone();
            std::thread::spawn(move || {
                let _ = tx.send(open_regular(&fifo).map(|_| ()));
            })
        };
        let opened = rx.recv_timeout(std::time::Duration::from_secs(5));
        release(&fifo);
        opener.join().expect("the opener ends");

        assert_eq!(
            opened.expect("returned without waiting for a writer"),
            Err(not_regular(&fifo))
        );
    }

    #[test]
    fn read_stops_at_its_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.log");
        let text: String = (1..=1_000_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &text).expect("write");
        let (file, len) = open_regular(&path).expect("opens");

        let loaded = load(&path, file, len, 5, 3, &ReadConfig::default());

        let Ok(Loaded::Text(out)) = loaded else {
            panic!("not read as text");
        };
        assert_eq!(
            out,
            "5: line 5\n6: line 6\n7: line 7\n\n(Showing lines 5-7 of 1000000. Use offset=8 to continue.)"
        );
    }

    #[test]
    fn a_line_is_kept_only_up_to_its_cut() {
        // Ten MiB with no ending, as a minified bundle or a log of one line.
        let long = vec![b'a'; 10 * 1024 * 1024];
        let mut lines = Lines {
            reader: BufReader::new(Cursor::new(long)),
            read: 0,
            stop: MAX_TEXT_BYTES,
        };
        let mut line = Vec::new();

        assert!(matches!(lines.next(&mut line, 100), Ok(Next::Line)));
        assert_eq!(line.len(), 100);
        assert!(
            line.capacity() < 64 * 1024,
            "{} bytes held",
            line.capacity()
        );
        assert_eq!(lines.read, 10 * 1024 * 1024);
        assert!(matches!(lines.next(&mut line, 100), Ok(Next::End)));
    }

    #[tokio::test]
    async fn crlf_endings_and_long_lines_read_as_before() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "ab\r\nabcdefgh\r\nc\r").expect("write");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let read = Read::new(
            ReadConfig {
                max_line_chars: 3,
                ..ReadConfig::default()
            },
            crate::post_write::lsp_off(),
        );
        let out = read.call(json!({ "filePath": "a.txt" }), &ctx).await;
        assert_eq!(
            out,
            Ok("1: ab\n2: abc... (line truncated to 3 chars)\n3: c\r\n".to_string())
        );
    }

    #[tokio::test]
    async fn an_offset_past_the_end_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "1\n2\n").expect("write");
        let out = read(dir.path(), json!({ "filePath": "a.txt", "offset": 4 })).await;
        assert_eq!(
            out,
            Err("offset 4 is past the end of the file (2 lines)".to_string())
        );
        let out = read(dir.path(), json!({ "filePath": "a.txt", "offset": 3 })).await;
        assert_eq!(
            out,
            Err("offset 3 is past the end of the file (2 lines)".to_string())
        );
    }

    /// A file whose first line fills the head, so it reads as text, and
    /// whose second runs past the input cap.
    fn past_the_cap(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("a.txt");
        let file = std::fs::File::create(&path).expect("creates");
        let first = "a".repeat(HEAD as usize);
        std::io::Write::write_all(&mut &file, format!("{first}\ntwo").as_bytes()).expect("writes");
        // Sparse: NULs past the cap, a few blocks on disk.
        file.set_len(MAX_TEXT_BYTES + 1024).expect("grows");
        path
    }

    #[tokio::test]
    async fn a_read_stops_at_the_input_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = past_the_cap(dir.path());

        let out = read(dir.path(), json!({ "filePath": "a.txt" }))
            .await
            .expect("read");
        assert!(
            out.contains("\n2: two\0\0"),
            "{}",
            &out[..out.len().min(40)]
        );
        assert!(
            out.ends_with("\n\n(Showing lines 1-2. The read tool stops 64 MiB into a file; use bash (tail, sed) for what follows.)"),
            "{out}"
        );
        let deep = read(dir.path(), json!({ "filePath": "a.txt", "offset": 3 })).await;
        assert_eq!(
            deep,
            Err(format!(
                "cannot read {} from line 3: it starts past the first 64 MiB, all the read tool reads; use bash (tail, sed) for that part",
                path.display()
            ))
        );
    }

    #[tokio::test]
    async fn a_line_the_budget_hid_before_the_cap_can_still_be_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        past_the_cap(dir.path());
        let ctx = ToolContext::new(dir.path().to_path_buf());
        // Room for the first line, cut to its 2000 characters, not the
        // second.
        let read = Read::new(
            ReadConfig {
                max_bytes: 3000,
                ..ReadConfig::default()
            },
            crate::post_write::lsp_off(),
        );

        let out = read
            .call(json!({ "filePath": "a.txt" }), &ctx)
            .await
            .expect("read");

        assert!(out.starts_with("1: aaa"), "{}", &out[..out.len().min(40)]);
        assert!(
            out.ends_with(
                "chars)\n\n(Showing lines 1-1 of more than 1. Use offset=2 to continue.)"
            ),
            "{out}"
        );
    }

    #[tokio::test]
    async fn missing_intermediate_component_suggests_sibling() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("source")).unwrap();
        let err = read(dir.path(), json!({"filePath":"sourc/missing/file"}))
            .await
            .unwrap_err();
        assert!(err.contains("cannot read"));
        assert!(err.contains("source"), "{err}");
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
