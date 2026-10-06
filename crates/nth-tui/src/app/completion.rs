//! The completion popup over the prompt: nth's commands and the skills
//! after `/`, agents and files after `@`.

use nth_context::Agents;
use ratatui::{
    Frame,
    layout::{Position, Rect},
};

use super::App;
use crate::{
    command::Entry,
    mention,
    popup::{self, Popup},
};

/// The open completion popup.
pub(super) enum Completion {
    Command(Popup<Entry>),
    /// An agent or a file; `start` is the byte offset of the mention's `@`
    /// in the prompt.
    Mention {
        popup: Popup<mention::Item>,
        start: usize,
    },
}

impl Completion {
    pub(super) fn next(&mut self) {
        match self {
            Completion::Command(popup) => popup.next(),
            Completion::Mention { popup, .. } => popup.next(),
        }
    }

    pub(super) fn prev(&mut self) {
        match self {
            Completion::Command(popup) => popup.prev(),
            Completion::Mention { popup, .. } => popup.prev(),
        }
    }

    /// Where in the prompt the completed token begins; a command is always
    /// the whole prompt.
    pub(super) fn start(&self) -> usize {
        match self {
            Completion::Command(_) => 0,
            Completion::Mention { start, .. } => *start,
        }
    }

    pub(super) fn draw(&self, frame: &mut Frame, area: Rect, anchor: Position) {
        let (rows, selected): (Vec<(String, &str)>, _) = match self {
            Completion::Command(popup) => (Entry::rows(popup.items()), popup.selected_index()),
            Completion::Mention { popup, .. } => {
                (mention::rows(popup.items()), popup.selected_index())
            }
        };
        popup::draw(frame, area, anchor, &rows, selected);
    }
}

impl App {
    pub(super) fn refresh_completion(&mut self) {
        let text = self.prompt.text();
        self.completion = Entry::complete(text, &self.context.skills)
            .map(Completion::Command)
            .or_else(|| {
                let mention = mention::find(text, self.prompt.cursor())?;
                // A subagent starts no subagents, so `@name` means nothing
                // on its tab.
                let none = Agents::default();
                let agents = match self.showing_subagent() {
                    Some(_) => &none,
                    None => &self.context.agents,
                };
                let items = mention::items(agents, &self.files, mention.query);
                Some(Completion::Mention {
                    popup: Popup::new(items)?,
                    start: mention.start,
                })
            });
    }

    /// `submit` runs a highlighted command; an agent or a file is filled in
    /// either way, since sending a half-typed mention is never what Enter
    /// meant. A skill is filled in with room for its arguments, and runs
    /// once its name is typed out.
    pub(super) fn accept(&mut self, completion: Completion, submit: bool) {
        match completion {
            Completion::Command(popup) => match popup.selected().clone() {
                Entry::Builtin(command) if submit => {
                    self.prompt.clear();
                    self.run_command(command);
                }
                Entry::Builtin(command) => {
                    self.prompt.set(&format!("/{}", command.name()));
                    self.completion = Some(Completion::Command(popup));
                }
                Entry::Skill { name, .. } if submit && self.prompt.text() == format!("/{name}") => {
                    self.submit()
                }
                Entry::Skill { name, .. } => self.prompt.set(&format!("/{name} ")),
            },
            Completion::Mention { popup, start } => {
                let end = self.prompt.cursor();
                let spaced = self.prompt.text()[end..].starts_with(char::is_whitespace);
                let gap = if spaced { "" } else { " " };
                self.prompt
                    .replace(start..end, &format!("@{}{gap}", popup.selected().name()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{
        keys::Action,
        tests::{app, rows},
    };

    #[test]
    fn completion_pops_over_the_content_panel() {
        let mut app = app();
        app.apply(Action::Insert('/'));
        let rows = rows(&mut app);

        assert!(rows[4].starts_with("  /clear "), "{rows:#?}");
        assert!(rows[5].starts_with("  /close "));
        assert!(rows[6].starts_with("  /diagnostics "));
        assert!(rows[7].starts_with("  /exit "));
        assert!(rows[8].starts_with("  /models "));
        assert!(
            rows[9].starts_with("  /resume "),
            "right above the cursor, moved left to fit"
        );
        assert!(rows[10].starts_with(" ▎ /"));
    }

    #[test]
    fn at_lists_the_agents_first_and_fills_one_in() {
        let mut app = app();
        app.context = std::sync::Arc::new(nth_context::Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        ));
        app.files = vec!["src/explorer.rs".into()];
        for c in "ask @ex".chars() {
            app.apply(Action::Insert(c));
        }
        let rows = rows(&mut app);
        // Too wide for the terminal, the popup is shifted to the margin.
        assert!(rows[8].contains("@explore  Fast agent"), "{}", rows[8]);
        assert!(rows[9].contains("src/explorer.rs"), "{}", rows[9]);

        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "ask @explore ");
    }

    #[test]
    fn file_popup_opens_mid_prompt() {
        let mut app = app();
        app.files = vec!["src/app/keys.rs".into(), "src/lib.rs".into()];
        for c in "see @ke".chars() {
            app.apply(Action::Insert(c));
        }
        let rows = rows(&mut app);

        assert!(rows[8].trim().is_empty());
        assert!(
            rows[9].starts_with(" ▎ act  src/app/keys.rs "),
            "lined up with the @"
        );
        assert!(rows[10].starts_with(" ▎ see @ke"));
    }

    #[test]
    fn popup_follows_the_cursor_down_and_shifts_left_at_the_edge() {
        let mut app = app();
        app.files = vec!["src/app/keys.rs".into()];
        app.apply(Action::Newline);
        app.apply(Action::Newline);
        for c in format!("{} @ke", "x".repeat(30)).chars() {
            app.apply(Action::Insert(c));
        }
        let rows = rows(&mut app);

        let popup = format!(" ▎{}src/app/keys.rs  ", " ".repeat(21));
        assert_eq!(rows[11], popup, "above the third row, against the margin");
        assert!(rows[12].starts_with(" ▎ xxx"));
    }
}
