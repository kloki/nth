//! The input panel between the chat and the status bar: the prompt, or a
//! widget that swaps in for it and hands back to the prompt when done.

use nth_protocol::{BoxError, ModelInfo};

use super::App;
use crate::llm_picker::LlmPicker;

#[derive(Debug)]
pub(super) enum Panel {
    Prompt,
    LlmPicker(LlmPicker),
}

impl Panel {
    /// Fixed while the panel is open, so typing or filtering never moves
    /// the layout; the chat takes up the difference when panels swap.
    pub(super) fn rows(&self) -> u16 {
        match self {
            Panel::Prompt => 3,
            Panel::LlmPicker(_) => 8,
        }
    }
}

impl App {
    /// Opens the picker on the LLM in use, listing LLMs the first time.
    pub(super) fn open_llm_picker(&mut self) {
        self.completion = None;
        let mut picker = LlmPicker::new(&self.model, self.effort);
        match &self.llms {
            Some(llms) => picker.load(Ok(llms.clone())),
            // The answer fills this picker when it comes.
            None => self.list_llms(),
        }
        self.panel = Panel::LlmPicker(picker);
    }

    /// Asks for the LLMs unless they are listed or already asked for. Done
    /// at start-up too, since the status bar needs the context window.
    pub(super) fn list_llms(&mut self) {
        if self.llms.is_none() && self.llm_listing.is_none() {
            let provider = self.provider.clone();
            self.llm_listing = Some(tokio::spawn(async move { provider.models().await }));
        }
    }

    /// The context window of the model in use, when the provider says.
    pub fn context_window(&self) -> Option<u64> {
        let llms = self.llms.as_ref()?;
        llms.iter().find(|llm| llm.id == self.model)?.context
    }

    /// Only a list is kept; after a failure the next open asks again.
    pub(super) fn llms_listed(&mut self, llms: Result<Vec<ModelInfo>, BoxError>) {
        self.llm_listing = None;
        let llms = llms.map_err(|e| e.to_string());
        if let Ok(llms) = &llms {
            self.llms = Some(llms.clone());
        }
        if let Panel::LlmPicker(picker) = &mut self.panel {
            picker.load(llms);
        }
    }

    /// Switches later turns to the highlighted model; the session picks it
    /// up when the next turn starts, since mid-turn it is in the turn task.
    pub(super) fn choose_llm(&mut self) {
        let Panel::LlmPicker(picker) = &self.panel else {
            return;
        };
        if let Some((model, effort)) = picker.chosen() {
            self.model = model;
            self.effort = effort;
            self.panel = Panel::Prompt;
        }
    }
}
