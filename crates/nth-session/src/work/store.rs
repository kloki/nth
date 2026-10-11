//! The work plan on disk: `<cwd>/.nth/work/<session>/`, a numbered folder
//! per cycle (`01-foundation/`) and one markdown file per item in it, its
//! state in the frontmatter and its brief, todo and result in the body.
//! Only this module writes item files; it reads whatever is there,
//! hand-edited or not.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use nth_context::frontmatter;
use nth_protocol::{TaskId, Todo, WorkOutcome, one_line, todo_counts, todo_lines};

const TODO: &str = "## Todo";
const RESULT: &str = "## Result";

/// Where the session `session` keeps its work plan.
pub fn work_dir(cwd: &Path, session: &str) -> PathBuf {
    cwd.join(".nth").join("work").join(session)
}

/// Where an item stands. Only the orchestrator closes one, as done or
/// failed; a subagent's return leaves it for the orchestrator to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Returned,
    Done,
    Failed,
    /// A file that does not read as an item, and why: shown as it is, for
    /// the orchestrator to write again.
    Broken(String),
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Returned => "returned",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Broken(_) => "broken",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "pending" => Status::Pending,
            "running" => Status::Running,
            "returned" => Status::Returned,
            "done" => Status::Done,
            "failed" => Status::Failed,
            _ => return None,
        })
    }

    /// Not yet closed by the orchestrator, which keeps its cycle open.
    pub fn is_open(&self) -> bool {
        !matches!(self, Status::Done | Status::Failed)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which item: its cycle's folder and its slug, `01-foundation/store`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemRef {
    pub cycle: String,
    pub item: String,
}

impl ItemRef {
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.cycle).join(format!("{}.md", self.item))
    }
}

impl fmt::Display for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.cycle, self.item)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub at: ItemRef,
    pub title: String,
    pub status: Status,
    /// Who it was last assigned to, and as which subagent of the process
    /// that did: ids start again with every run of nth.
    pub agent: Option<String>,
    pub task: Option<TaskId>,
    pub assigned: Option<jiff::Timestamp>,
    /// How its subagent's last turn ended, its text being `result`.
    pub outcome: Option<WorkOutcome>,
    /// The orchestrator's word when it closed the item.
    pub note: Option<String>,
    /// The brief, markdown.
    pub instruction: String,
    pub todos: Vec<Todo>,
    /// The subagent's last answer or error.
    pub result: Option<String>,
    /// When the file last changed: the item's last activity.
    pub modified: Option<SystemTime>,
}

impl Item {
    /// A new item, nobody on it yet.
    pub fn pending(at: ItemRef, title: &str, instruction: &str, todos: Vec<Todo>) -> Self {
        Self {
            at,
            title: one_line(title),
            status: Status::Pending,
            agent: None,
            task: None,
            assigned: None,
            outcome: None,
            note: None,
            instruction: instruction.trim().to_string(),
            todos,
            result: None,
            modified: None,
        }
    }

    /// Ticked steps and all of them.
    pub fn progress(&self) -> (usize, usize) {
        todo_counts(&self.todos)
    }

