//! The rows under the status line hold the prompt, or a widget that swaps
//! in over it and hands back to the prompt when done.

use nth_protocol::{BoxError, ModelInfo};

use super::App;
use crate::models::Picker;

/// Every panel is this tall, so swapping one in moves nothing.
pub(super) const PANEL_ROWS: u16 = 8;

#[derive(Debug)]
pub(super) enum Panel {
    Prompt,
    Models(Picker),
}

impl App {
    /// Opens the picker on the model in use, listing models the first time.
    pub(super) fn open_models(&mut self) {
        self.completion = None;
        let mut picker = Picker::new(&self.model, self.effort);
        match &self.models {
            Some(models) => picker.load(Ok(models.clone())),
            None if self.listing.is_none() => {
                let provider = self.provider.clone();
                self.listing = Some(tokio::spawn(async move { provider.models().await }));
            }
            // Already asked; the answer fills this picker when it comes.
            None => {}
        }
        self.panel = Panel::Models(picker);
    }

    /// Only a list is kept; after a failure the next open asks again.
    pub(super) fn listed(&mut self, models: Result<Vec<ModelInfo>, BoxError>) {
        self.listing = None;
        let models = models.map_err(|e| e.to_string());
        if let Ok(models) = &models {
            self.models = Some(models.clone());
        }
        if let Panel::Models(picker) = &mut self.panel {
            picker.load(models);
        }
    }

    /// Switches later turns to the highlighted model; the session picks it
    /// up when the next turn starts, since mid-turn it is in the turn task.
    pub(super) fn choose_model(&mut self) {
        let Panel::Models(picker) = &self.panel else {
            return;
        };
        if let Some((model, effort)) = picker.chosen() {
            self.model = model;
            self.effort = effort;
            self.panel = Panel::Prompt;
        }
    }
}
