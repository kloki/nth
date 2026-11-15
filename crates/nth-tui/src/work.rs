//! The work tab: the orchestration's plan from `.nth/work/<session>/` as a
//! tree of cycles and their items, each item marked by how it is doing,
//! with the todo lists of the ones in play.

use std::{collections::BTreeMap, time::Instant};

use nth_icons::icons;
use nth_session::subagent::SubagentId;
use nth_work::{
    Item, Plan, Status,
    outline::{CycleState, Kind, Outline},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{app::TabState, pager::Pager, rich, spinner, theme};

/// What this run's tab of a work subagent knows about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveTask {
    /// When its turn began, while it runs.
    pub since: Option<Instant>,
    pub tool_calls: usize,
}

/// This run's work subagents by their number, which is what an item file
/// names: one subagent works on several items of a plan in turn, so its
/// tab's description, fixed when it started, cannot say which.
pub type Live = BTreeMap<SubagentId, LiveTask>;

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

/// Numbers start again with every run of nth, so an item a gone run left
/// running under a number a work subagent of this run also has reads as
/// live. That lasts only until the plan is next read: this run has a work
/// subagent only after a `work_assign`, and every work tool call first
/// settles what a gone run left.
fn seen(item: &Item, live: &Live) -> Seen {
    if item.status != Status::Running {
        return Seen::AsFiled;
    }
    match item.task.and_then(|task| live.get(&task)) {
        Some(task) => Seen::Live(*task),
        None => Seen::Lost,
    }
}

#[derive(Debug, Default)]
pub struct WorkView {
    /// `None` while the session has no plan.
    plan: Option<Plan>,
    pub pager: Pager,
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

    /// The subagent on each running item, by the item, `01-a/store`.
    pub fn running(&self) -> impl Iterator<Item = (SubagentId, String)> + '_ {
        let items = self.plan.iter().flat_map(Plan::items);
        items
            .filter(|item| item.status == Status::Running)
            .filter_map(|item| Some((item.task?, item.at.to_string())))
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

    /// `place` is the work directory as the header names it. Drawn anew
    /// every frame: a running item's spinner and time move.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, place: &str, live: &Live) {
        let lines = self.lines(place, usize::from(area.width), live);
        self.pager.render(frame, area, lines);
    }

    /// The header and overview, then a block per cycle, every row behind
    /// a bar in the colour of what it belongs to and wrapped to `width`.
    fn lines(&self, place: &str, width: usize, live: &Live) -> Vec<Line<'static>> {
        let dim = theme::dim();
        let Some(plan) = self.plan.as_ref().filter(|plan| !plan.cycles.is_empty()) else {
            return theme::sections(vec![vec![Line::styled("No work plan yet.", dim)]]);
        };
        let outline = plan.outline();
        let room = width.saturating_sub(usize::from(theme::BAR_WIDTH)).max(1);
        let mut overview = vec![Line::styled(header(place, &outline, plan, live), dim)];
        overview.extend(models(plan, room));
        overview.push(Line::default());
        let mut cycles: Vec<Vec<Line<'static>>> = Vec::new();
        // The bar of the item a todo or note belongs to.
        let mut item = theme::SECTION_COLOUR;
        for row in &outline.rows {
            match &row.kind {
                // `plan.md` as written: its heading, then the rest, as markdown.
                Kind::Title(title) => {
                    overview.extend(rich::markdown(&format!("# {title}"), room).lines)
                }
                Kind::Goal(goal) => overview.extend(rich::markdown(goal, room).lines),
                Kind::Cycle { state, .. } => {
                    cycles.push(wrapped(&row.kind, width, cycle_colour(*state), 0, live));
                }
                Kind::Item(it) => {
                    item = item_colour(it, live);
                    push(&mut cycles, wrapped(&row.kind, width, item, ITEM, live));
                }
                Kind::Todo { .. } | Kind::Note(_) => {
                    push(&mut cycles, wrapped(&row.kind, width, item, TODO, live));
                }
            }
        }
        if matches!(overview.last(), Some(line) if line.spans.is_empty()) {
            overview.pop();
        }
        let mut blocks = theme::sections(vec![overview]);
        blocks.push(Line::default());
        blocks.extend(theme::join(cycles));
        blocks
    }
}

/// How far in an item sits under its cycle, and a todo or note under the
/// item's name, past its mark.
const ITEM: usize = theme::INDENT.len();
const TODO: usize = ITEM + 2;

/// Adds `lines` to the last cycle's block.
fn push(cycles: &mut Vec<Vec<Line<'static>>>, lines: Vec<Line<'static>>) {
    match cycles.last_mut() {
        Some(cycle) => cycle.extend(lines),
        None => cycles.push(lines),
    }
}

