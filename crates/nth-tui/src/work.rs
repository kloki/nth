//! The work tab: the orchestration's plan from `.nth/work/<session>/` as a
//! tree of cycles and their items, each item marked by how it is doing,
//! with the todo lists of the ones in play.

use std::{collections::BTreeMap, time::Instant};

use nth_icons::icons;
use nth_session::{
    subagent::SubagentId,
    work::{
        Item, Plan, Status,
        outline::{CycleState, Guide, Kind, Outline, Row},
    },
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, ScrollbarState},
};
use unicode_width::UnicodeWidthStr;

use crate::{app::TabState, rich, spinner, theme};

/// What this run's tab of the subagent on an item knows about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveTask {
    pub task: SubagentId,
    /// When its turn began, while it runs.
    pub since: Option<Instant>,
    pub tool_calls: usize,
}

/// This run's subagents by the item they work on, `01-a/store`.
pub type Live = BTreeMap<String, LiveTask>;

/// How an item reads given who of this run works on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// As its file says.
    AsFiled,
    /// Running, with its subagent's tab here.
    Live(LiveTask),
    /// Running in its file, with no subagent of this run on it: a run of
    /// nth that ended left it so. The next work tool call settles it.
    Lost,
}

fn seen(item: &Item, live: &Live) -> Seen {
    if item.status != Status::Running {
        return Seen::AsFiled;
    }
    match live.get(&item.at.to_string()) {
        Some(task) if Some(task.task) == item.task => Seen::Live(*task),
        _ => Seen::Lost,
    }
}

#[derive(Debug, Default)]
pub struct WorkView {
    /// `None` while the session has no plan.
    plan: Option<Plan>,
    top: usize,
    max_top: usize,
    height: usize,
}

impl WorkView {
    pub fn exists(&self) -> bool {
        self.plan
            .as_ref()
            .is_some_and(|plan| !plan.cycles.is_empty())
    }

    /// The plan as read now; `None` when there is none.
    pub fn update(&mut self, plan: Option<Plan>) {
        self.plan = plan;
    }

    /// `work`, or `work 3/7` once there are items: done over all.
    pub fn label(&self) -> String {
        match self.plan.as_ref().map(Plan::outline) {
            Some(outline) if outline.total > 0 => {
                format!("work {}/{}", outline.done, outline.total)
            }
            _ => "work".into(),
        }
    }

    /// Failed over lost over running over all done.
    pub fn state(&self, live: &Live) -> TabState {
        let Some(plan) = &self.plan else {
            return TabState::Idle;
        };
        let items: Vec<_> = plan.items().collect();
        let any = |pick: &dyn Fn(&Item) -> bool| items.iter().any(|item| pick(item));
        if any(&|i| matches!(i.status, Status::Failed | Status::Broken(_))) {
            TabState::Failed
        } else if any(&|i| seen(i, live) == Seen::Lost) {
            TabState::NeedsYou
        } else if any(&|i| i.status == Status::Running) {
            TabState::Working
        } else if !items.is_empty() && items.iter().all(|i| i.status == Status::Done) {
            TabState::Done
        } else {
            TabState::Idle
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = (self.top + lines).min(self.max_top);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.max(1));
    }

    pub fn jump_top(&mut self) {
        self.top = 0;
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
    }

    /// Where the view sits in the tab, while it doesn't all fit.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        (self.max_top > 0).then(|| {
            ScrollbarState::new(self.max_top + 1)
                .position(self.top)
                .viewport_content_length(self.height)
        })
    }

    /// `place` is the work directory as the header names it. Drawn anew
    /// every frame: a running item's spinner and time move.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, place: &str, live: &Live) {
        let lines = self.lines(place, usize::from(area.width), live);
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = self.top.min(self.max_top);
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    /// The header, then the tree, every row wrapped to `width`.
    fn lines(&self, place: &str, width: usize, live: &Live) -> Vec<Line<'static>> {
        let dim = theme::dim();
        let Some(plan) = self.plan.as_ref().filter(|plan| !plan.cycles.is_empty()) else {
            return vec![Line::styled("No work plan yet.", dim)];
        };
        let outline = plan.outline();
        let mut lines = vec![Line::styled(header(place, &outline, plan, live), dim)];
        lines.push(Line::default());
        let mut overview = false;
        for row in &outline.rows {
            if matches!(row.kind, Kind::Cycle { .. }) && overview {
                lines.push(Line::default());
                overview = false;
            }
            if matches!(row.kind, Kind::Title(_) | Kind::Goal(_)) {
                overview = true;
            }
            lines.extend(wrapped(row, width, live));
        }
        lines
    }
}

