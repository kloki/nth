//! A file tab: a file the model showed you, as rendered markdown or as
//! highlighted code, read again whenever it changes on disk.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
};
use tokio::{io::AsyncReadExt, sync::mpsc, task::JoinHandle};

use crate::{
    pager::{Cache, Pager},
    rich, theme,
};

/// Which file tab a read is for.
pub type FileId = u32;

/// What the watcher read: the file's head, or `None` once it can't be.
pub type FileRead = (FileId, Option<Contents>);

/// How often the watcher looks whether the file changed.
const POLL: Duration = Duration::from_millis(500);

/// The most of a file a tab reads. A log the model shows can grow without
/// end, and the tab reads it again on every change and lays all of it out.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A file's text as the watcher read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    /// The whole file, or its lines up to [`MAX_FILE_BYTES`].
    pub text: String,
    /// The file goes on past `text`.
    pub cut: bool,
}

impl From<&str> for Contents {
    fn from(text: &str) -> Self {
        Self {
            text: text.into(),
            cut: false,
        }
    }
}

#[derive(Debug)]
pub struct FileView {
    path: PathBuf,
    /// `None` until first read, and once the file is gone.
    text: Option<Contents>,
    /// The watcher has reported at least once.
    read: bool,
    pub pager: Pager,
    cache: Cache,
    _watcher: Watcher,
}

impl FileView {
    /// Starts watching `path`, sending what it reads as `id` on `reads`.
    pub fn new(id: FileId, path: PathBuf, reads: mpsc::Sender<FileRead>) -> Self {
        Self {
            _watcher: Watcher::start(id, path.clone(), reads),
            path,
            text: None,
            read: false,
            pager: Pager::default(),
            cache: Cache::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file now reads `text`, or can't be read with `None`. The view
    /// stays where it was, so a reload does not jump.
    pub fn update(&mut self, text: Option<Contents>) {
        self.read = true;
        if text != self.text {
            self.text = text;
            self.cache.invalidate();
        }
    }

    /// The file's name.
    pub fn label(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// `place` is the file as the header names it.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, place: &str) {
        let width = usize::from(area.width);
        let lines = match self.cache.take(width) {
            Some(lines) => lines,
            None => self.lines(place, width),
        };
        self.pager.render(frame, area, lines.clone());
        self.cache.keep(width, lines);
    }

    /// A header naming the file, then the file wrapped to `width`.
    fn lines(&self, place: &str, width: usize) -> Vec<Line<'static>> {
        let header = match (&self.text, self.read) {
            (None, true) => format!("{place} · gone"),
            _ => place.to_string(),
        };
        let mut lines = vec![Line::styled(header, theme::dim()), Line::default()];
        let Some(Contents { text, cut }) = &self.text else {
            return lines;
        };
        let path = self.path.to_string_lossy();
        if rich::is_markdown(&path) {
            lines.extend(rich::markdown(text, width).lines);
        } else {
            lines.extend(numbered(rich::code(text, &path, width), width));
        }
        if *cut {
            lines.push(Line::default());
            let mib = MAX_FILE_BYTES / (1024 * 1024);
            lines.push(Line::styled(format!("(cut at {mib} MiB)"), theme::dim()));
        }
        lines
    }
}

/// Code lines wrapped to `width` behind their line numbers, right-aligned
/// in a dim gutter; the rows a line wraps onto leave the gutter blank.
fn numbered(code: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let digits = code.len().max(1).to_string().len();
    // The number and a space after it.
    let gutter = digits + 1;
    let room = width.saturating_sub(gutter).max(1);
    let mut rows = Vec::new();
    for (i, line) in code.into_iter().enumerate() {
        for (j, row) in rich::wrap(line, room).into_iter().enumerate() {
            let number = if j == 0 {
                format!("{:>digits$} ", i + 1)
            } else {
                " ".repeat(gutter)
            };
            let mut spans = vec![Span::styled(number, theme::dim())];
            spans.extend(row.spans);
            rows.push(Line::from(spans));
        }
    }
    rows
}

/// Reads the file whenever its size or modification time changes, until
/// dropped.
#[derive(Debug)]
struct Watcher(JoinHandle<()>);

impl Watcher {
    fn start(id: FileId, path: PathBuf, reads: mpsc::Sender<FileRead>) -> Self {
        Self(tokio::spawn(async move {
            let mut seen: Option<Option<(SystemTime, u64)>> = None;
            loop {
                let stamp = tokio::fs::metadata(&path)
                    .await
                    .ok()
                    .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
                if seen != Some(stamp) {
                    seen = Some(stamp);
                    // A file that can't be read shows as gone, and comes
                    // back once it can.
                    let text = read_head(&path).await.ok().flatten();
                    if reads.send((id, text)).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        }))
    }
}

/// Reads `path` up to [`MAX_FILE_BYTES`]; a file cut there ends at its last
/// whole line. `None` for a file that is not UTF-8, which has no text to
/// show.
async fn read_head(path: &Path) -> std::io::Result<Option<Contents>> {
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    // One byte past the cap tells a file that ends there from a longer one.
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    let cut = bytes.len() as u64 > MAX_FILE_BYTES;
    if cut {
        let end = bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(MAX_FILE_BYTES as usize, |newline| newline + 1);
        bytes.truncate(end);
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            // A single line past the cap may end inside a character.
            let valid = e.utf8_error().valid_up_to();
            if !cut || e.utf8_error().error_len().is_some() {
                return Ok(None);
            }
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).map_err(std::io::Error::other)?
        }
    };
    Ok(Some(Contents { text, cut }))
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn view(path: &str, content: &str) -> FileView {
        let (tx, _) = mpsc::channel(1);
        let mut view = FileView::new(1, path.into(), tx);
        view.update(Some(content.into()));
        view
    }