    /// The item as its subagent reads it: title, brief, todo and the last
    /// result, the frontmatter left out.
    pub fn body(&self) -> String {
        let mut out = format!("# {}\n", self.title);
        if !self.instruction.is_empty() {
            out.push_str(&format!("\n{}\n", self.instruction));
        }
        out.push_str(&format!("\n{TODO}\n\n"));
        if !self.todos.is_empty() {
            out.push_str(&todo_lines(&self.todos));
            out.push('\n');
        }
        if let Some(result) = &self.result {
            out.push_str(&format!("\n{RESULT}\n\n{}\n", result.trim()));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub number: u32,
    pub slug: String,
    pub items: Vec<Item>,
}

impl Cycle {
    /// Its folder's name, `01-foundation`.
    pub fn name(&self) -> String {
        cycle_name(self.number, &self.slug)
    }

    /// Whether an item in it is not yet closed.
    pub fn is_open(&self) -> bool {
        self.items.iter().any(|item| item.status.is_open())
    }
}

/// A cycle's folder name.
pub fn cycle_name(number: u32, slug: &str) -> String {
    format!("{number:02}-{slug}")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The first heading of `plan.md`, which the orchestrator writes itself.
    pub title: Option<String>,
    /// The rest of `plan.md`.
    pub goal: Option<String>,
    /// By number.
    pub cycles: Vec<Cycle>,
    /// Folders in the work directory that are not a cycle: not named
    /// `NN-slug`, or a second folder for a cycle's number.
    pub ignored: Vec<String>,
}

impl Plan {
    /// The serial guard: the first cycle with an item not yet closed. Its
    /// items are the ones that may be assigned.
    pub fn first_open_cycle(&self) -> Option<&Cycle> {
        self.cycles.iter().find(|cycle| cycle.is_open())
    }

    pub fn cycle(&self, number: u32) -> Option<&Cycle> {
        self.cycles.iter().find(|cycle| cycle.number == number)
    }

    pub fn item(&self, at: &ItemRef) -> Option<&Item> {
        self.cycles
            .iter()
            .filter(|cycle| cycle.name() == at.cycle)
            .flat_map(|cycle| &cycle.items)
            .find(|item| item.at == *at)
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.cycles.iter().flat_map(|cycle| &cycle.items)
    }
}

/// A session's work directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The whole plan; `None` when the session never wrote one.
    pub async fn load(&self) -> io::Result<Option<Plan>> {
        let dir = self.dir.clone();
        // One hop to the blocking pool for the whole walk, not one per file.
        tokio::task::spawn_blocking(move || load(&dir))
            .await
            .map_err(io::Error::other)?
    }

    /// One item, as `load` reads it; `None` when there is no such file.
    pub async fn read(&self, at: &ItemRef) -> io::Result<Option<Item>> {
        let path = at.path(&self.dir);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let modified = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .ok();
        Ok(Some(item(at.clone(), &text, modified)))
    }

    /// Writes `item`, its cycle's folder too if it is new. Written aside
    /// and renamed over, so a reader never sees half a file.
    pub async fn write(&self, item: &Item) -> io::Result<()> {
        let path = item.at.path(&self.dir);
        if let Some(folder) = path.parent() {
            tokio::fs::create_dir_all(folder).await?;
        }
        // Its own name per write: a write cancelled mid-way still lands,
        // and must not land in the file another write is about to rename.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("md.{}.{n}.tmp", std::process::id()));
        tokio::fs::write(&tmp, render(item)).await?;
        tokio::fs::rename(&tmp, &path).await
    }
}

fn load(dir: &Path) -> io::Result<Option<Plan>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut plan = Plan::default();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        // The tools find a cycle's items by `cycle_name`, so a folder named
        // otherwise (`1-a`) would hold items nobody can reach.
        let Some((number, slug)) = parse_cycle(&name).filter(|(n, s)| cycle_name(*n, s) == name)
        else {
            plan.ignored.push(name);
            continue;
        };
        let mut items = Vec::new();
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            let file_name = file.file_name().to_string_lossy().into_owned();
            let Some(slug) = file_name.strip_suffix(".md") else {
                continue;
            };
            let at = ItemRef {
                cycle: name.clone(),
                item: slug.to_string(),
            };
            let modified = file.metadata().and_then(|m| m.modified()).ok();
            let item = match fs::read_to_string(file.path()) {
                Ok(text) => item(at, &text, modified),
                Err(e) => broken(at, e.to_string()),
            };
            items.push(item);
        }
        items.sort_by(|a, b| a.at.item.cmp(&b.at.item));
        plan.cycles.push(Cycle {
            number,
            slug: slug.to_string(),
            items,
        });
    }
    plan.cycles
        .sort_by(|a, b| (a.number, &a.slug).cmp(&(b.number, &b.slug)));
    // One folder a number: the tools reach the first only, and the rest
    // would hold every later cycle up for good.
    let mut seen = std::collections::BTreeSet::new();
    let (cycles, doubles): (Vec<_>, Vec<_>) = std::mem::take(&mut plan.cycles)
        .into_iter()
        .partition(|cycle| seen.insert(cycle.number));
    plan.cycles = cycles;
    plan.ignored.extend(doubles.iter().map(Cycle::name));
    plan.ignored.sort();
    match fs::read_to_string(dir.join("plan.md")) {
        Ok(text) => (plan.title, plan.goal) = overview(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(Some(plan))
}

/// `01-foundation` as its number and slug.
fn parse_cycle(name: &str) -> Option<(u32, &str)> {
    let (number, slug) = name.split_once('-')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    super::slug(slug).ok()?;
    Some((number.parse().ok()?, slug))
}

/// `plan.md`'s first heading, and the rest.
fn overview(text: &str) -> (Option<String>, Option<String>) {
    let text = text.trim();
    let (title, goal) = match text.strip_prefix("# ") {
        Some(rest) => match rest.split_once('\n') {
            Some((title, goal)) => (Some(title.trim().to_string()), goal),
            None => (Some(rest.trim().to_string()), ""),
        },
        None => (None, text),
    };
    let goal = goal.trim();
    (title, (!goal.is_empty()).then(|| goal.to_string()))
}

fn item(at: ItemRef, text: &str, modified: Option<SystemTime>) -> Item {
    match parse(at.clone(), text) {
        Ok(item) => Item { modified, ..item },
        Err(why) => Item {
            modified,
            ..broken(at, why)
        },
    }
}

fn broken(at: ItemRef, why: String) -> Item {
    let title = at.item.clone();
    Item {
        status: Status::Broken(why),
        ..Item::pending(at, &title, "", Vec::new())
    }
}

/// An item file as the store writes it.
pub fn render(item: &Item) -> String {
    let mut out = format!("---\nstatus: {}\n", item.status.name());
    if let Some(agent) = &item.agent {
        out.push_str(&format!("agent: {}\n", quoted(agent)));
    }
    if let Some(task) = item.task {
        out.push_str(&format!("task: {task}\n"));
    }
    if let Some(assigned) = item.assigned {
        out.push_str(&format!("assigned: {}\n", quoted(&assigned.to_string())));
    }
    if let Some(outcome) = &item.outcome {
        out.push_str(&format!("outcome: {}\n", outcome.name()));
    }
    if let Some(note) = &item.note {
        out.push_str(&format!("note: {}\n", quoted(&one_line(note))));
    }
    out.push_str("---\n\n");
    out.push_str(&item.body());
    out
}

/// A YAML double-quoted string, which takes any text on one line.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An item file. The outcome's text is the result's.
pub fn parse(at: ItemRef, text: &str) -> Result<Item, String> {
    let (front, body) = frontmatter::parse(text)?;
    let status = frontmatter::field(&front, "status").ok_or("no status in the frontmatter")?;
    let status = Status::parse(&status).ok_or_else(|| format!("unknown status {status:?}"))?;
    let assigned = match frontmatter::field(&front, "assigned") {
        Some(at) => Some(
            at.parse::<jiff::Timestamp>()
                .map_err(|e| format!("assigned {at:?} is not a time: {e}"))?,
        ),
        None => None,
    };
    // A hand edit may quote it.
    let task = frontmatter::number(&front, "task")
        .or_else(|| frontmatter::field(&front, "task")?.parse().ok());
    let task = match task {
        Some(task) => Some(TaskId::try_from(task).map_err(|_| format!("task {task} is too big"))?),
        None => None,
    };
    let sections = sections(body)?;
    let outcome = match frontmatter::field(&front, "outcome") {
        Some(name) => Some(
            with_text(&name, sections.result.as_deref().unwrap_or_default())
                .ok_or_else(|| format!("unknown outcome {name:?}"))?,
        ),
        None => None,
    };
    Ok(Item {
        at,
        title: sections.title,
        status,
        agent: frontmatter::field(&front, "agent"),
        task,
        assigned,
        outcome,
        note: frontmatter::field(&front, "note"),
        instruction: sections.instruction,
        todos: sections.todos,
        result: sections.result,
        modified: None,
    })
}

/// The outcome named `name`, carrying `text`.
fn with_text(name: &str, text: &str) -> Option<WorkOutcome> {
    Some(match WorkOutcome::parse(name)? {
        WorkOutcome::Completed(_) => WorkOutcome::Completed(text.to_string()),
        WorkOutcome::Failed(_) => WorkOutcome::Failed(text.to_string()),
        WorkOutcome::TimedOut(_) => WorkOutcome::TimedOut(text.to_string()),
        WorkOutcome::Interrupted => WorkOutcome::Interrupted,
    })
}

struct Sections {
    title: String,
    instruction: String,
    todos: Vec<Todo>,
    result: Option<String>,
}

/// The body: a `# title` line, the instruction, then `## Todo` and
/// `## Result`.
fn sections(body: &str) -> Result<Sections, String> {
    let mut lines = body.lines().skip_while(|line| line.trim().is_empty());
    let title = lines
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .ok_or("the body does not start with a # title")?
        .trim()
        .to_string();
    enum Part {
        Instruction,
        Todo,
        Result,
    }
    let mut part = Part::Instruction;
    let (mut instruction, mut result) = (Vec::new(), Vec::new());
    let mut todos = Vec::new();
    for line in lines {
        match (&part, line.trim_end()) {
            // The result is last and the subagent's own words, so a heading
            // in it is just text.
            (Part::Result, _) => result.push(line),
            (_, TODO) => part = Part::Todo,
            (_, RESULT) => part = Part::Result,
            (Part::Instruction, line) => instruction.push(line),
            (Part::Todo, line) => todos.extend(todo(line)),
        }
    }
    let result = result.join("\n").trim().to_string();
    Ok(Sections {
        title,
        instruction: instruction.join("\n").trim().to_string(),
        todos,
        result: (!result.is_empty()).then_some(result),
    })
}

/// A `- [ ] step` or `- [x] step` line.
fn todo(line: &str) -> Option<Todo> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let (mark, text) = rest.split_once("] ")?;
    let done = match mark {
        " " => false,
        "x" | "X" => true,
        _ => return None,
    };
    Some(Todo {
        text: text.trim().to_string(),
        done,
    })
}

