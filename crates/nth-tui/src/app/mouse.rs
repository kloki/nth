//! What the mouse does: the wheel scrolls the tab under it, a click on a
//! header tab shows it, a click on a link in a chat or on the status bar's
//! pull-request link or curio opens it, and a right click on an entry, or
//! on the status bar's working directory or branch, copies what it says. Whatever a click would act on changes colour while
//! the pointer is over it, and the chat entry under it gets a background,
//! so two things can be hovered at once. Where it moves is kept for the empty
//! chat's field to ripple under, and in a split it gives the pane under it
//! the focus.
//!
//! Something new to point at is a [`Target`]: recorded with
//! [`Areas::target`] where it is drawn, and acted on in [`App::click`] or
//! [`App::right_click`].

use std::process::Stdio;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
};

use super::{App, Tab, input::Input};
use crate::{chat::Chat, terminal, theme};

const WHEEL_LINES: usize = 3;

/// Something the pointer can be over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    /// The header's tab at this index of `App::tab_labels`.
    Tab(usize),
    /// The working directory on the status bar; a right click copies it.
    Place,
    /// The branch on the status bar; a right click copies it.
    Branch,
    /// The branch's pull request on the status bar.
    PullRequest,
    /// The status bar's curio icon.
    Curio,
    /// The `/connect` panel's login URL, while it shows.
    LoginUrl,
    /// A web address in a chat.
    Link(String),
    /// A chat entry, shaded while hovered; a click on it does nothing.
    Entry,
}

/// How a hovered target is marked, over what the draw left.
enum Hover {
    /// Its text in this colour.
    Text(Color),
    /// This background behind it, wherever nothing set one, so a code
    /// block keeps its own.
    Background(Color),
}

impl Target {
    fn hover(&self) -> Hover {
        match self {
            Target::Tab(_) | Target::Link(_) => Hover::Text(theme::HOVER),
            Target::Place
            | Target::Branch
            | Target::PullRequest
            | Target::Curio
            | Target::LoginUrl => Hover::Text(theme::HOVER_STATUS),
            Target::Entry => Hover::Background(theme::HOVER_ENTRY),
        }
    }
}

/// Where the last draw put the content panel's panes and what can be
/// pointed at; the chats know their own place.
#[derive(Debug, Default, Clone)]
pub(super) struct Areas {
    /// Each tab drawn, one or two.
    pub panes: Vec<(Tab, Rect)>,
    /// What can be pointed at, in the order it was drawn.
    targets: Vec<(Target, Rect)>,
    /// Drawn over the panes, hiding what the chats would find under them.
    covers: Vec<Rect>,
}

impl Areas {
    /// Where the right pane starts when the panel is split, which the
    /// header lines its tabs up with.
    pub fn header_split(&self) -> Option<u16> {
        self.panes.get(1).map(|&(_, area)| area.x)
    }

    /// Records that `target` was drawn over `area`.
    pub fn target(&mut self, target: Target, area: Rect) {
        self.targets.push((target, area));
    }

    /// Records a popup drawn over `area`.
    pub fn cover(&mut self, area: Rect) {
        self.covers.push(area);
    }

    /// Drops `target` until it is drawn again, when what it pointed at is
    /// gone before the next draw.
    pub fn forget(&mut self, target: &Target) {
        self.targets.retain(|(t, _)| t != target);
    }

    #[cfg(test)]
    pub fn find(&self, target: &Target) -> Option<Rect> {
        self.targets
            .iter()
            .find(|(t, _)| t == target)
            .map(|&(_, area)| area)
    }
}

