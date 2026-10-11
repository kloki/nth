//! The work tab, wired to the session's work directory: read in the
//! background after any work tool ran, in the model's turn or a
//! subagent's, after each subagent's turn and at the end of every turn,
//! and shown in a tab while there is a plan.

use std::path::PathBuf;

use nth_session::work::{Plan, Store};

use super::{App, Tab, flash::Flash};
use crate::work::{Live, LiveTask};

/// What a subagent's tab says it works on: its description.
const WORK_PREFIX: &str = "work ";

impl App {
    /// Points the work tab at the directory of the session the app is on,
    /// and reads it as already there.
    pub(super) fn work_for_session(&mut self, dir: PathBuf) {
        self.work_dir = dir;
        self.work_settle = true;
        self.read_work();
    }

    /// Reads the plan in the background; a tool may just have written it.
    pub(super) fn read_work(&mut self) {
        let store = Store::new(self.work_dir.clone());
        self.work_reading.start_or_queue(|_| {
            // A plan that can't be read shows as none, as the plan file does.
            tokio::spawn(async move { store.load().await.ok().flatten() })
        });
    }

    /// The plan as read: its tab shows when the plan is first written,
    /// stays open while there is one, and closes once there is none.
    pub(super) fn work_read(&mut self, plan: Option<Plan>) {
        let settle = std::mem::take(&mut self.work_settle);
        let existed = self.work.exists();
        self.work.update(plan);
        if !settle && !existed && self.work.exists() {
            // The first work plan is what you will want to watch, and it
            // comes in after a blank like a tab the model shows you.
            self.content.open(Tab::Work);
            self.flash = Some(Flash::new());
        } else if self.work.exists() {
            // Progress opens the tab unfocused, so the reply the model is
            // writing stays in view.
            self.content.add(Tab::Work);
        } else {
            self.content.remove(Tab::Work);
        }
        if self.work_reading.take_again() {
            self.read_work();
        }
    }

    /// The work directory as the tab names it: relative to the working
    /// directory.
    pub(super) fn work_place(&self) -> String {
        self.work_dir
            .strip_prefix(&self.cwd)
            .unwrap_or(&self.work_dir)
            .display()
            .to_string()
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
        app.work_dir = dir.path().join("work");
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
        Store::new(app.work_dir.clone()).write(&item).await.unwrap();
    }

    async fn read(app: &mut App) {
        app.read_work();
        let plan = app.work_reading.join().await.unwrap();
        app.work_read(plan);
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
        let rows = rows(&mut app);
        assert!(rows[0].starts_with("  › chat [⊞ work 1/2] "), "{rows:?}");
        assert!(rows[2].starts_with(" work · 1/2 done"), "{rows:?}");
        assert_eq!(rows[4].trim_end(), " ✓ 1 a  1/1");
        assert_eq!(rows[5].trim_end(), " └─ ✓ store  1/2");
        assert_eq!(rows[6].trim_end(), " ▸ 2 b  0/1");
        assert_eq!(rows[7].trim_end(), " └─ ○ tab  1/2");
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
        assert!(app.work_reading.is_running(), "a ticked todo is read");
        app.work_reading.join().await.unwrap();
        app.on_subagent(SubagentEvent::TurnEnded {
            id: 1,
            outcome: TaskOutcome::Completed("ok".into()),
            elapsed: std::time::Duration::from_secs(1),
            model: "glm".into(),
        });
        assert!(app.work_reading.is_running(), "and so is a return");
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
        assert!(app.work_reading.is_running());
        app.work_reading.join().await.unwrap();
        std::fs::remove_dir_all(dir.path().join("work")).unwrap();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat]);
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
