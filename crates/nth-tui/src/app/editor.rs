//! ctrl+g: you edit a copy of the plan in your editor, and the model gets
//! your edits as review feedback to work into the plan itself. The app
//! keeps running while the editor has the terminal, so a turn or a
//! monitor never stalls on a full channel; it only stops drawing.

use std::{io, path::PathBuf, process::ExitStatus};

use crossterm::event::EventStream;
use nth_session::plan::edits;
use ratatui::DefaultTerminal;
use tokio::task::JoinError;

use super::App;
use crate::terminal;

/// The copy being edited, and the plan it was copied from.
#[derive(Debug)]
pub(super) struct Editing {
    copy: PathBuf,
    original: String,
}

impl App {
    /// Asks the loop to open the editor; it owns the terminal and the key
    /// stream the editor needs.
    pub(super) fn edit_plan(&mut self) {
        if !self.plan.exists() {
            self.hint = Some("no plan yet".into());
            return;
        }
        if self.editing.is_none() {
            self.pending_editor = true;
        }
    }

    pub(super) fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Copies the plan, hands the terminal to the editor and waits for it
    /// in the background. `input` goes, so its reader thread stops taking
    /// the keys meant for the editor.
    pub(super) async fn open_editor(&mut self, input: &mut Option<EventStream>) {
        self.pending_editor = false;
        let Some(original) = self.plan.text().map(str::to_string) else {
            return;
        };
        let name = self.plan_path.file_name().unwrap_or_default();
        let copy = std::env::temp_dir().join("nth").join(name);
        if let Err(e) = write_copy(&copy, &original).await {
            self.hint = Some(format!("cannot copy the plan: {e}"));
            return;
        }
        *input = None;
        terminal::suspend();
        let editor = editor();
        let path = copy.clone();
        self.editor
            .start(|_| tokio::spawn(async move { run_editor(&editor, &path).await }));
        self.editing = Some(Editing { copy, original });
    }

    /// Takes the terminal back however the editor ended, then sends the
    /// edits if there are any. A failed editor is a hint, never an exit.
    pub(super) async fn editor_closed(
        &mut self,
        terminal: &mut DefaultTerminal,
        input: &mut Option<EventStream>,
        ended: Result<io::Result<ExitStatus>, JoinError>,
    ) -> anyhow::Result<()> {
        terminal::resume(terminal)?;
        *input = Some(EventStream::new());
        let Some(Editing { copy, original }) = self.editing.take() else {
            return Ok(());
        };
        let edited = match ended {
            Ok(Ok(status)) if status.success() => tokio::fs::read_to_string(&copy)
                .await
                .map_err(|e| format!("cannot read the edited plan: {e}")),
            // :cq in vim, say: you meant to throw the edits away.
            Ok(Ok(status)) => Err(format!("editor exited with {status} · edits dropped")),
            Ok(Err(e)) => Err(format!("editor failed: {e}")),
            Err(e) => Err(format!("editor failed: {e}")),
        };
        // The copy has done its job either way; a leftover in the temp
        // folder is harmless.
        let _ = tokio::fs::remove_file(&copy).await;
        self.edited(&original, edited);
        Ok(())
    }

    /// Sends `edited` as your edits of `original`, queued behind a running
    /// turn like any prompt, or says why there is nothing to send.
    fn edited(&mut self, original: &str, edited: Result<String, String>) {
        let edited = match edited {
            Ok(edited) if edited == original => {
                self.hint = Some("no changes to the plan".into());
                return;
            }
            Ok(edited) => edited,
            Err(why) => {
                self.hint = Some(why);
                return;
            }
        };
        let text = edits::render(&self.plan_path, original, &edited);
        self.hold_notices = false;
        if self.is_busy() {
            self.queue.push_back(text);
        } else {
            self.start_turn(text);
        }
    }
}

async fn write_copy(copy: &std::path::Path, text: &str) -> io::Result<()> {
    if let Some(dir) = copy.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(copy, text).await
}

/// `$VISUAL`, else `$EDITOR`, else vi, as most tools pick one.
fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|editor| !editor.trim().is_empty())
        .unwrap_or_else(|| "vi".into())
}

/// Runs `editor` on `path` through the shell, as git does, so an editor
/// with arguments such as `code --wait` works.
async fn run_editor(editor: &str, path: &std::path::Path) -> io::Result<ExitStatus> {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("nth")
        .arg(path)
        .status()
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_protocol::Message;
    use nth_session::Session;

    use super::*;
    use crate::{
        app::{keys::Action, tests::Idle},
        chat::Entry,
    };

    #[tokio::test]
    async fn an_editor_with_arguments_edits_the_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plan.md");
        std::fs::write(&path, "# Plan\n").expect("writes");

        let status = run_editor("echo note >>", &path).await.expect("runs");
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            "# Plan\nnote\n"
        );

        let status = run_editor("false", &path).await.expect("runs");
        assert!(!status.success());
    }

    #[test]
    fn ctrl_g_without_a_plan_only_hints() {
        let mut app = crate::app::tests::app();
        app.apply(Action::EditPlan);
        assert_eq!(app.hint.as_deref(), Some("no plan yet"));
        assert!(!app.pending_editor);
    }

    #[tokio::test]
    async fn edits_go_to_the_model_and_show_as_one_row() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Idle), Arc::new(Vec::new()));

        app.edited("# Plan\n", Ok("# Plan\n".into()));
        assert_eq!(app.hint.as_deref(), Some("no changes to the plan"));
        app.edited("# Plan\n", Err("editor failed: gone".into()));
        assert_eq!(app.hint.as_deref(), Some("editor failed: gone"));
        assert!(!app.is_busy());

        app.edited("# Plan\n", Ok("# Plan\n<!-- why? -->\n".into()));
        assert!(app.is_busy());
        app.interrupt();
        let ended = app.turn.join().await.expect("ends");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("came back");
        let Some(Message::User(sent)) = session.messages.last() else {
            panic!("sent");
        };
        assert!(
            sent.starts_with("<plan-edits path=\"/repo/.nth/plans/"),
            "{sent}"
        );
        assert!(sent.contains("+<!-- why? -->"));
        assert_eq!(
            app.chat.transcript.entries().next(),
            Some(&Entry::PlanEdits(edits::PlanEdits {
                added: 1,
                removed: 0
            }))
        );
    }
}
