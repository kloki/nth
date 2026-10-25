//! What the mouse does: the wheel scrolls the tab under it, a click on the
//! header shows that tab, a click on a link in a chat or on the status
//! bar's pull-request link opens it, and a right click on an entry copies
//! what it says. Where it moves is kept for the empty chat's field to
//! ripple under, and in a split it gives the pane under it the focus.

use std::process::Stdio;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::{App, Tab, input::Input};
use crate::{chat::Chat, header, terminal};

const WHEEL_LINES: usize = 3;

/// Where the last draw put the header, the content panel's panes and the
/// status bar's pull-request link; the chats know their own place.
#[derive(Debug, Default, Clone)]
pub(super) struct Areas {
    pub header: Rect,
    /// Each tab drawn, one or two.
    pub panes: Vec<(Tab, Rect)>,
    pub pr_link: Option<Rect>,
    pub curio: Option<Rect>,
    /// The `/connect` panel's login URL, while it shows.
    pub login_url: Option<Rect>,
}

impl Areas {
    /// Where the right pane starts when the panel is split, which the
    /// header lines its tabs up with.
    pub fn header_split(&self) -> Option<u16> {
        self.panes.get(1).map(|&(_, area)| area.x)
    }
}

impl App {
    pub(super) fn on_mouse(&mut self, mouse: MouseEvent) {
        let at = Position::new(mouse.column, mouse.row);
        self.pointer = Some(at);
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
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
            MouseEventKind::Down(MouseButton::Left) if self.areas.header.contains(at) => {
                let tabs = self.tab_labels();
                let split = self.areas.header_split();
                if let Some(index) = header::tab_at(&tabs, self.areas.header, split, at.x) {
                    self.content.select(index);
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.areas.pr_link.is_some_and(|area| area.contains(at)) =>
            {
                self.open_pr();
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.areas.login_url.is_some_and(|area| area.contains(at)) =>
            {
                self.open_login_url();
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.areas.curio.is_some_and(|area| area.contains(at)) =>
            {
                self.open_latest_curio()
            }
            MouseEventKind::Down(MouseButton::Left) => self.click_content(at),
            MouseEventKind::Down(MouseButton::Right) => self.copy_entry(at),
            _ => {}
        }
    }

    /// Use the same targets as left clicks, with the layout of the latest
    /// frame so a stationary mouse follows changes beneath it too.
    pub(super) fn hovering_clickable(&mut self) -> bool {
        let Some(at) = self.pointer else {
            return false;
        };
        if self.areas.header.contains(at) {
            return header::tab_at(
                &self.tab_labels(),
                self.areas.header,
                self.areas.header_split(),
                at.x,
            )
            .is_some();
        }
        [self.areas.pr_link, self.areas.login_url, self.areas.curio]
            .into_iter()
            .flatten()
            .any(|area| area.contains(at))
            || self
                .chat_at(at)
                .and_then(|chat| chat.link_at(at.x, at.y))
                .is_some()
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

    fn click_content(&mut self, at: Position) {
        let Some(url) = self.chat_at(at).and_then(|chat| chat.link_at(at.x, at.y)) else {
            return;
        };
        self.open_url(&url);
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
        super::tests::{app, rows, rows_of},
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

    #[tokio::test]
    async fn hand_pointer_tracks_tabs_and_split_header_gaps() {
        let mut app = app().with_split_width(80);
        app.open_content(Tab::Diagnostics);
        rows_of(&mut app, 100);
        assert!(!app.hovering_clickable());
        hover(&mut app, 3, 0);
        assert!(app.hovering_clickable());
        hover(&mut app, 30, 0);
        assert!(!app.hovering_clickable());
        let x = app.areas.header_split().unwrap();
        hover(&mut app, x + 2, 0);
        assert!(app.hovering_clickable());
        hover(&mut app, 0, 0);
        assert!(!app.hovering_clickable());
    }

    #[tokio::test]
    async fn hand_pointer_tracks_only_clickable_chat_links_and_redraws() {
        let mut app = with_answer("see [docs](file:///etc/passwd) and [more](https://x.y)");
        let lines = rows(&mut app);
        let y = lines
            .iter()
            .position(|line| line.contains("see docs"))
            .unwrap();
        let docs = lines[y].find("docs").unwrap();
        let more = lines[y].find("more").unwrap();
        hover(&mut app, docs as u16, y as u16);
        assert!(!app.hovering_clickable());
        hover(&mut app, more as u16, y as u16);
        assert!(app.hovering_clickable());

        // A tab switch replaces the link beneath a stationary mouse.
        app.open_content(Tab::Diagnostics);
        rows(&mut app);
        assert!(!app.hovering_clickable());
        app.content.select_tab(Tab::Chat);
        rows(&mut app);
        assert!(app.hovering_clickable());
    }

    #[test]
    fn hand_pointer_tracks_status_and_login_hit_areas() {
        let mut app = app();
        rows(&mut app);
        let area = Rect::new(5, 20, 10, 1);
        for target in 0..3 {
            app.areas.pr_link = (target == 0).then_some(area);
            app.areas.curio = (target == 1).then_some(area);
            app.areas.login_url = (target == 2).then_some(area);
            hover(&mut app, 5, 20);
            assert!(app.hovering_clickable());
            hover(&mut app, 15, 20);
            assert!(!app.hovering_clickable());
        }
        // The next frame clears targets no longer drawn.
        hover(&mut app, 5, 20);
        rows(&mut app);
        assert!(!app.hovering_clickable());
    }

    #[tokio::test]
    async fn hand_pointer_finds_subagent_links_in_a_split() {
        let mut app = app().with_split_width(80);
        let mut view = crate::subagent::SubagentView::new(
            "general".into(),
            "research".into(),
            app.cwd.clone(),
        );
        view.chat
            .apply(&Event::TextDelta("[subagent docs](https://x.y)".into()));
        view.chat
            .transcript
            .finish_turn(Ok(()), "glm", Duration::from_secs(1));
        app.subagent_views.insert(1, view);
        app.open_content(Tab::Subagent(1));
        let lines = rows_of(&mut app, 100);
        let y = lines
            .iter()
            .position(|line| line.contains("subagent docs"))
            .unwrap();
        let x = lines[y].find("subagent docs").unwrap();
        hover(&mut app, x as u16, y as u16);
        assert!(app.hovering_clickable());
        hover(&mut app, 3, y as u16);
        assert!(!app.hovering_clickable());
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
            app.chat.link_at(more as u16, row as u16).as_deref(),
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
        let link = app.areas.pr_link.expect("the link shows");

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
