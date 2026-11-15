//! `work_todo`: the one work tool an assigned subagent gets. Each call
//! replaces the todo list of the item the subagent runs now and tells
//! the model that assigned it.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, tool_names};
use nth_work::{ItemRef, Status, Todo, one_line, todo_counts};
use serde::Deserialize;
use serde_json::json;

use super::{WorkState, Worker};

/// Bound to the subagent `work_assign` built it for, never to an item:
/// each call asks `Live` which one the subagent runs, since `task_id`
/// may have put it on another since.
pub(super) struct WorkTodo(Worker);

impl WorkTodo {
    pub(super) fn new(worker: Worker) -> Self {
        Self(worker)
    }
}

#[derive(Deserialize)]
struct Args {
    todos: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    text: String,
    #[serde(default)]
    done: bool,
}

impl Tool for WorkTodo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: tool_names::WORK_TODO,
            description: include_str!("todo.md").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The whole list as it is now, in order; it replaces the one before.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "description": "The step, short." },
                                "done": { "type": "boolean", "description": "Whether it is finished." }
                            },
                            "required": ["text", "done"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        _: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = nth_protocol::parse_args(args)?;
            let a = &self.0;
            let todos: Vec<Todo> = args
                .todos
                .into_iter()
                .map(|step| Todo {
                    text: one_line(&step.text),
                    done: step.done,
                })
                .filter(|todo| !todo.text.is_empty())
                .collect();
            let at = {
                let live = a.files.0.lock().await;
                let plan = a.store.dir();
                let Some(at) = live.current(plan, a.task).cloned() else {
                    return Err(match live.last(plan, a.task) {
                        Some(at) => gone(at),
                        None => "you are on no work item; finish with your answer".into(),
                    });
                };
                let item = a
                    .store
                    .read(&at)
                    .await
                    .map_err(|e| format!("reading {at} failed: {e}"))?;
                // Written since by the orchestrator, whose it is now.
                let Some(mut item) =
                    item.filter(|item| item.status == Status::Running && item.task == Some(a.task))
                else {
                    return Err(gone(&at));
                };
                item.todos = todos.clone();
                a.store
                    .write(&item)
                    .await
                    .map_err(|e| format!("writing {at} failed: {e}"))?;
                at
            };
            let (done, total) = todo_counts(&todos);
            a.tell(&at, WorkState::Todo(todos));
            Ok(format!("Todo updated: {done}/{total} done."))
        }
        .boxed()
    }
}

/// Told to a subagent whose turn on the item at `at` is over.
fn gone(at: &ItemRef) -> String {
    format!("{at} is no longer yours to work on; finish with your answer")
}
