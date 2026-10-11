//! The plan as rows both front-ends draw, the TUI's work tab as styled
//! lines and `nth work` as coloured text: the tree's shape is decided here
//! once, and each front-end picks the glyphs and colours.

use super::{Item, Plan, Status};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    pub rows: Vec<Row>,
    /// Items done, and all of them.
    pub done: usize,
    pub total: usize,
    pub running: usize,
    pub returned: usize,
    /// Failed, or not readable as an item.
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The guides in front of the row, outermost first.
    pub guides: Vec<Guide>,
    pub kind: Kind,
}

/// One step of a tree's indent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guide {
    /// An earlier sibling's line passing by: `│  `.
    Bar,
    /// Nothing passing by.
    Blank,
    /// A child with more after it: `├─ `.
    Tee,
    /// The last child: `└─ `.
    Elbow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Title(String),
    /// What `plan.md` says under its title.
    Goal(String),
    Cycle {
        number: u32,
        slug: String,
        state: CycleState,
        /// Items closed, and all of them.
        closed: usize,
        total: usize,
    },
    Item(Box<Item>),
    /// A step of the item above, shown while the item is in play.
    Todo {
        text: String,
        done: bool,
    },
    /// What the orchestrator said when it closed the item above.
    Note(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleState {
    /// Every item closed.
    Closed,
    /// The first cycle with an item not yet closed: its items are the ones
    /// that may run.
    Current,
    /// Waiting for the current one.
    Upcoming,
}

impl Plan {
    /// Title, goal, then each cycle with its items under it, and under the
    /// items still in play their todo lists.
    pub fn outline(&self) -> Outline {
        let mut rows = Vec::new();
        let row = |kind| Row {
            guides: Vec::new(),
            kind,
        };
        if let Some(title) = &self.title {
            rows.push(row(Kind::Title(title.clone())));
        }
        if let Some(goal) = &self.goal {
            rows.push(row(Kind::Goal(goal.clone())));
        }
        let current = self.first_open_cycle().map(|cycle| cycle.number);
        for cycle in &self.cycles {
            let closed = cycle.items.iter().filter(|i| !i.status.is_open()).count();
            let state = match current {
                Some(current) if current == cycle.number => CycleState::Current,
                Some(current) if current < cycle.number => CycleState::Upcoming,
                _ => CycleState::Closed,
            };
            rows.push(row(Kind::Cycle {
                number: cycle.number,
                slug: cycle.slug.clone(),
                state,
                closed,
                total: cycle.items.len(),
            }));
            for (i, item) in cycle.items.iter().enumerate() {
                let last = i + 1 == cycle.items.len();
                let (branch, under) = match last {
                    true => (Guide::Elbow, Guide::Blank),
                    false => (Guide::Tee, Guide::Bar),
                };
                rows.push(Row {
                    guides: vec![branch],
                    kind: Kind::Item(Box::new(item.clone())),
                });
                if shows_todos(&item.status) {
                    rows.extend(item.todos.iter().map(|todo| Row {
                        guides: vec![under],
                        kind: Kind::Todo {
                            text: todo.text.clone(),
                            done: todo.done,
                        },
                    }));
                }
                if let Some(note) = &item.note {
                    rows.push(Row {
                        guides: vec![under],
                        kind: Kind::Note(note.clone()),
                    });
                }
            }
        }
        let count = |pick: fn(&Status) -> bool| self.items().filter(|i| pick(&i.status)).count();
        Outline {
            rows,
            done: count(|s| *s == Status::Done),
            total: self.items().count(),
            running: count(|s| *s == Status::Running),
            returned: count(|s| *s == Status::Returned),
            failed: count(|s| matches!(s, Status::Failed | Status::Broken(_))),
        }
    }
}

/// Whether an item's todo list shows under it: while someone works on it
/// or the orchestrator has yet to judge it, or it failed. A pending item's
/// list is unstarted and a done one's finished, so their counts say it.
pub fn shows_todos(status: &Status) -> bool {
    matches!(status, Status::Running | Status::Returned | Status::Failed)
}

#[cfg(test)]
mod tests {
    use nth_protocol::Todo;

    use super::*;
    use crate::work::{Cycle, ItemRef};

    fn item(cycle: &str, slug: &str, status: Status, todos: &[(&str, bool)]) -> Item {
        let at = ItemRef {
            cycle: cycle.into(),
            item: slug.into(),
        };
        let todos = todos
            .iter()
            .map(|(text, done)| Todo {
                text: (*text).into(),
                done: *done,
            })
            .collect();
        Item {
            status,
            ..Item::pending(at, slug, "", todos)
        }
    }

    /// Each row as its guides and a short name, to compare shapes.
    fn shape(outline: &Outline) -> Vec<String> {
        outline
            .rows
            .iter()
            .map(|row| {
                let guides: String = row
                    .guides
                    .iter()
                    .map(|g| match g {
                        Guide::Bar => "| ",
                        Guide::Blank => "  ",
                        Guide::Tee => "+ ",
                        Guide::Elbow => "` ",
                    })
                    .collect();
                let what = match &row.kind {
                    Kind::Title(t) => format!("title {t}"),
                    Kind::Goal(g) => format!("goal {g}"),
                    Kind::Cycle {
                        number,
                        state,
                        closed,
                        total,
                        ..
                    } => format!("cycle {number} {state:?} {closed}/{total}"),
                    Kind::Item(item) => format!("item {} {}", item.at.item, item.status),
                    Kind::Todo { text, done } => format!("todo {text} {done}"),
                    Kind::Note(note) => format!("note {note}"),
                };
                format!("{guides}{what}")
            })
            .collect()
    }

    #[test]
    fn a_plan_outlines_as_cycles_items_and_the_todos_in_play() {
        let plan = Plan {
            title: Some("Retry".into()),
            goal: Some("Add retries.".into()),
            cycles: vec![
                Cycle {
                    number: 1,
                    slug: "scaffold".into(),
                    items: vec![Item {
                        note: Some("merged".into()),
                        ..item("01-scaffold", "trait", Status::Done, &[("a", true)])
                    }],
                },
                Cycle {
                    number: 2,
                    slug: "build".into(),
                    items: vec![
                        item(
                            "02-build",
                            "http",
                            Status::Running,
                            &[("wrap", true), ("cap", false)],
                        ),
                        item("02-build", "docs", Status::Returned, &[("write", true)]),
                    ],
                },
                Cycle {
                    number: 3,
                    slug: "verify".into(),
                    items: vec![item(
                        "03-verify",
                        "test",
                        Status::Pending,
                        &[("run", false)],
                    )],
                },
            ],
        };

        let outline = plan.outline();

        assert_eq!(
            shape(&outline),
            [
                "title Retry",
                "goal Add retries.",
                "cycle 1 Closed 1/1",
                "` item trait done",
                "  note merged",
                "cycle 2 Current 0/2",
                "+ item http running",
                "| todo wrap true",
                "| todo cap false",
                "` item docs returned",
                "  todo write true",
                "cycle 3 Upcoming 0/1",
                "` item test pending",
            ]
        );
        assert_eq!(
            (
                outline.done,
                outline.total,
                outline.running,
                outline.returned,
                outline.failed
            ),
            (1, 4, 1, 1, 0)
        );
    }
}