/// `.nth/work/6b2e3f1a… · 3/7 done · 1 running`, the counts that are
/// not zero.
fn header(place: &str, outline: &Outline, plan: &Plan, live: &Live) -> String {
    let lost = plan.items().filter(|i| seen(i, live) == Seen::Lost).count();
    let mut parts = vec![
        place.to_string(),
        format!("{}/{} done", outline.done, outline.total),
    ];
    for (count, what) in [
        (outline.running - lost, "running"),
        (outline.returned, "returned"),
        (outline.failed, "failed"),
        (lost, "lost"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {what}"));
        }
    }
    parts.join(" · ")
}

/// The guides in front of a row, and in front of its wrapped rest.
fn guides(row: &Row) -> (String, String) {
    let tree = &icons().tree;
    let (mut first, mut rest) = (String::new(), String::new());
    for guide in &row.guides {
        let (here, after) = match guide {
            Guide::Bar => (format!("{}  ", tree.bar), format!("{}  ", tree.bar)),
            Guide::Blank => ("   ".to_string(), "   ".to_string()),
            Guide::Tee => (
                format!("{}{} ", tree.tee, tree.dash),
                format!("{}  ", tree.bar),
            ),
            Guide::Elbow => (format!("{}{} ", tree.elbow, tree.dash), "   ".to_string()),
        };
        first.push_str(&here);
        rest.push_str(&after);
    }
    // A todo or note sits under its item's name, past the item's mark.
    if matches!(row.kind, Kind::Todo { .. } | Kind::Note(_)) {
        first.push_str("  ");
        rest.push_str("  ");
    }
    (first, rest)
}

fn wrapped(row: &Row, width: usize, live: &Live) -> Vec<Line<'static>> {
    let (first, rest) = guides(row);
    let room = width.saturating_sub(first.width()).max(1);
    let content = Line::from(spans(&row.kind, live));
    rich::wrap(content, room)
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            let guide = if i == 0 { first.clone() } else { rest.clone() };
            line.spans.insert(0, Span::styled(guide, theme::dim()));
            line
        })
        .collect()
}

fn spans(kind: &Kind, live: &Live) -> Vec<Span<'static>> {
    let dim = theme::dim();
    let colour = |colour| Style::new().fg(colour);
    let icons = icons();
    match kind {
        Kind::Title(title) => vec![Span::styled(
            title.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )],
        Kind::Goal(goal) => vec![Span::styled(goal.replace('\n', " "), dim)],
        Kind::Cycle {
            number,
            slug,
            state,
            closed,
            total,
        } => {
            let (icon, icon_style, name_style) = match state {
                CycleState::Closed => (icons.ok, colour(Color::Green), Style::new()),
                CycleState::Current => (
                    icons.current,
                    colour(Color::Blue),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                CycleState::Upcoming => (icons.work.upcoming, dim, dim),
            };
            vec![
                Span::styled(format!("{icon} "), icon_style),
                Span::styled(format!("{number} {slug}"), name_style),
                Span::styled(format!("  {closed}/{total}"), dim),
            ]
        }
        Kind::Item(item) => item_spans(item, live),
        Kind::Todo { text, done: true } => vec![
            Span::styled(format!("{} ", icons.checked), colour(Color::Green)),
            Span::styled(text.clone(), dim),
        ],
        Kind::Todo { text, done: false } => vec![
            Span::styled(format!("{} ", icons.unchecked), dim),
            Span::raw(text.clone()),
        ],
        Kind::Note(note) => vec![Span::styled(format!("note: {note}"), dim)],
    }
}

