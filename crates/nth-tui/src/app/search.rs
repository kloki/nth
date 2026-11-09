//! Binding search to its source view and applying jumps.

use ratatui::layout::Rect;

use super::{App, Tab, input::Input, keys::Action};
use crate::{
    fuzzy::Filter,
    search::{Row, Search},
};

/// A result Enter chose, waiting for the draw that shows its source at the
/// shape the search picker was hiding.
pub(super) struct Jump {
    source: Tab,
    row: Row,
    /// Which match the row was, and the query that found it, should the
    /// row itself not survive a rewrap.
    selected: usize,
    query: String,
}

impl App {
    pub(super) fn open_search(&mut self) {
        let source = self.content.active();
        if !matches!(source, Tab::Chat | Tab::Plan) {
            self.hint = Some("search is available in chat and plan".into());
            return;
        }
        if source == Tab::Plan && !self.plan.view.exists() {
            self.hint = Some("there is no plan to search".into());
            return;
        }
        let width = self
            .areas
            .panes
            .iter()
            .find(|(tab, _)| *tab == source)
            .map_or(1, |(_, area)| area.width);
        self.completion = None;
        self.input = Input::Search(Search::new(source, width));
        self.update_search(width);
    }

    /// Points the source at the row Enter chose, once the draw knows the
    /// width the source has with the prompt back in place.
    pub(super) fn apply_search_jump(&mut self, panes: &[(Tab, Rect)]) {
        let Some(jump) = self.search_jump.take() else {
            return;
        };
        if !self.source_open(jump.source) {
            return;
        }
        let Some(width) = panes
            .iter()
            .find(|(tab, _)| *tab == jump.source)
            .map(|(_, area)| area.width)
        else {
            // Not in view yet; the next draw that shows it lands the jump.
            self.search_jump = Some(jump);
            return;
        };
        let Some(index) = self
            .source_rows(jump.source, width)
            .and_then(|rows| locate(&rows, &jump.row, &jump.query, jump.selected))
        else {
            return;
        };
        match jump.source {
            Tab::Chat => self.chat.pager.jump_row(index),
            Tab::Plan => self.plan.view.pager.jump_row(index),
            _ => {}
        }
    }

    pub(super) fn refresh_search(&mut self, panes: &[(Tab, Rect)]) {
        let Input::Search(search) = &self.input else {
            return;
        };
        let source = search.source;
        if !self.source_open(source) {
            self.input = Input::Prompt;
            return;
        }
        // A hidden source has no width worth wrapping its rows for; Enter
        // resolves the chosen row again once the source is shown.
        let Some(width) = panes
            .iter()
            .find(|(tab, _)| *tab == source)
            .map(|(_, area)| area.width)
        else {
            return;
        };
        self.update_search(width);
    }

    /// Whether the source is still there to search: its tab is open, and a
    /// plan still exists.
    fn source_open(&self, source: Tab) -> bool {
        self.content.tabs().contains(&source) && (source != Tab::Plan || self.plan.view.exists())
    }

    fn update_search(&mut self, width: u16) {
        let Input::Search(search) = &self.input else {
            return;
        };
        let source = search.source;
        let revision = match source {
            Tab::Chat => self.chat.transcript.search_revision(),
            Tab::Plan => self.plan.view.search_revision(),
            _ => return,
        };
        if search.revision == Some(revision) && search.width == width {
            return;
        }
        let Some(rows) = self.source_rows(source, width) else {
            return;
        };
        // Laying the source out may have changed its revision.
        let revision = match source {
            Tab::Chat => self.chat.transcript.search_revision(),
            Tab::Plan => self.plan.view.search_revision(),
            _ => return,
        };
        if let Input::Search(search) = &mut self.input {
            search.update(rows, width);
            search.revision = Some(revision);
        }
    }

    /// The searchable rows of `source` as it renders `width` columns wide.
    fn source_rows(&mut self, source: Tab, width: u16) -> Option<Vec<Row>> {
        match source {
            Tab::Chat => {
                self.chat.transcript.layout(width, self.settings);
                Some(self.chat.transcript.search_rows())
            }
            Tab::Plan => {
                let path = self.plan_place();
                Some(self.plan.view.search_rows(usize::from(width), &path))
            }
            _ => None,
        }
    }

    pub(super) fn key_search(&mut self, action: Action) {
        if action == Action::Submit {
            let Input::Search(search) = &self.input else {
                return;
            };
            let source = search.source;
            let Some(row) = search.chosen().cloned() else {
                return;
            };
            let (selected, query) = (search.selected(), search.query().to_string());
            self.input = Input::Prompt;
            self.content.select_tab(source);
            if self.source_open(source) {
                self.search_jump = Some(Jump {
                    source,
                    row,
                    selected,
                    query,
                });
            }
            return;
        }
        let Input::Search(search) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext => search.next(),
            Action::SelectPrev => search.prev(),
            Action::Insert(c) => search.insert(c),
            Action::Backspace => search.backspace(),
            Action::ClearOrQuit if search.clear() => {}
            Action::Interrupt | Action::ClearOrQuit | Action::Search => self.input = Input::Prompt,
            _ => {}
        }
    }
}