/// Whether `instruction` has a line the body's sections would split at.
pub fn splits_sections(instruction: &str) -> bool {
    instruction
        .lines()
        .any(|line| matches!(line.trim_end(), TODO | RESULT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(cycle: &str, item: &str) -> ItemRef {
        ItemRef {
            cycle: cycle.into(),
            item: item.into(),
        }
    }

    fn todos(steps: &[(&str, bool)]) -> Vec<Todo> {
        steps
            .iter()
            .map(|(text, done)| Todo {
                text: (*text).into(),
                done: *done,
            })
            .collect()
    }

    #[test]
    fn an_item_reads_back_as_it_was_written() {
        let pending = Item::pending(
            at("01-foundation", "store"),
            "Store round-trip",
            "Read and write item files.\n\nUse `tokio::fs`.",
            todos(&[("parse", false), ("write", false)]),
        );
        let text = render(&pending);
        assert_eq!(
            text,
            "---\nstatus: pending\n---\n\n# Store round-trip\n\nRead and write item files.\n\nUse `tokio::fs`.\n\n\
             ## Todo\n\n- [ ] parse\n- [ ] write\n"
        );
        assert_eq!(parse(pending.at.clone(), &text), Ok(pending.clone()));

        let returned = Item {
            status: Status::Returned,
            agent: Some("general".into()),
            task: Some(3),
            assigned: Some("2026-10-11T12:02:11Z".parse().unwrap()),
            outcome: Some(WorkOutcome::Completed("Done: \"it\" works.".into())),
            note: Some("merged: in \"3b2c\"".into()),
            todos: todos(&[("parse", true), ("write", false)]),
            result: Some("Done: \"it\" works.".into()),
            ..pending
        };
        let text = render(&returned);
        assert!(
            text.starts_with("---\nstatus: returned\nagent: \"general\"\ntask: 3\nassigned: \"2026-10-11T12:02:11Z\"\noutcome: completed\nnote: \"merged: in \\\"3b2c\\\"\"\n---\n"),
            "{text}"
        );
        assert!(
            text.ends_with("- [x] parse\n- [ ] write\n\n## Result\n\nDone: \"it\" works.\n"),
            "{text}"
        );
        assert_eq!(parse(returned.at.clone(), &text), Ok(returned));
    }

    #[test]
    fn every_status_and_outcome_reads_back() {
        for status in [
            Status::Pending,
            Status::Running,
            Status::Returned,
            Status::Done,
            Status::Failed,
        ] {
            let item = Item {
                status: status.clone(),
                ..Item::pending(at("01-a", "x"), "X", "", Vec::new())
            };
            assert_eq!(
                parse(at("01-a", "x"), &render(&item)).map(|i| i.status),
                Ok(status)
            );
        }
        for outcome in [
            WorkOutcome::Failed("boom".into()),
            WorkOutcome::TimedOut("no answer after 1s".into()),
            WorkOutcome::Interrupted,
        ] {
            let item = Item {
                status: Status::Returned,
                result: Some(outcome.text().to_string()).filter(|t| !t.is_empty()),
                outcome: Some(outcome.clone()),
                ..Item::pending(at("01-a", "x"), "X", "", Vec::new())
            };
            assert_eq!(
                parse(at("01-a", "x"), &render(&item)).map(|i| i.outcome),
                Ok(Some(outcome))
            );
        }
    }

    #[test]
    fn hand_written_todos_and_sections_are_read_leniently() {
        let text = "---\nstatus: running\ntask: 2\n---\n# Tidy\n\nDo it.\n\n## Todo\n- [X] one\n\n- [ ] two\nnot a step\n";

        let item = parse(at("01-a", "tidy"), text).unwrap();

        assert_eq!(item.todos, todos(&[("one", true), ("two", false)]));
        assert_eq!(item.instruction, "Do it.");
        assert_eq!(item.task, Some(2));
        assert_eq!(item.result, None);
        assert!(splits_sections("a\n## Todo\nb"));
        assert!(!splits_sections("a\n### Todo\nb"));
    }

    #[test]
    fn a_result_keeps_its_own_headings() {
        let item = Item {
            status: Status::Returned,
            todos: todos(&[("one", true)]),
            result: Some(
                "Done. Follow-ups:\n\n## Todo\n\n- [ ] add tests\n\n## Result\n\nThat is all."
                    .into(),
            ),
            ..Item::pending(at("01-a", "x"), "X", "", Vec::new())
        };

        let back = parse(item.at.clone(), &render(&item)).unwrap();

        assert_eq!(back.todos, item.todos);
        assert_eq!(back.result, item.result);
        let quoted = parse(
            at("01-a", "x"),
            "---\nstatus: running\ntask: \"3\"\n---\n# X\n",
        )
        .unwrap();
        assert_eq!(quoted.task, Some(3));
    }

    #[test]
    fn a_file_that_is_not_an_item_says_why() {
        assert_eq!(
            parse(at("01-a", "x"), "---\nstatus: sleeping\n---\n# X\n"),
            Err("unknown status \"sleeping\"".into())
        );
        assert!(parse(at("01-a", "x"), "---\nstatus: done\n---\nno title").is_err());
        assert!(parse(at("01-a", "x"), "# X\n").is_err(), "no status");
    }

    #[tokio::test]
    async fn the_plan_loads_by_cycle_number_and_keeps_broken_items() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("work"));
        assert_eq!(store.load().await.unwrap(), None, "no plan yet");

        let b = Item::pending(at("02-b", "late"), "Late", "", Vec::new());
        let a = Item {
            status: Status::Done,
            ..Item::pending(at("01-a", "first"), "First", "", Vec::new())
        };
        store.write(&b).await.unwrap();
        store.write(&a).await.unwrap();
        std::fs::write(store.dir().join("01-a/notes.txt"), "stray").unwrap();
        std::fs::write(store.dir().join("01-a/bad.md"), "garbage").unwrap();
        std::fs::create_dir(store.dir().join("misc")).unwrap();
        std::fs::write(store.dir().join("plan.md"), "# Retry\n\nAdd retries.\n").unwrap();

        let plan = store.load().await.unwrap().unwrap();

        assert_eq!(plan.title.as_deref(), Some("Retry"));
        assert_eq!(plan.goal.as_deref(), Some("Add retries."));
        let names: Vec<_> = plan.cycles.iter().map(Cycle::name).collect();
        assert_eq!(names, ["01-a", "02-b"]);
        let first: Vec<_> = plan.cycles[0]
            .items
            .iter()
            .map(|i| i.at.item.as_str())
            .collect();
        assert_eq!(first, ["bad", "first"]);
        assert!(matches!(plan.cycles[0].items[0].status, Status::Broken(_)));
        assert!(plan.cycles[0].items[1].modified.is_some());
        assert_eq!(
            plan.first_open_cycle().map(Cycle::name).as_deref(),
            Some("01-a"),
            "a broken item keeps its cycle open"
        );
        std::fs::remove_file(store.dir().join("01-a/bad.md")).unwrap();
        let plan = store.load().await.unwrap().unwrap();
        assert_eq!(
            plan.first_open_cycle().map(Cycle::name).as_deref(),
            Some("02-b")
        );
        assert_eq!(plan.ignored, ["misc"]);
        std::fs::create_dir(store.dir().join("1-x")).unwrap();
        std::fs::create_dir(store.dir().join("02-a")).unwrap();
        let reloaded = store.load().await.unwrap().unwrap();
        assert_eq!(
            reloaded.ignored,
            ["02-b", "1-x", "misc"],
            "02-a sorts first"
        );
        assert_eq!(
            store
                .read(&at("02-b", "late"))
                .await
                .unwrap()
                .map(|i| i.title),
            Some("Late".into())
        );
        assert_eq!(store.read(&at("02-b", "gone")).await.unwrap(), None);
    }
}