fn item_spans(item: &Item, live: &Live) -> Vec<Span<'static>> {
    let dim = theme::dim();
    let colour = |colour| Style::new().fg(colour);
    let icons = icons();
    let seen = seen(item, live);
    let (icon, icon_style) = match (&item.status, seen) {
        (_, Seen::Lost) => (icons.interrupted, colour(Color::Yellow)),
        (
            _,
            Seen::Live(LiveTask {
                since: Some(since), ..
            }),
        ) => (spinner::dot(since.elapsed()), colour(Color::Yellow)),
        (Status::Running, _) => (icons.dot, colour(Color::Yellow)),
        (Status::Pending, _) => (icons.work.pending, dim),
        (Status::Returned, _) => (icons.done, colour(Color::Magenta)),
        (Status::Done, _) => (icons.ok, colour(Color::Green)),
        (Status::Failed | Status::Broken(_), _) => (icons.fail, colour(Color::Red)),
    };
    let name = match item.status {
        Status::Pending => dim,
        _ => Style::new(),
    };
    let mut spans = vec![
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(item.at.item.clone(), name),
    ];
    let (done, total) = item.progress();
    if total > 0 {
        spans.push(Span::styled(format!("  {done}/{total}"), dim));
    }
    if matches!(
        item.status,
        Status::Running | Status::Returned | Status::Failed
    ) && let (Some(agent), Some(task)) = (&item.agent, item.task)
    {
        spans.push(Span::styled(
            format!("  @{agent}"),
            colour(theme::SUBAGENT_COLOUR),
        ));
        spans.push(Span::styled(format!(" #{task}"), dim));
    }
    match (&item.status, seen) {
        (_, Seen::Lost) => spans.push(Span::styled(" · lost", colour(Color::Yellow))),
        (
            _,
            Seen::Live(LiveTask {
                since: Some(since),
                tool_calls,
                ..
            }),
        ) => {
            let calls = match tool_calls {
                1 => "1 call".to_string(),
                n => format!("{n} calls"),
            };
            spans.push(Span::styled(
                format!(" · {}s · {calls}", since.elapsed().as_secs()),
                dim,
            ));
        }
        (Status::Returned, _) => {
            let outcome = item
                .outcome
                .as_ref()
                .map(|o| format!(" {}", o.name()))
                .unwrap_or_default();
            spans.push(Span::styled(
                format!(" · returned{outcome}"),
                colour(Color::Magenta),
            ));
        }
        (Status::Failed, _) => spans.push(Span::styled(" · failed", colour(Color::Red))),
        (Status::Broken(why), _) => {
            spans.push(Span::styled(format!(" · {why}"), colour(Color::Red)));
        }
        _ => {}
    }
    spans
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Todo, WorkOutcome};
    use nth_session::work::{Cycle, ItemRef};

    use super::*;

    pub(crate) fn item(cycle: &str, slug: &str, status: Status, todos: &[(&str, bool)]) -> Item {
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

    fn plan() -> Plan {
        Plan {
            title: Some("Retry".into()),
            ignored: Vec::new(),
            goal: Some("Add retries with jitter to every client call.".into()),
            cycles: vec![
                Cycle {
                    number: 1,
                    slug: "scaffold".into(),
                    items: vec![item("01-scaffold", "trait", Status::Done, &[("a", true)])],
                },
                Cycle {
                    number: 2,
                    slug: "build".into(),
                    items: vec![
                        Item {
                            agent: Some("general".into()),
                            task: Some(3),
                            ..item(
                                "02-build",
                                "http-retry",
                                Status::Running,
                                &[("wrap idempotent requests", true), ("cap", false)],
                            )
                        },
                        Item {
                            agent: Some("explore".into()),
                            task: Some(4),
                            outcome: Some(WorkOutcome::Completed("ok".into())),
                            ..item("02-build", "docs", Status::Returned, &[("write", true)])
                        },
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
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn the_tree_marks_each_item_and_shows_the_todos_in_play() {
        let mut view = WorkView::default();
        view.update(Some(plan()));
        let live = Live::from([(
            "02-build/http-retry".to_string(),
            LiveTask {
                task: 3,
                since: None,
                tool_calls: 0,
            },
        )]);

        let lines = view.lines(".nth/work/s1", 40, &live);

        assert_eq!(
            text(&lines),
            [
                ".nth/work/s1 · 1/4 done · 1 running · 1 returned",
                "",
                "Retry",
                "Add retries with jitter to every client",
                "call.",
                "",
                "✓ 1 scaffold  1/1",
                "└─ ✓ trait  1/1",
                "▸ 2 build  0/2",
                "├─ ● http-retry  1/2  @general #3",
                "│    ☒ wrap idempotent requests",
                "│    ☐ cap",
                "└─ ∎ docs  1/1  @explore #4 · returned",
                "   completed",
                "     ☒ write",
                "◇ 3 verify  0/1",
                "└─ ○ test  0/1",
            ]
        );
        assert_eq!(view.label(), "work 1/4");
        assert_eq!(view.state(&live), TabState::Working);
    }

    #[test]
    fn a_running_item_nobody_here_works_on_is_lost() {
        let mut view = WorkView::default();
        view.update(Some(plan()));
        let other = Live::from([(
            "02-build/http-retry".to_string(),
            LiveTask {
                task: 7,
                since: None,
                tool_calls: 0,
            },
        )]);

        let lines = text(&view.lines("w", 60, &other));

        assert_eq!(lines[0], "w · 1/4 done · 1 returned · 1 lost");
        assert_eq!(lines[8], "├─ ⏹ http-retry  1/2  @general #3 · lost");
        assert_eq!(view.state(&other), TabState::NeedsYou);
        assert_eq!(view.state(&Live::new()), TabState::NeedsYou);
    }

    #[test]
    fn the_tab_turns_red_on_a_failure_and_green_when_all_is_done() {
        let mut view = WorkView::default();
        assert!(!view.exists());
        assert_eq!(view.label(), "work");
        let mut plan = plan();
        plan.cycles[2].items[0].status = Status::Failed;
        view.update(Some(plan.clone()));
        assert_eq!(view.state(&Live::new()), TabState::Failed);
        for item in plan.cycles.iter_mut().flat_map(|c| &mut c.items) {
            item.status = Status::Done;
        }
        view.update(Some(plan));
        assert!(view.exists());
        assert_eq!(view.state(&Live::new()), TabState::Done);
        assert_eq!(view.label(), "work 4/4");
    }
}
