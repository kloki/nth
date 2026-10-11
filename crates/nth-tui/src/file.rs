//! A file tab: a file the model showed you, as rendered markdown or as
//! highlighted code, read again whenever it changes on disk.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, ScrollbarState},
};
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{rich, theme};

/// Which file tab a read is for.
pub type FileId = u32;

/// What the watcher read: the file's text, or `None` once it can't be.
pub type FileRead = (FileId, Option<String>);

/// How often the watcher looks whether the file changed.
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct FileView {
    path: PathBuf,
    /// `None` until first read, and once the file is gone.
    text: Option<String>,
    /// The watcher has reported at least once.
    read: bool,
    /// The first line in view, and how far it can go, as of the last draw.
    top: usize,
    max_top: usize,
    height: usize,
    /// The file as drawn last, and the width it was wrapped for.
    rendered: Option<(usize, Vec<Line<'static>>)>,
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
            top: 0,
            max_top: 0,
            height: 0,
            rendered: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file now reads `text`, or can't be read with `None`. The view
    /// stays where it was, so a reload does not jump.
    pub fn update(&mut self, text: Option<String>) {
        self.read = true;
        if text != self.text {
            self.text = text;
            self.rendered = None;
        }
    }

    /// The file's name.
    pub fn label(&self) -> String {
        self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = (self.top + lines).min(self.max_top);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.max(1));
    }

    pub fn jump_top(&mut self) {
        self.top = 0;
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
    }

    /// Where the view sits in the file, while it is longer than the tab.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        (self.max_top > 0).then(|| {
            ScrollbarState::new(self.max_top + 1)
                .position(self.top)
                .viewport_content_length(self.height)
        })
    }

    /// `place` is the file as the header names it.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, place: &str) {
        let width = usize::from(area.width);
        let lines = match self.rendered.take() {
            Some((drawn, lines)) if drawn == width => lines,
            _ => self.lines(place, width),
        };
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = self.top.min(self.max_top);
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines.clone()).scroll((top, 0)), area);
        self.rendered = Some((width, lines));
    }

    /// A header naming the file, then the file wrapped to `width`.
    fn lines(&self, place: &str, width: usize) -> Vec<Line<'static>> {
        let header = match (&self.text, self.read) {
            (None, true) => format!("{place} · gone"),
            _ => place.to_string(),
        };
        let mut lines = vec![Line::styled(header, theme::dim()), Line::default()];
        let Some(text) = &self.text else {
            return lines;
        };
        let path = self.path.to_string_lossy();
        if rich::is_markdown(&path) {
            lines.extend(rich::markdown(text, width).lines);
        } else {
            for line in rich::code(text, &path, width) {
                lines.extend(rich::wrap(line, width));
            }
        }
        lines
    }
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
                    let text = tokio::fs::read_to_string(&path).await.ok();
                    if reads.send((id, text)).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        }))
    }
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
        assert_eq!(body, ["fn main() { let", "answer = 42; }"]);
        assert!(
            lines[2..]
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.fg.is_some()),
            "highlighted"
        );
    }

    #[tokio::test]
    async fn a_reload_keeps_the_view_where_it_was() {
        let mut view = view("/p/a.txt", &"line\n".repeat(30));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 10)).expect("backend");
        terminal
            .draw(|frame| view.draw(frame, frame.area(), "a.txt"))
            .expect("draws");
        view.scroll_down(5);
        assert!(view.scrollbar().is_some());

        view.update(Some("more\n".repeat(40)));
        terminal
            .draw(|frame| view.draw(frame, frame.area(), "a.txt"))
            .expect("draws");

        assert_eq!(view.top, 5);
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
}
