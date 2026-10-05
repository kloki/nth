//! ctrl+g: your editor on a copy of what the content panel shows. On the
//! plan tab the model gets your edits as review feedback to work into the
//! plan itself; anywhere else you edit the prompt, and the result becomes
//! its text again. The app keeps running while the editor has the
//! terminal, so a turn or a monitor never stalls on a full channel; it
//! only stops drawing.

use std::{io, path::PathBuf, process::ExitStatus};

use crossterm::event::EventStream;
use ratatui::DefaultTerminal;
use tokio::task::JoinError;

use super::{App, Queued, Tab};
use crate::terminal;

/// What the editor is open on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Target {
    /// The plan; the edits go to the model as review feedback.
    Plan,
    /// The prompt; the edits become the prompt's text again.
    Prompt,
}

/// The copy being edited, what it was copied from, and what to do with
/// the edits when the editor closes.
#[derive(Debug)]
pub(super) struct Editing {
    target: Target,
    copy: PathBuf,
    original: String,
}

impl App {
    /// ctrl+g: the plan while its tab shows, else the prompt. Asks the
    /// loop to open the editor; it owns the terminal and the key stream
    /// the editor needs.
    pub(super) fn edit(&mut self) {
        if self.editing.is_some() {
            return;
        }
        let target = if self.content.active() == Tab::Plan {
            if !self.plan.exists() {
                self.hint = Some("no plan yet".into());
                return;
            }
            Target::Plan
        } else {
            Target::Prompt
        };
        self.pending_editor = Some(target);
    }

    pub(super) fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Copies the target, hands the terminal to the editor and waits for
    /// it in the background. `input` goes, so its reader thread stops
    /// taking the keys meant for the editor.
    pub(super) async fn open_editor(&mut self, input: &mut Option<EventStream>) {
        let Some(target) = self.pending_editor.take() else {
            return;
        };
        let original = match target {
            Target::Plan => match self.plan.text() {
                Some(text) => text.to_string(),
                None => return,
            },
            Target::Prompt => self.prompt.expanded(),
        };
        // The plan is named after its session, so the prompt copy is too,
        // and two nth instances never share one temp file.
        let name = match target {
            Target::Plan => self.plan_path.file_name().unwrap_or_default().to_owned(),
            Target::Prompt => {
                let mut name = self.plan_path.file_stem().unwrap_or_default().to_owned();
                name.push("-prompt.md");
                name
            }
        };
        let copy = std::env::temp_dir().join("nth").join(name);
        if let Err(e) = write_copy(&copy, &original).await {
            self.hint = Some(format!("cannot open the editor: {e}"));
            return;
        }
        *input = None;
        terminal::suspend();
        let editor = editor();
        let path = copy.clone();
        self.editor
            .start(|_| tokio::spawn(async move { run_editor(&editor, &path).await }));
        self.editing = Some(Editing {
            target,
            copy,
            original,
        });
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
        let Some(Editing {
            target,
            copy,
            original,
        }) = self.editing.take()
        else {
            return Ok(());
        };
        let edited = match ended {
            Ok(Ok(status)) if status.success() => tokio::fs::read_to_string(&copy)
                .await
                .map_err(|e| format!("cannot read the edited file: {e}")),
            // :cq in vim, say: you meant to throw the edits away.
            Ok(Ok(status)) => Err(format!("editor exited with {status} · edits dropped")),
            Ok(Err(e)) => Err(format!("editor failed: {e}")),
            Err(e) => Err(format!("editor failed: {e}")),
        };
        // The copy has done its job either way; a leftover in the temp
        // folder is harmless.
        let _ = tokio::fs::remove_file(&copy).await;
        match target {
            Target::Plan => self.plan_edited(&original, edited),
            Target::Prompt => self.prompt_edited(edited),
        }
        Ok(())
    }

    /// Sends `edited` as your edits of `original`, queued behind a running
    /// turn like any prompt, or says why there is nothing to send.
    pub(super) fn plan_edited(&mut self, original: &str, edited: Result<String, String>) {
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
        self.hold_notices = false;
        self.send(Queued::PlanEdits {
            original: original.to_string(),
            edited,
        });
    }

    /// Puts the edited copy back into the prompt, or says on the status bar
    /// why there is nothing to put back.
    fn prompt_edited(&mut self, edited: Result<String, String>) {
        match edited {
            Ok(text) => self.prompt.set(&text),
            Err(why) => self.hint = Some(why),
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
    use nth_session::{Session, plan::edits};

    use super::*;
    use crate::{
        app::{Tab, keys::Action, tests::Idle},
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

    /// An app on a session in a temporary directory whose plan reads
    /// `"# Plan\n"`, with its plan tab showing.
    async fn app_with_plan() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = Session::new("glm", dir.path().to_path_buf());
        let mut app = App::new(session, Arc::new(Idle), Arc::new(Vec::new()));
        std::fs::create_dir_all(app.plan_path.parent().expect("dir")).expect("dirs");
        std::fs::write(&app.plan_path, "# Plan\n").expect("writes");
        app.read_plan();
        let text = app.plan_reading.join().await.expect("read");
        app.plan_read(text);
        app.content.open(Tab::Plan);
        (app, dir)
    }

    #[test]
    fn ctrl_g_on_the_plan_tab_without_a_plan_only_hints() {
        let mut app = crate::app::tests::app();
        app.content.open(Tab::Plan);
        app.apply(Action::Edit);
        assert_eq!(app.hint.as_deref(), Some("no plan yet"));
        assert_eq!(app.pending_editor, None);
    }

    #[test]
    fn ctrl_g_off_the_plan_tab_edits_the_prompt() {
        let mut app = crate::app::tests::app();
        app.apply(Action::Edit);
        assert_eq!(
            app.pending_editor,
            Some(Target::Prompt),
            "an empty prompt opens too"
        );
        assert_eq!(app.hint, None);
    }

    #[tokio::test]
    async fn ctrl_g_on_the_plan_tab_edits_the_plan() {
        let (mut app, _dir) = app_with_plan().await;
        app.apply(Action::Edit);
        assert_eq!(app.pending_editor, Some(Target::Plan));
    }

    #[test]
    fn prompt_edits_become_the_prompt() {
        let mut app = crate::app::tests::app();
        app.prompt.insert_str("draft");

        app.prompt_edited(Ok("edited\nmore\n".into()));
        assert_eq!(app.prompt.text(), "edited\nmore\n");

        app.prompt_edited(Err("editor failed: gone".into()));
        assert_eq!(app.hint.as_deref(), Some("editor failed: gone"));
        assert_eq!(app.prompt.text(), "edited\nmore\n", "kept on failure");
    }

    #[tokio::test]
    async fn edits_go_to_the_model_and_show_as_one_row() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Idle), Arc::new(Vec::new()));

        app.plan_edited("# Plan\n", Ok("# Plan\n".into()));
        assert_eq!(app.hint.as_deref(), Some("no changes to the plan"));
        app.plan_edited("# Plan\n", Err("editor failed: gone".into()));
        assert_eq!(app.hint.as_deref(), Some("editor failed: gone"));
        assert!(!app.is_busy());

        app.plan_edited("# Plan\n", Ok("# Plan\n<!-- why? -->\n".into()));
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