/// A cycle's bar: green once closed, blue while current, grey ahead.
fn cycle_colour(state: CycleState) -> Color {
    match state {
        CycleState::Closed => Color::Green,
        CycleState::Current => Color::Blue,
        CycleState::Upcoming => theme::SECTION_COLOUR,
    }
}

/// An item's bar and mark: grey pending, yellow running or lost, magenta
/// returned, green done, red failed.
fn item_colour(item: &Item, live: &Live) -> Color {
    match (&item.status, seen(item, live)) {
        (_, Seen::Lost) | (Status::Running, _) => Color::Yellow,
        (Status::Pending, _) => theme::SECTION_COLOUR,
        (Status::Returned, _) => Color::Magenta,
        (Status::Done, _) => Color::Green,
        (Status::Failed | Status::Broken(_), _) => Color::Red,
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

/// `zen/glm 3 · chatgpt/gpt-5 2`: the models the plan's items were worked
/// on and how many items each, under the header. A row of its own, since
/// the header's counts already fill most of a narrow tab; a whole entry
/// moves to the next row when `room` runs out, rather than its count
/// alone. An entry wider than the tab is cut there, as the header is.
fn models(plan: &Plan, room: usize) -> Vec<Line<'static>> {
    let dim = theme::dim();
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (model, count) in plan.models() {
        let mut entry = theme::model_spans(model, None);
        entry.push(Span::styled(format!(" {count}"), dim));
        let width: usize = entry.iter().map(Span::width).sum();
        match rows.last_mut() {
            Some(row) if row.width() + Span::raw(SEPARATOR).width() + width <= room => {
                row.spans.push(Span::styled(SEPARATOR, dim));
                row.spans.append(&mut entry);
            }
            _ => rows.push(Line::from(entry)),
        }
    }
    rows
}

/// Between the parts of a row; three columns wide.
const SEPARATOR: &str = " · ";

/// `kind` as rows behind a `bar` bar, `indent` columns in, its wrapped
/// rest under its text, past its mark.
fn wrapped(
    kind: &Kind,
    width: usize,
    bar: Color,
    indent: usize,
    live: &Live,
) -> Vec<Line<'static>> {
    let mut spans = spans(kind, live);
    let mark = match kind {
        Kind::Note(_) => Span::raw(""),
        _ => spans.remove(0),
    };
    let hang = " ".repeat(mark.width());
    let room = width
        .saturating_sub(usize::from(theme::BAR_WIDTH) + indent + mark.width())
        .max(1);
    rich::wrap(Line::from(spans), room)
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            let lead = match i {
                0 => mark.clone(),
                _ => Span::raw(hang.clone()),
            };
            let mut row = vec![
                Span::styled(theme::BAR, Style::new().fg(bar)),
                Span::raw(" ".repeat(indent)),
                lead,
            ];
            row.append(&mut line.spans);
            Line::from(row)
        })
        .collect()
}

