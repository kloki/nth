//! The input panel between the content panel and the status bar: the
//! prompt, or a widget that swaps in for it and hands back to the prompt
//! when done.

use nth_protocol::{Ask, BoxError, ModelInfo, Reply};
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

    /// Opens the picker on the LLM in use, listing LLMs the first time.
    pub(super) fn open_llm_picker(&mut self) {
        self.completion = None;
        let mut picker = LlmPicker::new(&self.model, self.effort);
        match &self.llms {
            Some(llms) => picker.load(Ok(llms.clone())),
            // The answer fills this picker when it comes.
            None => self.list_llms(),
        }
        self.input = Input::LlmPicker(picker);
    }

    /// Asks for the LLMs unless they are listed or already asked for. Done
    /// at start-up too, since the status bar needs the context window.
    pub(super) fn list_llms(&mut self) {
        if self.llms.is_none() && !self.llm_listing.is_running() {
            let provider = self.provider.clone();
            self.llm_listing
                .start(|_| tokio::spawn(async move { provider.models().await }));
        }
    }

    /// The context window of the model in use, when the provider says.
    pub fn context_window(&self) -> Option<u64> {
        let llms = self.llms.as_ref()?;
        llms.iter().find(|llm| llm.id == self.model)?.context
    }

    /// Only a list is kept; after a failure the next open asks again.
    pub(super) fn llms_listed(&mut self, llms: Result<Vec<ModelInfo>, BoxError>) {
        let llms = llms.map_err(|e| e.to_string());
        if let Ok(llms) = &llms {
            self.llms = Some(llms.clone());
        }
        if let Input::LlmPicker(picker) = &mut self.input {
            picker.load(llms);
        }
    }

    /// Switches later turns to the highlighted model; the session picks it
    /// up when the next turn starts, since mid-turn it is in the turn task.
    pub(super) fn choose_llm(&mut self) {
        let Input::LlmPicker(picker) = &self.input else {
            return;
        };
        if let Some((model, effort)) = picker.chosen() {
            self.model = model;
            self.effort = effort;
            self.input = Input::Prompt;
        }
    }
}
