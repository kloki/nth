//! The input panel between the content panel and the status bar: the
//! prompt, or a widget that swaps in for it and hands back to the prompt
//! when done.

use nth_protocol::{Ask, Reply};
use ratatui::layout::Rect;

use super::App;
use crate::{
    llm_picker::LlmPicker,
    prompt,
    question::{self, QuestionPanel},
    session_picker::SessionPicker,
};

#[derive(Debug)]
pub(super) enum Input {
    Prompt,
    LlmPicker(LlmPicker),
    SessionPicker(SessionPicker),
    Question(QuestionPanel),
}

impl Input {
    /// Fixed while the input is open, so typing or filtering never moves
    /// the layout; the content panel takes up the difference when inputs
    /// swap. The question panel sizes itself to its questions, within
    /// `screen`.
    pub(super) fn rows(&self, screen: Rect) -> u16 {
        match self {
            Input::Prompt => prompt::ROWS,
            Input::LlmPicker(_) | Input::SessionPicker(_) => 8,
            Input::Question(panel) => question::rows(panel, screen),
        }
    }
}

impl App {
    /// Shows a tool's questions, or queues them behind the ones showing:
    /// calls run in parallel, so several can ask at once.
    pub(super) fn on_ask(&mut self, ask: Ask) {
        if matches!(self.input, Input::Question(_)) {
            self.asks.push_back(ask);
            return;
        }
        // The prompt keeps its text underneath, as with any panel.
        self.completion = None;
        self.input = Input::Question(QuestionPanel::new(ask));
    }

    /// Answers the questions showing, then shows the next ones waiting.
    pub(super) fn reply(&mut self, reply: Reply) {
        if let Input::Question(panel) = std::mem::replace(&mut self.input, Input::Prompt) {
            panel.reply(reply);
        }
        if let Some(ask) = self.asks.pop_front() {
            self.on_ask(ask);
        }
    }

    /// Closes the question panel when its turn ends: the tools that asked
    /// are gone, so there is nobody left to answer.
    pub(super) fn drop_asks(&mut self) {
        self.asks.clear();
        while self.asks_rx.try_recv().is_ok() {}
        if matches!(self.input, Input::Question(_)) {
            self.input = Input::Prompt;
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Answer, Question};
    use ratatui::style::Color;

    use super::*;
    use crate::{
        app::{
            keys::Action,
            tests::{app, rows, tab_colour},
        },
        question::tests::question,
    };

    /// An ask for `questions`, and where its reply lands.
    fn ask(questions: Vec<Question>) -> (Ask, tokio::sync::oneshot::Receiver<Reply>) {
        let (reply, replied) = tokio::sync::oneshot::channel();
        let ask = Ask {
            call_id: "1".into(),
            questions,
            reply,
        };
        (ask, replied)
    }

    #[test]
    fn a_question_turns_the_chat_tab_magenta() {
        let mut app = app();
        assert_eq!(tab_colour(&mut app, "› chat"), Color::Reset);
        let (ask, _replied) = ask(vec![question("auth", false, &["oauth", "key"])]);
        app.on_ask(ask);
        assert_eq!(tab_colour(&mut app, "› chat"), Color::Magenta);
    }

    #[test]
    fn a_question_takes_the_prompts_place_and_answers_the_tool() {
        let mut app = app();
        app.prompt.insert_str("half typed");
        let (ask, mut replied) = ask(vec![question("auth", false, &["oauth", "key"])]);
        app.on_ask(ask);
        let asking = rows(&mut app);

        assert_eq!(
            asking[14].trim_end(),
            " glm · /repo",
            "the status bar stays"
        );
        let panel: Vec<&str> = asking[8..13].iter().map(|r| r.trim_end()).collect();
        assert_eq!(
            panel,
            [
                " ▎ question      ↑↓ · 1-2 · enter · esc",
                " ▎ Which auth?",
                " ▎ → 1. oauth",
                " ▎   2. key",
                " ▎   3. Type your own answer…",
            ]
        );

        app.apply(Action::SelectNext);
        app.apply(Action::Submit);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.prompt.text(), "half typed", "the prompt kept its text");
        assert_eq!(
            replied.try_recv(),
            Ok(Reply::Answered(vec![Answer {
                picked: vec!["key".into()],
                typed: None,
            }]))
        );
    }

    #[test]
    fn esc_declines_and_the_next_question_follows() {
        let mut app = app();
        let (first, mut declined) = ask(vec![question("auth", false, &["a", "b"])]);
        let (second, _) = ask(vec![question("checks", true, &["a", "b"])]);
        app.on_ask(first);
        app.on_ask(second);

        app.apply(Action::Interrupt);

        assert_eq!(declined.try_recv(), Ok(Reply::Declined));
        match &app.input {
            Input::Question(panel) => assert_eq!(panel.questions()[0].header, "checks"),
            _ => panic!("the queued question shows"),
        }

        app.drop_asks();
        assert!(matches!(app.input, Input::Prompt), "gone with the turn");
    }
}
