//! The question panel: the model's questions in the prompt's place, one
//! tab per question and a Submit tab to review them when there are several.
//! Every question ends with an open field you type your own answer into.

mod view;

use nth_protocol::{Answer, Ask, Question, Reply};
pub use view::{draw, rows};

use crate::prompt::Prompt;

#[derive(Debug)]
pub struct QuestionPanel {
    ask: Ask,
    panes: Vec<Pane>,
    /// The question shown; `panes.len()` is the Submit tab.
    tab: usize,
}

/// What you have done with one question.
#[derive(Debug, Default)]
struct Pane {
    /// The highlighted row; one past the options is the open field.
    highlight: usize,
    /// Ticked options. A one-choice question has at most one.
    checked: Vec<bool>,
    open: Prompt,
    /// A one-choice question answered with the open field; any-number
    /// questions count it whenever it has text.
    chose_open: bool,
}

impl QuestionPanel {
    pub fn new(ask: Ask) -> Self {
        let panes = ask
            .questions
            .iter()
            .map(|q| Pane {
                checked: vec![false; q.options.len()],
                ..Pane::default()
            })
            .collect();
        Self { ask, panes, tab: 0 }
    }

    pub fn questions(&self) -> &[Question] {
        &self.ask.questions
    }

    /// Sends `reply` to the tool that asked; a turn cancelled meanwhile no
    /// longer listens, which is fine.
    pub fn reply(self, reply: Reply) {
        let _ = self.ask.reply.send(reply);
    }

    pub fn on_submit(&self) -> bool {
        self.tab == self.panes.len()
    }

    /// Only several questions get tabs and a Submit tab to review them.
    pub fn has_tabs(&self) -> bool {
        self.panes.len() > 1
    }

    pub fn next_tab(&mut self) {
        if self.has_tabs() {
            self.tab = (self.tab + 1) % (self.panes.len() + 1);
        }
    }

    pub fn prev_tab(&mut self) {
        if self.has_tabs() {
            self.tab = (self.tab + self.panes.len()) % (self.panes.len() + 1);
        }
    }

    pub fn next(&mut self) {
        if let Some((pane, question)) = self.current_mut() {
            pane.highlight = (pane.highlight + 1) % (question.options.len() + 1);
        }
    }

    pub fn prev(&mut self) {
        if let Some((pane, question)) = self.current_mut() {
            let rows = question.options.len() + 1;
            pane.highlight = (pane.highlight + rows - 1) % rows;
        }
    }

    /// Whether ←→ and Home/End edit the open field rather than move tabs.
    pub fn editing(&self) -> bool {
        self.current()
            .is_some_and(|(pane, question)| pane.highlight == question.options.len())
    }

    /// The open field, when it is highlighted.
    pub fn open_mut(&mut self) -> Option<&mut Prompt> {
        let (pane, question) = self.current_mut()?;
        (pane.highlight == question.options.len()).then_some(&mut pane.open)
    }

    /// A typed key: text for the open field, or a number or space that
    /// picks an option. Any other key jumps to the open field and starts
    /// your own answer. Returns the answers when that settles them.
    pub fn insert(&mut self, c: char) -> Option<Vec<Answer>> {
        let (pane, question) = self.current_mut()?;
        let options = question.options.len();
        let multiple = question.multiple;
        if pane.highlight == options {
            pane.open.insert(c);
            return None;
        }
        match c.to_digit(10).map(|d| d as usize) {
            Some(n @ 1..) if n <= options + 1 => {
                pane.highlight = n - 1;
                if n == options + 1 {
                    None
                } else if multiple {
                    pane.toggle();
                    None
                } else {
                    self.enter()
                }
            }
            _ if c == ' ' && multiple => {
                pane.toggle();
                None
            }
            _ => {
                pane.highlight = options;
                pane.open.insert(c);
                None
            }
        }
    }

    /// Enter: answers the question shown and moves on, or on the Submit tab
    /// sends every answer. Returns them once they are all in.
    pub fn enter(&mut self) -> Option<Vec<Answer>> {
        if self.on_submit() {
            return self.answers();
        }
        let tab = self.tab;
        let question = &self.ask.questions[tab];
        let pane = &mut self.panes[tab];
        let on_open = pane.highlight == question.options.len();
        if question.multiple {
            // The highlighted option counts when nothing else is ticked, so
            // Enter alone picks it as in a one-choice question.
            if !on_open && pane.answer(question).is_none() {
                pane.toggle();
            }
        } else if on_open {
            if pane.open.text().trim().is_empty() {
                return None;
            }
            pane.checked.fill(false);
            pane.chose_open = true;
        } else {
            pane.checked.fill(false);
            pane.checked[pane.highlight] = true;
            pane.chose_open = false;
        }
        pane.answer(question)?;
        if !self.has_tabs() {
            return self.answers();
        }
        // On to the next unanswered question, or to Submit.
        self.tab = (tab + 1..self.panes.len())
            .find(|&i| self.answer(i).is_none())
            .unwrap_or(self.panes.len());
        None
    }