/// Where `row` reads at the source's new width: the same text, nearest
/// where it used to be; a rewrap can split it otherwise, so the query's
/// match in the same place among the results, or the same ordinal in the
/// source.
fn locate(rows: &[Row], row: &Row, query: &str, selected: usize) -> Option<usize> {
    if let Some(index) = rows
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.text == row.text)
        .min_by_key(|(_, candidate)| candidate.index.abs_diff(row.index))
        .map(|(_, candidate)| candidate.index)
    {
        return Some(index);
    }
    let mut filter = Filter::default();
    for c in query.chars() {
        filter.push(c);
    }
    let ranked = filter.rank(rows.iter().map(|r| r.text.as_str()));
    ranked
        .get(selected)
        .or_else(|| ranked.first())
        .and_then(|&i| rows.get(i))
        .map(|row| row.index)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent};
    use nth_session::Session;

    use super::*;
    use crate::app::tests::{Scripted, app, rows, rows_of};

    #[test]
    fn chat_search_jumps_and_preserves_draft_and_focus() {
        let mut app = app().with_split_width(60);
        for i in 0..40 {
            app.chat.transcript.push_user(format!(
                "message {i} {}",
                if i == 5 { "needle" } else { "ordinary" }
            ));
        }
        app.prompt.insert_str("draft");
        rows_of(&mut app, 80);
        app.apply(Action::Search);
        for c in "needle".chars() {
            app.apply(Action::Insert(c));
        }
        rows_of(&mut app, 80);
        app.content.open(Tab::Diagnostics);
        app.apply(Action::Submit);
        assert_eq!(app.content.active(), Tab::Chat);
        let drawn = rows_of(&mut app, 80);
        assert!(
            drawn[2..5].iter().any(|r| r.contains("needle")),
            "the match sits at the top: {drawn:#?}"
        );
        assert!(!app.chat.pager.is_following());
        assert!(app.chat.scrollbar().is_some());
        assert!(drawn.iter().any(|r| r.contains("draft")));
    }

    #[test]
    fn plan_search_includes_removed_lines_and_closes_when_gone() {
        let mut app = app();
        app.plan.view.settle(Some(format!(
            "{}removed needle\n{}",
            "before\n".repeat(20),
            "after\n".repeat(20)
        )));
        app.plan.view.update(Some(format!(
            "{}new\n{}",
            "before\n".repeat(20),
            "after\n".repeat(20)
        )));
        app.content.open(Tab::Plan);
        rows(&mut app);
        app.apply(Action::Search);
        for c in "removed needle".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Submit);
        let drawn = rows(&mut app);
        assert!(
            drawn[2..5].iter().any(|r| r.contains("removed needle")),
            "the removed match sits at the top: {drawn:#?}"
        );
        app.apply(Action::Search);
        app.plan.view.settle(None);
        rows(&mut app);
        assert!(matches!(app.input, Input::Prompt));
    }

    #[tokio::test]
    async fn escape_in_search_leaves_a_running_turn_alone() {
        let hang = Arc::new(Scripted::hang());
        let session = Session::new("glm", "/repo".into());
        let mut app = App::bare(session, hang.clone(), Arc::new(Vec::new()));
        app.chat.transcript.push_user("needle".into());
        app.prompt.insert_str("go");
        app.apply(Action::Submit);
        hang.asked.notified().await;

        rows(&mut app);
        app.apply(Action::Search);
        app.apply(Action::Interrupt);

        assert!(matches!(app.input, Input::Prompt));
        assert!(app.is_busy(), "the turn still runs");
        assert!(!app.turns.interrupted(), "Esc closed search, not the turn");

        app.on_key(KeyEvent::from(KeyCode::Esc));
        let ended = app.turn.join().await.expect("the turn ends");
        app.end_turn(ended);
    }

    #[test]
    fn empty_search_and_escape_keep_the_prompt() {
        let mut app = app();
        app.prompt.insert_str("draft");
        rows(&mut app);
        app.apply(Action::Search);
        app.apply(Action::Submit);
        assert!(matches!(app.input, Input::Search(_)));
        app.apply(Action::Interrupt);
        assert!(matches!(app.input, Input::Prompt));
        assert!(rows(&mut app).iter().any(|r| r.contains("draft")));
    }

    #[test]
    fn a_plan_result_survives_the_source_gaining_the_full_width() {
        let mut app = app().with_split_width(60);
        let line = format!("needle {}", "x ".repeat(30));
        app.plan
            .view
            .settle(Some(format!("{}found it\n{}", "before\n".repeat(10), line)));
        app.content.open(Tab::Plan);
        rows_of(&mut app, 80);
        app.apply(Action::Search);
        for c in "needle".chars() {
            app.apply(Action::Insert(c));
        }
        rows_of(&mut app, 80);

        // Hide the plan and narrow the terminal, so the jump resolves at a
        // width the search never saw.
        app.content.open(Tab::Diagnostics);
        rows_of(&mut app, 59);
        app.apply(Action::Submit);

        let drawn = rows_of(&mut app, 59);
        assert_eq!(app.content.active(), Tab::Plan);
        assert!(
            drawn[2..8].iter().any(|r| r.contains("needle")),
            "the result sits at the top: {drawn:#?}"
        );
    }

    #[test]
    fn search_elsewhere_only_hints() {
        let mut app = app();
        app.chat.transcript.push_user("needle".into());
        app.content.open(Tab::Usage);
        rows(&mut app);
        app.apply(Action::Search);
        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(
            app.hint.as_ref().map(|hint| hint.to_string()),
            Some("search is available in chat and plan".into())
        );
    }
}
