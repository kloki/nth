//! The work tab, wired to the session's work directory: read in the
//! background after any work tool ran, in the model's turn or a
//! subagent's, after each subagent's turn and at the end of every turn,
//! and shown in a tab while there is a plan.

use std::path::{Path, PathBuf};

use nth_session::work::{Plan, Store};
use tokio::task::JoinError;

use super::{App, Tab, content::Change, job::Job};
use crate::work::{Live, LiveTask, WorkView};

/// What a subagent's tab says it works on: its description.
const WORK_PREFIX: &str = "work ";

/// A read of the work plan, with the directory it read.
type Read = (PathBuf, Option<Plan>);

/// The session's work plan, where it is read from, and what the work tab
/// shows of it.
pub(super) struct WorkTab {
    pub(super) view: WorkView,
    pub(super) dir: PathBuf,
    pub(super) reading: Job<Read>,
    /// The next read is of a session just opened, so a plan it finds
    /// opens its tab without showing it.
    settle: bool,
}

impl WorkTab {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            view: WorkView::default(),
            dir,
            reading: Job::default(),
            settle: true,
        }
    }

    /// Points the tab at the directory of the session the app is on, and
    /// reads it as already there.
    pub(super) fn for_session(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.settle = true;
        self.read();
    }

    /// Reads the plan in the background; a tool may just have written it.
    pub(super) fn read(&mut self) {
        let dir = self.dir.clone();
        self.reading.start_or_queue(|_| {
            // A plan that can't be read shows as none, as the plan file does.
            tokio::spawn(async move {
                let plan = Store::new(dir.clone()).load().await.ok().flatten();
                (dir, plan)
            })
        });
    }

    /// The read running, once it is back; never while none runs.
    pub(super) async fn next(&mut self) -> Result<Read, JoinError> {
        self.reading.join().await
    }

    /// Takes in the plan as read from `dir`, and says what that did to it;
    /// `None` for a read of a session the app has since left, whose
    /// queued read reads this one's.
    fn took(&mut self, (dir, plan): Read) -> Option<Change> {
        if dir != self.dir {
            return None;
        }
        let settle = std::mem::take(&mut self.settle);
        let existed = self.view.exists();
        self.view.update(plan);
        Some(if !settle && !existed && self.view.exists() {
            Change::Created
        } else if self.view.exists() {
            Change::Present
        } else {
            Change::Gone
        })
    }

    /// The work directory as the tab names it: relative to `cwd`.
    fn place(&self, cwd: &Path) -> String {
        self.dir
            .strip_prefix(cwd)
            .unwrap_or(&self.dir)
            .display()
            .to_string()
    }
}

impl App {
    /// The plan as read: its tab shows when the plan is first written,
    /// stays open while there is one, and closes once there is none.
    pub(super) fn work_read(&mut self, read: Read) {
        if let Some(change) = self.work.took(read) {
            self.tab_read(Tab::Work, change);
        }
        if self.work.reading.take_again() {
            self.work.read();
        }
    }

    /// The work directory as the tab names it: relative to the working
    /// directory.
    pub(super) fn work_place(&self) -> String {
        self.work.place(&self.cwd)
    }

    /// This run's subagents on work items, by the item, from their tabs.
    pub(super) fn live_work(&self) -> Live {
        self.subagent_views
            .iter()
            .filter_map(|(id, view)| {
                let item = view.description.strip_prefix(WORK_PREFIX)?;
                let task = LiveTask {
                    task: *id,
                    since: view.running_since(),
                    tool_calls: view.tool_calls(),
                };
                Some((item.to_string(), task))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Event, Panel, Screen, TaskOutcome, ToolCall};
    use nth_session::{
        SubagentEvent,
        work::{Item, ItemRef, Status},
    };
    use ratatui::style::Color;

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, rows, tab_colour},
    };

