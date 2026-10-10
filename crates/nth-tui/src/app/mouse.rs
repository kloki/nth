//! What the mouse does: the wheel scrolls the showing tab, a click on the
//! header shows that tab, a click on a link in a chat or on the status
//! bar's pull-request link opens it, and a right click on an entry copies
//! what it says. Where it moves is kept for the empty chat's field to
//! ripple under.

use std::process::Stdio;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use super::{App, Tab};
use crate::{chat::Chat, header, terminal};

const WHEEL_LINES: usize = 3;

/// Where the last draw put the header and the status bar's pull-request
/// link; the chats know their own place.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Areas {
    pub header: Rect,
    pub pr_link: Option<Rect>,
}

impl App {
    pub(super) fn on_mouse(&mut self, mouse: MouseEvent) {
        let at = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => self.pointer = Some(at),
            MouseEventKind::ScrollUp => {
                if let Some(view) = self.active_view() {
                    view.scroll_up(WHEEL_LINES);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(view) = self.active_view() {
                    view.scroll_down(WHEEL_LINES);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.areas.header.contains(at) => {
                let tabs = self.tab_labels();
                if let Some(index) = header::tab_at(&tabs, self.areas.header, at.x) {
                    self.content.select(index);
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.areas.pr_link.is_some_and(|area| area.contains(at)) =>
            {
                self.open_pr();
            }
            MouseEventKind::Down(MouseButton::Left) => self.click_content(at),
            MouseEventKind::Down(MouseButton::Right) => self.copy_entry(at),
            _ => {}
        }
    }

    /// The chat showing, if a chat is: the session's or a subagent's.
    fn shown_chat(&self) -> Option<&Chat> {
        match self.content.active() {
            Tab::Chat => Some(&self.chat),
            Tab::Subagent(id) => self.subagent_views.get(&id).map(|view| &view.chat),
            Tab::Diagnostics | Tab::Monitor(_) | Tab::Plan => None,
        }
    }

    /// The branch's pull request, which the link on the status bar names.
    fn open_pr(&mut self) {
        let Some(url) = self.pr.as_ref().map(|pr| pr.url.clone()) else {
            return;
        };
        self.open_url(&url);
    }

    fn click_content(&mut self, at: Position) {
        let Some(url) = self.shown_chat().and_then(|chat| chat.link_at(at.x, at.y)) else {
            return;
        };
        self.open_url(&url);
    }

    /// Hands `url` to the desktop's opener, and says how it went on the
    /// status bar.
    fn open_url(&mut self, url: &str) {
        self.hint = Some(match (self.opener)(url) {
            Ok(()) => format!("opened {url}"),
            Err(e) => format!("could not open: {e}"),
        });
    }

    fn copy_entry(&mut self, at: Position) {
        let Some(text) = self
            .shown_chat()
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
            Err(e) => format!("could not copy: {e}"),
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
        super::tests::{app, rows},
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