    /// Every answer, once each question has one.
    pub fn answers(&self) -> Option<Vec<Answer>> {
        (0..self.panes.len()).map(|i| self.answer(i)).collect()
    }

    pub fn answer(&self, i: usize) -> Option<Answer> {
        self.panes[i].answer(&self.ask.questions[i])
    }

    fn current(&self) -> Option<(&Pane, &Question)> {
        Some((self.panes.get(self.tab)?, &self.ask.questions[self.tab]))
    }

    fn current_mut(&mut self) -> Option<(&mut Pane, &Question)> {
        Some((self.panes.get_mut(self.tab)?, &self.ask.questions[self.tab]))
    }
}

impl Pane {
    fn toggle(&mut self) {
        if let Some(checked) = self.checked.get_mut(self.highlight) {
            *checked = !*checked;
        }
    }

    fn answer(&self, question: &Question) -> Option<Answer> {
        let picked: Vec<String> = question
            .options
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(option, _)| option.label.clone())
            .collect();
        let text = self.open.text().trim();
        let typed =
            (!text.is_empty() && (question.multiple || self.chose_open)).then(|| text.to_string());
        (!picked.is_empty() || typed.is_some()).then_some(Answer { picked, typed })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use nth_protocol::QuestionOption;
    use tokio::sync::oneshot;

    use super::*;

    pub(crate) fn question(header: &str, multiple: bool, labels: &[&str]) -> Question {
        Question {
            question: format!("Which {header}?"),
            header: header.into(),
            multiple,
            options: labels
                .iter()
                .map(|label| QuestionOption {
                    label: (*label).into(),
                    description: None,
                    preview: None,
                })
                .collect(),
        }
    }

    pub(crate) fn panel(questions: Vec<Question>) -> (QuestionPanel, oneshot::Receiver<Reply>) {
        let (reply, replied) = oneshot::channel();
        let ask = Ask {
            call_id: "1".into(),
            questions,
            reply,
        };
        (QuestionPanel::new(ask), replied)
    }

    fn picked(labels: &[&str]) -> Answer {
        Answer {
            picked: labels.iter().map(|l| (*l).into()).collect(),
            typed: None,
        }
    }

    #[test]
    fn a_lone_question_is_answered_by_enter() {
        let (mut p, _) = panel(vec![question("auth", false, &["oauth", "key"])]);
        p.next();

        assert_eq!(p.enter(), Some(vec![picked(&["key"])]));
    }

    #[test]
    fn a_number_picks_its_option() {
        let (mut p, _) = panel(vec![question("auth", false, &["oauth", "key"])]);

        assert_eq!(p.insert('2'), Some(vec![picked(&["key"])]));
    }

    #[test]
    fn the_open_field_answers_with_what_you_typed() {
        let (mut p, _) = panel(vec![question("auth", false, &["oauth", "key"])]);
        assert_eq!(p.enter_open_empty(), None, "empty does not answer");

        for c in "mtls".chars() {
            p.insert(c);
        }

        assert_eq!(
            p.enter(),
            Some(vec![Answer {
                picked: Vec::new(),
                typed: Some("mtls".into()),
            }])
        );
    }

    #[test]
    fn typing_on_an_option_starts_your_own_answer() {
        let (mut p, _) = panel(vec![question("auth", false, &["oauth", "key"])]);
        p.insert('x');

        assert!(p.editing());
        assert_eq!(p.open_mut().map(|o| o.text().to_string()), Some("x".into()));
    }

    #[test]
    fn any_number_ticks_options_and_the_open_field() {
        let (mut p, _) = panel(vec![question("checks", true, &["fmt", "clippy", "test"])]);
        p.insert(' ');
        p.insert('3');
        p.insert('3');
        p.insert('2');
        p.insert('4');
        for c in "docs".chars() {
            p.insert(c);
        }

        assert_eq!(
            p.enter(),
            Some(vec![Answer {
                picked: vec!["fmt".into(), "clippy".into()],
                typed: Some("docs".into()),
            }])
        );
    }

    #[test]
    fn several_questions_end_on_submit() {
        let (mut p, _) = panel(vec![
            question("auth", false, &["oauth", "key"]),
            question("checks", true, &["fmt", "clippy"]),
        ]);
        assert_eq!(p.enter(), None);
        assert_eq!(p.tab, 1, "on to the next question");

        p.prev_tab();
        p.next_tab();
        p.next_tab();
        assert!(p.on_submit());
        assert_eq!(p.enter(), None, "checks is unanswered");

        p.prev_tab();
        assert_eq!(p.insert(' '), None);
        assert_eq!(p.enter(), None);
        assert!(p.on_submit());
        assert_eq!(p.enter(), Some(vec![picked(&["oauth"]), picked(&["fmt"])]));
    }

    impl QuestionPanel {
        /// Enter on the open field while it is empty.
        fn enter_open_empty(&mut self) -> Option<Vec<Answer>> {
            self.prev();
            self.enter()
        }
    }
}