    /// An app whose session's work plan is in a fresh directory.
    fn app_with_work() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app();
        // Under the working directory, so the header names it short on
        // any platform's temporary directory.
        app.cwd = dir.path().to_path_buf();
        app.work.dir = dir.path().join("work");
        (app, dir)
    }

    async fn write(app: &App, cycle: &str, slug: &str, status: Status, task: Option<u32>) {
        let at = ItemRef {
            cycle: cycle.into(),
            item: slug.into(),
        };
        let todos = vec![
            nth_protocol::Todo {
                text: "one".into(),
                done: true,
            },
            nth_protocol::Todo {
                text: "two".into(),
                done: false,
            },
        ];
        let item = Item {
            status,
            agent: task.map(|_| "general".into()),
            task,
            ..Item::pending(at, slug, "Do it.", todos)
        };
        Store::new(app.work.dir.clone()).write(&item).await.unwrap();
    }

    async fn read(app: &mut App) {
        app.work.read();
        let read = app.work.reading.join().await.unwrap();
        app.work_read(read);
    }

    fn tool_finished(name: &str) -> Event {
        Event::ToolFinished {
            call: ToolCall {
                id: "1".into(),
                name: name.into(),
                arguments: "{}".into(),
            },
            result: Ok(String::new()),
        }
    }

    #[tokio::test]
    async fn a_new_plan_shows_its_tab_and_the_tree() {
        let (mut app, _dir) = app_with_work();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat], "no plan, no tab");

        write(&app, "01-a", "store", Status::Done, None).await;
        write(&app, "02-b", "tab", Status::Pending, None).await;
        read(&mut app).await;

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Work]);
        assert_eq!(app.content.active(), Tab::Work);
        assert!(app.flash.is_some(), "it comes in after a blank");
        app.flash = None;
        let top = rows(&mut app);
        assert!(top[0].starts_with("  › chat [⊞ work 1/2] "), "{top:?}");
        assert!(top[2].starts_with(" ▎ work · 1/2 done"), "{top:?}");
        // The scrollbar sits on the right: the second cycle's item is below.
        let shown = |row: &str| row.trim_end_matches([' ', '┃']).to_string();
        assert_eq!(shown(&top[4]), " ▎ ✓ 1 a  1/1");
        assert_eq!(shown(&top[5]), " ▎   ✓ store  1/2");
        assert_eq!(shown(&top[7]), " ▎ → 2 b  0/1");
        app.work.view.pager.jump_bottom();
        let bottom = rows(&mut app);
        assert!(
            bottom.iter().any(|r| shown(r) == " ▎   ● tab  1/2"),
            "{bottom:?}"
        );
    }

    #[tokio::test]
    async fn progress_and_a_sessions_plan_open_the_tab_unfocused() {
        let (mut app, _dir) = app_with_work();
        write(&app, "01-a", "store", Status::Pending, None).await;
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Work]);
        assert_eq!(app.content.active(), Tab::Chat, "a session's plan");

        app.content.remove(Tab::Work);
        write(&app, "01-a", "store", Status::Done, None).await;
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Work]);
        assert_eq!(app.content.active(), Tab::Chat, "progress");
    }

    #[tokio::test]
    async fn a_running_item_follows_its_subagent_and_without_one_is_lost() {
        let (mut app, _dir) = app_with_work();
        write(&app, "01-a", "store", Status::Running, Some(1)).await;
        read(&mut app).await;
        assert_eq!(tab_colour(&mut app, "⊞ work"), Color::Magenta, "lost");

        app.on_subagent(SubagentEvent::Started {
            id: 1,
            agent: "general".into(),
            description: "work 01-a/store".into(),
            model: "glm".into(),
        });
        app.on_subagent(SubagentEvent::Prompted {
            id: 1,
            text: "go".into(),
        });
        assert_eq!(tab_colour(&mut app, "⊞ work"), Color::Blue, "working");

        app.on_subagent(SubagentEvent::Session {
            id: 1,
            event: tool_finished("work_todo"),
        });
        assert!(app.work.reading.is_running(), "a ticked todo is read");
        app.work.reading.join().await.unwrap();
        app.on_subagent(SubagentEvent::TurnEnded {
            id: 1,
            outcome: TaskOutcome::Completed("ok".into()),
            elapsed: std::time::Duration::from_secs(1),
            model: "glm".into(),
        });
        assert!(app.work.reading.is_running(), "and so is a return");
    }

    #[tokio::test]
    async fn work_tools_reread_the_plan_and_a_gone_plan_closes_the_tab() {
        let (mut app, dir) = app_with_work();
        write(&app, "01-a", "store", Status::Failed, None).await;
        read(&mut app).await;
        assert_eq!(tab_colour(&mut app, "⊞ work"), Color::Red);

        app.apply(Action::Content(1));
        app.close_content();
        assert_eq!(
            app.hint.as_deref(),
            Some("the work tab stays while there is a plan")
        );

        app.on_session(tool_finished("work_item"));
        assert!(app.work.reading.is_running());
        app.work.reading.join().await.unwrap();
        std::fs::remove_dir_all(dir.path().join("work")).unwrap();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat]);
    }

    #[tokio::test]
    async fn todo_ticks_wait_for_a_wake_and_a_return_is_one() {
        let mut app = app();
        let notice = |state| nth_protocol::WorkNotice {
            cycle: "01-a".into(),
            item: "store".into(),
            task: 1,
            agent: "general".into(),
            state,
        };
        app.inbox
            .post_work(notice(nth_protocol::WorkState::Todo(Vec::new())));
        app.on_subagent(SubagentEvent::Session {
            id: 1,
            event: tool_finished("work_todo"),
        });
        assert_eq!(app.notices_due, None, "a tick alone wakes nobody");

        app.inbox
            .post_work(notice(nth_protocol::WorkState::Returned {
                outcome: nth_protocol::WorkOutcome::Completed("ok".into()),
                done: 0,
                total: 0,
            }));
        app.on_subagent(SubagentEvent::Stalled {
            id: 1,
            quiet: std::time::Duration::from_secs(1),
        });
        assert!(app.notices_due.is_some());
    }

    #[tokio::test]
    async fn a_read_of_a_session_left_is_dropped() {
        let (mut app, dir) = app_with_work();
        write(&app, "01-a", "store", Status::Pending, None).await;
        app.work.read();
        app.work.for_session(dir.path().join("other"));

        let stale = app.work.reading.join().await.unwrap();
        app.work_read(stale);

        assert_eq!(app.content.tabs(), [Tab::Chat], "the old plan never showed");
        assert!(app.work.reading.is_running(), "the new one is read");
    }

    #[tokio::test]
    async fn the_model_and_slash_work_show_the_tab() {
        let (mut app, _dir) = app_with_work();
        write(&app, "01-a", "store", Status::Pending, None).await;
        read(&mut app).await;

        let screen = Screen::new(app.screen_tx.clone());
        assert!(screen.show(Panel::Work).await);
        let show = app.screen_rx.recv().await.unwrap();
        app.show(show);
        assert_eq!(app.content.active(), Tab::Work);

        app.apply(Action::Content(0));
        app.run_command(crate::command::Command::Work);
        assert_eq!(app.content.active(), Tab::Work);
    }
}