    #[tokio::test]
    async fn markdown_renders_with_its_syntax_hidden() {
        let view = view("/p/notes.md", "# Notes\nsome text\n");

        let lines = view.lines("notes.md", 40);

        assert_eq!(text(&lines), ["notes.md", "", "Notes", "some text"]);
        let heading = &lines[2].spans[0].style;
        assert!(heading.add_modifier.contains(Modifier::BOLD), "{heading:?}");
    }

    #[tokio::test]
    async fn code_is_highlighted_and_wrapped() {
        let view = view("/p/main.rs", "fn main() { let answer = 42; }\n");

        let lines = view.lines("main.rs", 16);

        let body: Vec<String> = text(&lines[2..])
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(body, ["1 fn main() {", "  let answer =", "  42; }"]);
        assert_eq!(lines[2].spans[0].style, theme::dim(), "a dim number");
        assert!(
            lines[2..]
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg.is_some()),
            "highlighted"
        );
    }

    #[test]
    fn numbers_are_right_aligned_to_the_widest() {
        let code: Vec<Line<'static>> = (1..=10).map(|i| Line::raw(format!("x{i}"))).collect();

        let rows = text(&numbered(code, 20));

        assert_eq!(rows[0], " 1 x1");
        assert_eq!(rows[9], "10 x10");
    }

    #[tokio::test]
    async fn a_reload_keeps_the_view_where_it_was() {
        let mut view = view("/p/a.txt", &"line\n".repeat(30));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 10)).expect("backend");
        terminal
            .draw(|frame| view.draw(frame, frame.area(), "a.txt"))
            .expect("draws");
        view.pager.scroll_down(5);
        assert!(view.pager.scrollbar().is_some());

        view.update(Some(Contents::from("more\n".repeat(40).as_str())));
        terminal
            .draw(|frame| view.draw(frame, frame.area(), "a.txt"))
            .expect("draws");

        assert_eq!(view.pager.top(), 5);
    }

    #[tokio::test]
    async fn a_file_that_went_away_says_so() {
        let mut view = view("/p/a.rs", "fn a() {}\n");
        view.update(None);

        assert_eq!(text(&view.lines("a.rs", 40)), ["a.rs · gone", ""]);
        assert_eq!(view.label(), "a.rs");
    }

    #[tokio::test]
    async fn the_watcher_reads_the_file_again_when_it_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.md");
        std::fs::write(&path, "one\n").expect("writes");
        let (tx, mut rx) = mpsc::channel(4);
        let _view = FileView::new(7, path.clone(), tx);

        assert_eq!(rx.recv().await, Some((7, Some("one\n".into()))));
        std::fs::write(&path, "one\ntwo\n").expect("writes");
        assert_eq!(rx.recv().await, Some((7, Some("one\ntwo\n".into()))));
        std::fs::remove_file(&path).expect("removes");
        assert_eq!(rx.recv().await, Some((7, None)));
    }

    #[tokio::test]
    async fn a_file_past_the_cap_shows_its_head_and_a_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.log");
        let line = "0123456789abcdef\n";
        let lines = MAX_FILE_BYTES as usize / line.len() + 10;
        std::fs::write(&path, line.repeat(lines)).expect("writes");
        let (tx, mut rx) = mpsc::channel(4);
        let _watching = FileView::new(7, path, tx);

        let Some((7, Some(read))) = rx.recv().await else {
            panic!("the watcher reads the file");
        };
        assert!(read.cut, "past the cap");
        assert!(read.text.len() as u64 <= MAX_FILE_BYTES);
        assert!(read.text.ends_with(line), "ends on a whole line");

        // Laid out small: the note is what matters, not 4 MiB of lines.
        let mut view = view("/p/big.log", "");
        view.update(Some(Contents {
            text: "head\n".into(),
            cut: true,
        }));
        let shown: Vec<String> = text(&view.lines("big.log", 40))
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(shown, ["big.log", "", "1 head", "", "(cut at 4 MiB)"]);
    }

    #[tokio::test]
    async fn a_single_line_past_the_cap_is_cut_on_a_character() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("one.txt");
        // Three-byte characters, so the cap lands inside one.
        let text = "€".repeat(MAX_FILE_BYTES as usize / 3 + 1);
        std::fs::write(&path, text).expect("writes");

        let read = read_head(&path).await.expect("reads").expect("text");
        assert!(read.cut);
        assert_eq!(read.text.len() as u64, MAX_FILE_BYTES - MAX_FILE_BYTES % 3);
    }
}
