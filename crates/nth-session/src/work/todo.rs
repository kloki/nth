//! `work_todo`: the one work tool an assigned subagent gets, bound to its
//! item. Each call replaces the item's todo list and tells the model that
//! assigned it.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, tool_names};
use nth_work::{Status, Todo, one_line, todo_counts};
use serde::Deserialize;
use serde_json::json;

use super::{Assignment, WorkNotice, WorkState};

/// Bound to the item and subagent `work_assign` built it for.
pub(super) struct WorkTodo(Assignment);

impl WorkTodo {
    pub(super) fn new(assignment: Assignment) -> Self {
        Self(assignment)
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
            {
                let _live = a.files.0.lock().await;
                let item = a
                    .store
                    .read(&a.at)
                    .await
                    .map_err(|e| format!("reading {} failed: {e}", a.at))?;
                let Some(mut item) =
                    item.filter(|item| item.status == Status::Running && item.task == Some(a.task))
                else {
                    return Err(format!(
                        "{} is no longer yours to work on; finish with your answer",
                        a.at
                    ));
                };
                item.todos = todos.clone();
                a.store
                    .write(&item)
                    .await
                    .map_err(|e| format!("writing {} failed: {e}", a.at))?;
            }
            let (done, total) = todo_counts(&todos);
            // Not once forgotten with the session that assigned it: a
            // later one's model would read about another plan.
            a.parent.tell(|| {
                a.inbox.post_work(WorkNotice {
                    cycle: a.at.cycle.clone(),
                    item: a.at.item.clone(),
                    task: a.task,
                    agent: a.agent.clone(),
                    state: WorkState::Todo(todos),
                });
            });
            Ok(format!("Todo updated: {done}/{total} done."))
        }
        .boxed()
    }
}
