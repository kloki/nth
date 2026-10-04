//! Prompts sent before, recalled with Up and Down, saved in the background
//! after each one.

use std::path::Path;

use super::App;

impl App {
    /// Writes the whole history in the background; recalling never waits
    /// on it.
    pub(super) fn save_history(&mut self) {
        let Some(path) = self.history.saved_at().map(Path::to_path_buf) else {
            return;
        };
        let jsonl = self.history.to_jsonl();
        self.history_saving.start_or_queue(|_| {
            tokio::spawn(async move {
                if let Some(dir) = path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::write(&path, jsonl).await
            })
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