fn spans(kind: &Kind, live: &Live) -> Vec<Span<'static>> {
    let dim = theme::dim();
    let colour = |colour| Style::new().fg(colour);
    let icons = icons();
    match kind {
        // The title and goal render as markdown in `wrapped`, not here.
        Kind::Title(_) | Kind::Goal(_) => Vec::new(),
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
    let icon = match (&item.status, seen) {
        (_, Seen::Lost) => icons.interrupted,
        (
            _,
            Seen::Live(LiveTask {
                since: Some(since), ..
            }),
        ) => spinner::dot(since.elapsed()),
        (Status::Running, _) => icons.dot,
        (Status::Pending, _) => icons.work.pending,
        (Status::Returned, _) => icons.done,
        (Status::Done, _) => icons.ok,
        (Status::Failed | Status::Broken(_), _) => icons.fail,
    };
    let icon_style = match item.status {
        Status::Pending => dim,
        _ => colour(item_colour(item, live)),
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
    spans.extend(worker_spans(item));
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

/// `  Atlas @general #3 · zen/glm · high`: who worked on the item and on
/// what, each part only when its file has it (an older one names no
/// subagent and no model). In the subagent's and the model's colours while
/// the item is in play, all dim once it is done or back to pending, so
/// finished work recedes.
fn worker_spans(item: &Item) -> Vec<Span<'static>> {
    let dim = theme::dim();
    let subagent = Style::new().fg(theme::SUBAGENT_COLOUR);
    let who = [
        item.name.clone().map(|name| (name, subagent)),
        item.agent
            .as_ref()
            .map(|agent| (format!("@{agent}"), subagent)),
        item.task.map(|task| (format!("#{task}"), dim)),
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (text, style) in who.into_iter().flatten() {
        let lead = if spans.is_empty() { "  " } else { " " };
        spans.push(Span::styled(format!("{lead}{text}"), style));
    }
    if let Some(model) = &item.model {
        let lead = if spans.is_empty() { "  " } else { SEPARATOR };
        spans.push(Span::styled(lead, dim));
        spans.extend(theme::model_spans(model, item.effort.as_deref()));
    }
    let in_play = matches!(
        item.status,
        Status::Running | Status::Returned | Status::Failed
    );
    if !in_play {
        for span in &mut spans {
            span.style = dim;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use nth_work::{Cycle, ItemRef, Todo, WorkOutcome};
    use ratatui::style::Modifier;

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
                    items: vec![Item {
                        agent: Some("general".into()),
                        task: Some(2),
                        name: Some("Atlas".into()),
                        model: Some("zen/glm".into()),
                        effort: Some("high".into()),
                        ..item("01-scaffold", "trait", Status::Done, &[("a", true)])
                    }],
                },
                Cycle {
                    number: 2,
                    slug: "build".into(),
                    items: vec![
                        Item {
                            agent: Some("general".into()),
                            task: Some(3),
                            name: Some("Borg".into()),
                            model: Some("chatgpt/gpt-5".into()),
                            ..item(
                                "02-build",
                                "http-retry",
                                Status::Running,
                                &[("wrap idempotent requests", true), ("cap", false)],
                            )
                        },
                        Item {
                            // An older file: no name and no model.
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
                    items: vec![Item {
                        // Sent back to pending: who had it stays.
                        agent: Some("general".into()),
                        task: Some(5),
                        name: Some("Cleo".into()),
                        model: Some("zen/glm".into()),
                        ..item("03-verify", "test", Status::Pending, &[("run", false)])
                    }],
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
            3,
            LiveTask {
                since: None,
                tool_calls: 0,
            },
        )]);

        let lines = view.lines(".nth/work/s1", 40, &live);

        assert_eq!(
            text(&lines),
            [
                "▎ .nth/work/s1 · 1/4 done · 1 running · 1 returned",
                "▎ zen/glm 2 · chatgpt/gpt-5 1",
                "▎ ",
                "▎ Retry",
                "▎ Add retries with jitter to every",
                "▎ client call.",
                "",
                "▎ ✓ 1 scaffold  1/1",
                "▎   ✓ trait  1/1  Atlas @general #2 ·",
                "▎     zen/glm · high",
                "",
                "▎ → 2 build  0/2",
                "▎   ● http-retry  1/2  Borg @general #3",
                "▎     · chatgpt/gpt-5",
                "▎     ☒ wrap idempotent requests",
                "▎     ☐ cap",
                "▎   ∎ docs  1/1  @explore #4 · returned",
                "▎     completed",
                "▎     ☒ write",
                "",
                "▎ ● 3 verify  0/1",
                "▎   ● test  0/1  Cleo @general #5 ·",
                "▎     zen/glm",
            ]
        );
        assert_eq!(view.label(), "work 1/4");
        assert_eq!(view.state(&live), TabState::Working);

        // Each row's bar is the state of what it belongs to.
        let bar = |row: usize| lines[row].spans[0].style.fg;
        let grey = Some(theme::SECTION_COLOUR);
        assert_eq!(bar(0), grey, "the header");
        assert_eq!(bar(1), grey, "and its models");
        assert_eq!(bar(7), Some(Color::Green), "a closed cycle");
        assert_eq!(bar(8), Some(Color::Green), "a done item");
        assert_eq!(bar(9), Some(Color::Green), "and its wrapped rest");
        assert_eq!(bar(11), Some(Color::Blue), "the current cycle");
        assert_eq!(bar(12), Some(Color::Yellow), "a running item");
        assert_eq!(bar(15), Some(Color::Yellow), "and its todos");
        assert_eq!(bar(16), Some(Color::Magenta), "a returned item");
        assert_eq!(bar(20), grey, "an upcoming cycle");
        assert_eq!(bar(21), grey, "a pending item");
    }

    /// The span of row `row` that reads `text`.
    fn span<'a>(lines: &'a [Line<'static>], row: usize, text: &str) -> &'a Span<'static> {
        lines[row]
            .spans
            .iter()
            .find(|span| span.content.trim() == text)
            .unwrap_or_else(|| panic!("no {text:?} in {:?}", lines[row]))
    }

    #[test]
    fn every_item_names_who_worked_on_it_and_on_what() {
        let mut view = WorkView::default();
        view.update(Some(plan()));
        let live = Live::from([(
            3,
            LiveTask {
                since: None,
                tool_calls: 0,
            },
        )]);

        let lines = view.lines("w", 80, &live);
        let rows = text(&lines);

        assert_eq!(
            rows[7],
            "▎   ✓ trait  1/1  Atlas @general #2 · zen/glm · high"
        );
        assert_eq!(
            rows[10],
            "▎   ● http-retry  1/2  Borg @general #3 · chatgpt/gpt-5"
        );
        assert_eq!(
            rows[13], "▎   ∎ docs  1/1  @explore #4 · returned completed",
            "a file from before names and models were kept"
        );
        assert_eq!(rows[17], "▎   ● test  0/1  Cleo @general #5 · zen/glm");

        // In play, the subagent and the model in their own colours.
        let fg = |row: usize, text: &str| span(&lines, row, text).style.fg;
        assert_eq!(fg(10, "Borg"), Some(theme::SUBAGENT_COLOUR));
        assert_eq!(fg(10, "@general"), Some(theme::SUBAGENT_COLOUR));
        assert_eq!(fg(10, "chatgpt/"), Some(theme::PROVIDER_COLOUR));
        assert_eq!(fg(10, "gpt-5"), Some(Color::Blue));
        // Failed is in play too: the model looks at it next.
        let mut failed = plan();
        failed.cycles[1].items[0].status = Status::Failed;
        view.update(Some(failed));
        let failed = view.lines("w", 80, &live);
        assert_eq!(
            text(&failed)[10],
            "▎   ✗ http-retry  1/2  Borg @general #3 · chatgpt/gpt-5 · failed"
        );
        assert_eq!(
            span(&failed, 10, "Borg").style.fg,
            Some(theme::SUBAGENT_COLOUR)
        );
        assert_eq!(span(&failed, 10, "gpt-5").style.fg, Some(Color::Blue));
        // Done or back to pending, all of it dim.
        for (row, parts) in [
            (7, &["Atlas", "@general", "#2", "zen/", "glm", "· high"][..]),
            (17, &["Cleo", "@general", "#5", "zen/", "glm"][..]),
        ] {
            for part in parts {
                assert_eq!(span(&lines, row, part).style, theme::dim(), "{part}");
            }
        }
    }

    #[test]
    fn the_header_counts_the_items_of_each_model() {
        let mut view = WorkView::default();
        view.update(Some(plan()));

        let lines = view.lines("w", 80, &Live::new());

        assert_eq!(text(&lines)[1], "▎ zen/glm 2 · chatgpt/gpt-5 1");
        assert_eq!(
            span(&lines, 1, "zen/").style.fg,
            Some(theme::PROVIDER_COLOUR)
        );
        assert_eq!(span(&lines, 1, "glm").style.fg, Some(Color::Blue));
        assert_eq!(span(&lines, 1, "2").style, theme::dim());

        // The row is 27 columns behind the 2 of its bar: it stays whole
        // where it just fits, and one column narrower a model moves down
        // with its count.
        let fits = text(&view.lines("w", 29, &Live::new()));
        assert_eq!(fits[1..3], ["▎ zen/glm 2 · chatgpt/gpt-5 1", "▎ "]);
        let narrow = text(&view.lines("w", 28, &Live::new()));
        assert_eq!(narrow[1..3], ["▎ zen/glm 2", "▎ chatgpt/gpt-5 1"]);

        // No item with a model, no row.
        let mut plan = plan();
        for item in plan.cycles.iter_mut().flat_map(|c| &mut c.items) {
            item.model = None;
        }
        view.update(Some(plan));
        assert_eq!(text(&view.lines("w", 80, &Live::new()))[1], "▎ ");
    }

    #[test]
    fn the_overview_renders_plan_md_as_markdown() {
        let mut view = WorkView::default();
        let mut plan = plan();
        plan.goal = Some("- step one\n- **step two**".into());
        view.update(Some(plan));

        let lines = view.lines("w", 40, &Live::new());
        let text = text(&lines);

        assert_eq!(text[3], "▎ Retry", "the title as a heading");
        assert!(
            lines[3].spans[1..]
                .iter()
                .filter(|s| !s.content.is_empty())
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert_eq!(text[4], "▎ • step one");
        assert_eq!(text[5], "▎ • step two", "a list, its bold rendered");
        assert_eq!(text[6], "", "a blank before the first cycle");
    }

    #[test]
    fn a_running_item_nobody_here_works_on_is_lost() {
        let mut view = WorkView::default();
        view.update(Some(plan()));
        let other = Live::from([(
            7,
            LiveTask {
                since: None,
                tool_calls: 0,
            },
        )]);

        let lines = text(&view.lines("w", 80, &other));

        assert_eq!(lines[0], "▎ w · 1/4 done · 1 returned · 1 lost");
        assert_eq!(
            lines[10],
            "▎   ⏹ http-retry  1/2  Borg @general #3 · chatgpt/gpt-5 · lost"
        );
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