impl App {
    pub(super) fn on_mouse(&mut self, mouse: MouseEvent) {
        let at = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                self.pointer = Some(at);
                // Only a split has a pane without the focus.
                if let Some(tab) = self.pane_at(at) {
                    self.content.select_tab(tab);
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some(view) = self.view_at(at) {
                    view.scroll_up(WHEEL_LINES);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(view) = self.view_at(at) {
                    view.scroll_down(WHEEL_LINES);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((target, _)) = self.hits_at(at).into_iter().next() {
                    self.click(target);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some((target, _)) = self.hits_at(at).into_iter().next() {
                    self.right_click(target, at);
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, target: Target) {
        match target {
            Target::Tab(index) => self.content.select(index),
            Target::PullRequest => self.open_pr(),
            Target::Curio => self.open_latest_curio(),
            Target::LoginUrl => self.open_login_url(),
            Target::Link(url) => self.open_url(&url),
            Target::Place | Target::Branch | Target::Entry => {}
        }
    }

    fn right_click(&mut self, target: Target, at: Position) {
        match target {
            Target::Place => self.copy(self.cwd.display().to_string()),
            Target::Branch => {
                if let Some(branch) = self.git.as_ref().and_then(|git| git.branch.clone()) {
                    self.copy(branch);
                }
            }
            Target::Link(_) | Target::Entry => self.copy_entry(at),
            Target::Tab(_) | Target::PullRequest | Target::Curio | Target::LoginUrl => {}
        }
    }

    /// Everything under `at`, the one a click acts on first: what the last
    /// draw recorded, newest first, then, unless a popup hides it, the link
    /// and the entry of the chat there.
    fn hits_at(&self, at: Position) -> Vec<(Target, Rect)> {
        let mut hits: Vec<_> = self
            .areas
            .targets
            .iter()
            .rev()
            .filter(|(_, area)| area.contains(at))
            .cloned()
            .collect();
        if self.areas.covers.iter().any(|area| area.contains(at)) {
            return hits;
        }
        if let Some(chat) = self.chat_at(at) {
            if let Some((area, url)) = chat.link_at(at.x, at.y) {
                hits.push((Target::Link(url), area));
            }
            if let Some(area) = chat.entry_area_at(at.x, at.y) {
                hits.push((Target::Entry, area));
            }
        }
        hits
    }

    /// Marks what is under the pointer, over the finished draw.
    pub(super) fn draw_hover(&self, frame: &mut Frame) {
        let Some(at) = self.pointer else {
            return;
        };
        let buffer = frame.buffer_mut();
        for (target, area) in self.hits_at(at) {
            let area = area.intersection(buffer.area);
            match target.hover() {
                Hover::Text(colour) => buffer.set_style(area, Style::new().fg(colour)),
                Hover::Background(colour) => {
                    for y in area.top()..area.bottom() {
                        let row = Rect {
                            y,
                            height: 1,
                            ..area
                        };
                        // The blank separator above an entry stays as it is.
                        if row.positions().all(|at| buffer[at].symbol() == " ") {
                            continue;
                        }
                        for at in row.positions() {
                            let cell = &mut buffer[at];
                            if cell.bg == Color::Reset {
                                cell.bg = colour;
                            }
                        }
                    }
                }
            }
        }
    }

    /// The tab of the pane under `at`, if the last draw put one there.
    fn pane_at(&self, at: Position) -> Option<Tab> {
        self.areas
            .panes
            .iter()
            .find(|(_, area)| area.contains(at))
            .map(|&(tab, _)| tab)
    }

    /// The view of the pane under `at`, else the focused one.
    fn view_at(&mut self, at: Position) -> Option<&mut dyn super::content::Scrollable> {
        let tab = self.pane_at(at).unwrap_or(self.content.active());
        self.view_mut(tab)
    }

    /// The chat under `at`, if a chat is there: the session's or a
    /// subagent's.
    fn chat_at(&self, at: Position) -> Option<&Chat> {
        match self.pane_at(at)? {
            Tab::Chat => Some(&self.chat),
            Tab::Subagent(id) => self.subagent_views.get(&id).map(|view| &view.chat),
            Tab::Diagnostics
            | Tab::File(_)
            | Tab::Monitor(_)
            | Tab::Plan
            | Tab::Usage
            | Tab::Work => None,
        }
    }

    /// The branch's pull request, which the link on the status bar names.
    fn open_pr(&mut self) {
        let Some(url) = self.pr.as_ref().map(|pr| pr.url.clone()) else {
            return;
        };
        self.open_url(&url);
    }

    /// The `/connect` panel's URL, in case the browser did not open.
    fn open_login_url(&mut self) {
        let Input::Connect(panel) = &self.input else {
            return;
        };
        if let Some(url) = panel.url().map(str::to_string) {
            self.open_url(&url);
        }
    }

    /// Hands `url` to the desktop's opener, and says how it went on the
    /// status bar.
    fn open_url(&mut self, url: &str) {
        self.hint = Some(match (self.opener)(url) {
            Ok(()) => format!("opened {url}").into(),
            Err(e) => format!("could not open: {e}").into(),
        });
    }

    fn copy_entry(&mut self, at: Position) {
        let Some(text) = self
            .chat_at(at)
            .and_then(|chat| chat.entry_at(at.x, at.y))
            .and_then(|entry| entry.clipboard())
        else {
            return;
        };
        self.copy(text);
    }

    /// Puts `text` on the clipboard, and says how it went on the status bar.
    fn copy(&mut self, text: String) {
        if text.len() > terminal::COPY_LIMIT {
            self.hint = Some("too large to copy".into());
            return;
        }
        self.hint = Some(match (self.clipboard)(&text) {
            Ok(()) => "copied".into(),
            Err(e) => format!("could not copy: {e}").into(),
        });
    }
}

/// Hands `url` to the desktop's opener and comes back at once. The child is
/// let go of: tokio reaps it when it exits, and the browser it starts is
/// meant to outlive nth. `Ok` means the opener started, not that it found
/// a browser.
pub(super) fn open(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    tokio::process::Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{Event as TermEvent, KeyModifiers};
    use nth_protocol::Event;

    use super::{
        super::tests::{app, buffer, rows, rows_of},
        *,
    };

    fn click(app: &mut App, button: MouseButton, column: u16, row: u16) {
        app.on_terminal(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn hover(app: &mut App, column: u16, row: u16) {
        app.on_terminal(TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    #[tokio::test]
    async fn hovering_a_pane_of_the_split_gives_it_the_focus() {
        let mut app = app().with_split_width(80);
        app.open_content(Tab::Diagnostics);
        app.apply(super::super::keys::Action::Content(0));
        rows_of(&mut app, 100);
        hover(&mut app, 80, 5);
        assert_eq!(app.content.active(), Tab::Diagnostics);
        hover(&mut app, 10, 5);
        assert_eq!(app.content.active(), Tab::Chat);
        hover(&mut app, 80, 0);
        assert_eq!(app.content.active(), Tab::Chat, "the header is no pane");

        rows_of(&mut app, 60);
        hover(&mut app, 50, 5);
        assert_eq!(app.content.active(), Tab::Chat, "narrow, one pane");
    }

    /// An app whose chat shows one answer, drawn once so clicks can land.
    fn with_answer(text: &str) -> App {
        let mut app = app();
        app.clipboard = |_| Ok(());
        app.chat.apply(&Event::TextDelta(text.into()));
        app.chat
            .transcript
            .finish_turn(Ok(()), "glm", Duration::from_secs(1));
        rows(&mut app);
        app
    }

    /// The column `needle` starts at on `row`, which holds wide and
    /// multi-byte symbols such as the bar.
    fn column(row: &str, needle: &str) -> u16 {
        let byte = row.find(needle).expect("it shows");
        row[..byte].chars().count() as u16
    }

    fn fg(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> Color {
        buffer[(x, y)].fg
    }

    /// The rows with the hovered entry's background.
    fn shaded(buffer: &ratatui::buffer::Buffer) -> Vec<u16> {
        (0..buffer.area.height)
            .filter(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].bg == theme::HOVER_ENTRY))
            .collect()
    }

    #[tokio::test]
    async fn hovering_a_tab_colours_it_alone() {
        let mut app = app();
        app.open_content(Tab::Diagnostics);
        rows(&mut app);
        let tab = app.areas.find(&Target::Tab(1)).expect("the tab shows");

        hover(&mut app, tab.x + 2, 0);
        let drawn = buffer(&mut app);
        assert!((tab.left()..tab.right()).all(|x| fg(&drawn, x, 0) == theme::HOVER));
        assert_ne!(fg(&drawn, tab.x - 1, 0), theme::HOVER, "the chat's tab");

        hover(&mut app, 30, 5);
        let drawn = buffer(&mut app);
        assert_ne!(
            fg(&drawn, tab.x + 2, 0),
            theme::HOVER,
            "moving off clears it"
        );
    }

    #[test]
    fn hovering_a_link_colours_it_and_shades_its_entry() {
        let mut app = app();
        app.chat.transcript.push_user("hi".into());
        app.chat
            .apply(&Event::TextDelta("see [docs](https://x.y) now".into()));
        let rows = rows(&mut app);
        let row = rows
            .iter()
            .position(|row| row.contains("see docs"))
            .expect("the answer shows") as u16;
        let user = rows
            .iter()
            .position(|row| row.contains("hi"))
            .expect("the prompt shows") as u16;
        let docs = column(&rows[usize::from(row)], "docs");

        hover(&mut app, docs + 1, row);
        let drawn = buffer(&mut app);
        assert!((docs..docs + 4).all(|x| fg(&drawn, x, row) == theme::HOVER));
        assert_ne!(fg(&drawn, docs - 1, row), theme::HOVER);
        assert_ne!(fg(&drawn, docs + 4, row), theme::HOVER);
        assert_eq!(shaded(&drawn), [row], "both at once, the separator left be");

        // Off the link, on its entry: only the background.
        hover(&mut app, docs + 6, row);
        let drawn = buffer(&mut app);
        assert_ne!(fg(&drawn, docs + 1, row), theme::HOVER);
        assert_eq!(shaded(&drawn), [row]);

        hover(&mut app, 3, user);
        let drawn = buffer(&mut app);
        assert_eq!(shaded(&drawn), [user]);
    }

    #[test]
    fn a_bare_address_is_marked_without_its_full_stop() {
        let mut app = app();
        app.chat
            .apply(&Event::TextDelta("go to https://a.b.".into()));
        let rows = rows(&mut app);
        let row = rows
            .iter()
            .position(|row| row.contains("https://a.b"))
            .expect("the answer shows");
        let at = column(&rows[row], "https");
        let row = row as u16;

        hover(&mut app, at, row);
        let drawn = buffer(&mut app);
        assert!((at..at + 11).all(|x| fg(&drawn, x, row) == theme::HOVER));
        assert_ne!(fg(&drawn, at + 11, row), theme::HOVER, "the full stop");
    }

    #[test]
    fn a_code_block_keeps_its_background_in_a_shaded_entry() {
        let mut app = app();
        app.chat.apply(&Event::TextDelta(
            "look:\n\n```rust\nlet x = 1;\n```".into(),
        ));
        let rows = rows(&mut app);
        let code = rows
            .iter()
            .position(|row| row.contains("let x"))
            .expect("the code shows") as u16;
        let x = column(&rows[usize::from(code)], "let");
        let before = buffer(&mut app)[(x, code)].bg;
        assert_ne!(before, Color::Reset, "hoodrich gives code its background");

        hover(&mut app, x, code);
        let drawn = buffer(&mut app);
        assert_eq!(drawn[(x, code)].bg, before);
        assert!(
            !shaded(&drawn).is_empty(),
            "the rest of the entry is shaded"
        );
    }

    #[test]
    fn hovering_the_pull_request_link_whitens_its_number() {
        let mut app = app();
        app.git = Some(crate::git::GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        });
        app.pr = Some(crate::git::Pr {
            number: 123,
            url: "https://x.y/repo/pull/123".into(),
            state: crate::git::State::Open,
        });
        rows(&mut app);
        let link = app
            .areas
            .find(&Target::PullRequest)
            .expect("the link shows");

        hover(&mut app, link.x, link.y);
        let drawn = buffer(&mut app);
        let hovered = |x| fg(&drawn, x, link.y) == theme::HOVER_STATUS;
        assert!((link.left()..link.right()).all(hovered));
        // `main` before it stays green.
        assert!(!hovered(link.x - 2));
    }

    #[test]
    fn a_right_click_on_the_working_directory_copies_it() {
        let mut app = app();
        app.clipboard = |text| {
            assert_eq!(text, "/repo");
            Ok(())
        };
        rows(&mut app);
        let place = app.areas.find(&Target::Place).expect("the place shows");

        hover(&mut app, place.x, place.y);
        let drawn = buffer(&mut app);
        assert!(
            (place.left()..place.right()).all(|x| fg(&drawn, x, place.y) == theme::HOVER_STATUS)
        );

        click(&mut app, MouseButton::Right, place.x, place.y);
        assert_eq!(app.hint.as_deref(), Some("copied"));
    }

    #[test]
    fn a_right_click_on_the_branch_copies_it() {
        let mut app = app();
        app.clipboard = |text| {
            assert_eq!(text, "main");
            Ok(())
        };
        app.git = Some(crate::git::GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        });
        let rows = rows(&mut app);
        let branch = app.areas.find(&Target::Branch).expect("the branch shows");
        let shown: String = rows[usize::from(branch.y)]
            .chars()
            .skip(usize::from(branch.x))
            .take(usize::from(branch.width))
            .collect();
        assert_eq!(shown, "main");

        // A left click does nothing.
        click(&mut app, MouseButton::Left, branch.x, branch.y);
        assert_eq!(app.hint, None);
        click(&mut app, MouseButton::Right, branch.x, branch.y);
        assert_eq!(app.hint.as_deref(), Some("copied"));
    }

    #[tokio::test]
    async fn a_click_on_a_tab_shows_it() {
        let mut app = app();
        app.open_content(Tab::Diagnostics);
        let header = rows(&mut app)[0].clone();
        assert!(header.starts_with("  › chat ["), "{header:?}");

        click(&mut app, MouseButton::Left, 3, 0);
        assert_eq!(app.content.active(), Tab::Chat);

        click(&mut app, MouseButton::Left, 12, 0);
        assert_eq!(app.content.active(), Tab::Diagnostics);

        // Past the tabs, and in the margin before them: nothing changes.
        click(&mut app, MouseButton::Left, 30, 0);
        click(&mut app, MouseButton::Left, 0, 0);
        assert_eq!(app.content.active(), Tab::Diagnostics);
    }

    #[test]
    fn a_right_click_copies_the_entry_under_it() {
        let mut app = with_answer("hello *there*");
        let rows = rows(&mut app);
        let row = rows
            .iter()
            .position(|row| row.contains("hello there"))
            .expect("the answer shows");

        click(&mut app, MouseButton::Right, 3, row as u16);
        assert_eq!(app.hint.as_deref(), Some("copied"));

        // The turn's footer has nothing to copy, nor has the header.
        app.hint = None;
        click(&mut app, MouseButton::Right, 3, row as u16 + 2);
        click(&mut app, MouseButton::Right, 3, 0);
        assert_eq!(app.hint, None);
    }

    #[test]
    fn a_copy_over_the_limit_is_refused() {
        let mut app = with_answer(&"x".repeat(terminal::COPY_LIMIT + 1));
        let row = rows(&mut app)
            .iter()
            .position(|row| row.contains("xxx"))
            .expect("the answer shows");

        click(&mut app, MouseButton::Right, 3, row as u16);
        assert_eq!(app.hint.as_deref(), Some("too large to copy"));
    }

    #[test]
    fn only_web_links_open() {
        let mut app = with_answer("see [docs](file:///etc/passwd) and [more](https://x.y)");
        let rows = rows(&mut app);
        let row = rows
            .iter()
            .position(|row| row.contains("see docs"))
            .expect("the answer shows");
        let line = &rows[row];
        let docs = line.find("docs").expect("docs shows");
        let more = line.find("more").expect("more shows");

        click(&mut app, MouseButton::Left, docs as u16, row as u16);
        assert_eq!(app.hint, None);

        assert_eq!(
            app.chat
                .link_at(more as u16, row as u16)
                .map(|(_, url)| url)
                .as_deref(),
            Some("https://x.y")
        );
    }

    #[test]
    fn a_click_on_the_pull_request_link_opens_it() {
        let mut app = app();
        app.git = Some(crate::git::GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        });
        app.pr = Some(crate::git::Pr {
            number: 123,
            url: "https://x.y/repo/pull/123".into(),
            state: crate::git::State::Merged,
        });
        app.opener = |url| {
            assert_eq!(url, "https://x.y/repo/pull/123");
            Ok(())
        };
        rows(&mut app); // Where the link was drawn decides the click.
        let link = app
            .areas
            .find(&Target::PullRequest)
            .expect("the link shows");

        click(&mut app, MouseButton::Left, link.x + 1, link.y);
        assert_eq!(
            app.hint.as_deref(),
            Some("opened https://x.y/repo/pull/123")
        );

        // A click beside the link does nothing.
        app.hint = None;
        click(
            &mut app,
            MouseButton::Left,
            link.x.saturating_sub(2),
            link.y,
        );
        assert_eq!(app.hint, None);
    }
}
