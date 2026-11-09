//! Prompts sent before, recalled with Up and Down, saved in the background
//! after each one.

use std::path::Path;

use super::App;
use crate::history;

impl App {
    /// Appends what was sent since the last save in the background;
    /// recalling never waits on it. A save asked for while one runs takes
    /// its prompts once that one ends.
    pub(super) fn save_history(&mut self) {
        let Some(path) = self.history.saved_at().map(Path::to_path_buf) else {
            return;
        };
        let history = &mut self.history;
        self.history_saving.start_or_queue(|_| {
            let jsonl = history.take_unsaved();
            tokio::task::spawn_blocking(move || history::append(&path, &jsonl))
        });
    }

    /// A failed save is told once; the history stays in memory for this run.
    pub(super) fn history_saved(&mut self, saved: std::io::Result<()>) {
        if self.history_saving.take_again() {
            self.save_history();
        }
        if let Err(e) = saved {
            let path = self.history.saved_at().map(|p| p.display().to_string());
            self.chat.transcript.push_error(format!(
                "prompt history not saved to {}: {e}",
                path.unwrap_or_default()
            ));
            self.history.forget_path();
        }
    }
}
