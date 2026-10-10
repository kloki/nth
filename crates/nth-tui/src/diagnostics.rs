//! The diagnostics tab: nth's own state, for when something does not work
//! as expected. The model each mode runs with and the turns run on each
//! model, the instruction files, skills and agents found for the project,
//! and the language servers and formatters that check writes.

use nth_context::Context;
use nth_format::FormatterStatus;
use nth_icons::icons;
use nth_lsp::{ServerInfo, ServerState, ServerStatus};
use nth_protocol::{Failed, Mode};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, ScrollbarState},
};

use crate::{status, theme};

/// What the tab knows beyond what the app shows elsewhere: which servers
/// and formatters apply, looked up each time the tab opens.
#[derive(Debug, Default)]
pub struct Diagnostics {
    /// `None` while being looked up.
    pub servers: Option<Vec<ServerInfo>>,
    pub formatters: Option<Vec<FormatterStatus>>,
    /// The first line in view, and how far it can go, as of the last draw.
    top: usize,
    max_top: usize,
    height: usize,
}

/// The model a mode runs with.
pub struct ModeModel {
    pub mode: Mode,
    pub model: String,
    pub effort: Option<&'static str>,
    pub context_window: Option<u64>,
    /// Whether the next turn runs in this mode.
    pub current: bool,
}

/// Everything the tab shows that lives elsewhere on the app.
pub struct Facts<'a> {
    /// The model each mode runs with, plan first.
    pub modes: [ModeModel; 2],
    /// How many LLMs the provider lists, once listed.
    pub llms: Option<usize>,
    /// The providers that could not be asked for theirs.
    pub failed: &'a [Failed],
    pub listing: bool,
    /// Whether the app checks writes at all; a bare app has no servers or
    /// formatters to look up.
    pub checks: bool,
    pub running: &'a [ServerStatus],
    pub context: &'a Context,
    pub home: Option<&'a str>,
    /// Turns run on each model, most first.
    pub usage: Vec<(&'a str, u64)>,
}

impl Diagnostics {
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

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, facts: &Facts) {
        let lines = self.lines(facts);
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = self.top.min(self.max_top);
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    fn lines(&self, facts: &Facts) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        model(&mut lines, facts);
        lines.push(Line::default());
        usage(&mut lines, facts);
        lines.push(Line::default());
        context(&mut lines, facts);
        lines.push(Line::default());
        self.servers(&mut lines, facts);
        lines.push(Line::default());
        self.formatters(&mut lines, facts);
        lines
    }

    /// Every server nth knows, the ones that can run here first. A server
    /// the tools started shows its state's dot in place of the tick.
    fn servers(&self, lines: &mut Vec<Line<'static>>, facts: &Facts) {
        lines.push(title("language servers"));
        let Some(servers) = checked(lines, facts.checks, self.servers.as_ref()) else {
            return;
        };
        let mut servers: Vec<&ServerInfo> = servers.iter().collect();
        servers.sort_by_key(|s| s.program.is_none());
        let width = servers.iter().map(|s| s.id.len()).max().unwrap_or(0);
        for server in servers {
            let id = format!("{:width$}", server.id);
            let Some(program) = &server.program else {
                lines.push(unavailable(id, "not on PATH".into()));
                continue;
            };
            let root = match &server.root {
                Some(root) => format!("{} {}", icons().to, path(root, facts.home)),
                None => "no project root here".into(),
            };
            let about = format!("{}  {root}", path(program, facts.home));
            let running = facts.running.iter().find(|s| s.id == server.id);
            let mut line = match running {
                Some(status) => row(
                    Span::styled(
                        icons().dot,
                        Style::new().fg(status::state_colour(&status.state)),
                    ),
                    id,
                    about,
                ),
                None => row(tick(), id, about),
            };
            line.spans[2].style = Style::new().fg(Color::Cyan);
            if let Some(ServerStatus {
                state: ServerState::Broken(reason),
                ..
            }) = running
            {
                line.spans.push(Span::styled(
                    format!("  {reason}"),
                    Style::new().fg(Color::Red),
                ));
            }
            lines.push(line);
        }
    }

    /// Every formatter nth knows, the ones that run here first.
    fn formatters(&self, lines: &mut Vec<Line<'static>>, facts: &Facts) {
        lines.push(title("formatters"));
        let Some(formatters) = checked(lines, facts.checks, self.formatters.as_ref()) else {
            return;
        };
        let mut formatters: Vec<&FormatterStatus> = formatters.iter().collect();
        formatters.sort_by_key(|f| f.command.is_err());
        let width = formatters.iter().map(|f| f.name.len()).max().unwrap_or(0);
        for formatter in formatters {
            let name = format!("{:width$}", formatter.name);
            lines.push(match &formatter.command {
                Ok(command) => {
                    let mut line = row(tick(), name, command.join(" "));
                    line.spans[2].style = Style::new().fg(Color::Cyan);
                    line
                }
                Err(reason) => unavailable(name, reason.clone()),
            });
        }
    }
}

/// A row per mode, the current one marked.
fn model(lines: &mut Vec<Line<'static>>, facts: &Facts) {
    lines.push(title("model"));
    let width = Mode::ALL
        .map(|m| m.label().len())
        .into_iter()
        .max()
        .unwrap_or(0);
    for mode in &facts.modes {
        let mut about = vec![mode.model.clone()];
        about.extend(mode.effort.map(String::from));
        if let Some(window) = mode.context_window {
            about.push(format!("{}k context", window / 1000));
        }
        let (mark, label) = if mode.current {
            (icons().current, Style::new())
        } else {
            (" ", theme::dim())
        };
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::raw(mark),
            Span::styled(format!(" {:width$}  ", mode.mode.label()), label),
            Span::styled(about.join(" · "), Style::new().fg(Color::Blue)),
        ]));
    }
    let listed = match facts.llms {
        _ if facts.listing => "listing models…".to_string(),
        Some(1) => "1 model served".to_string(),
        Some(n) => format!("{n} models served"),
        None => "models not listed".to_string(),
    };
    lines.push(note(listed));
    for failed in facts.failed {
        lines.push(unavailable(
            failed.origin.name.clone(),
            failed.error.clone(),
        ));
    }
}

/// Cells the busiest model's bar fills.
const BAR_WIDTH: u64 = 30;

/// A bar per model, scaled to the busiest one, then its turns.
fn usage(lines: &mut Vec<Line<'static>>, facts: &Facts) {
    lines.push(title("model usage"));
    let Some(most) = facts.usage.iter().map(|(_, turns)| *turns).max() else {
        lines.push(note("no turns yet".into()));
        return;
    };
    let width = facts.usage.iter().map(|(m, _)| m.len()).max().unwrap_or(0);
    let digits = most.to_string().len();
    for (model, turns) in &facts.usage {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::raw(format!("{model:width$}  ")),
            Span::styled(
                format!("{:bar$}", bar(*turns, most), bar = BAR_WIDTH as usize),
                Style::new().fg(Color::Blue),
            ),
            Span::styled(format!("  {turns:>digits$}"), theme::dim()),
        ]));
    }
}

/// `turns` of `most` in eighths of a cell, at least one so every used
/// model shows.
fn bar(turns: u64, most: u64) -> String {
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (turns * BAR_WIDTH * 8 / most.max(1)).max(1);
    let mut bar = "█".repeat((eighths / 8) as usize);
    let rest = (eighths % 8) as usize;
    if rest > 0 {
        bar.push(EIGHTHS[rest]);
    }
    bar
}

/// The instruction files in the system prompt, the skills and agents
/// found, and what went wrong finding them.
fn context(lines: &mut Vec<Line<'static>>, facts: &Facts) {
    lines.push(title("instructions"));
    if facts.context.instructions.is_empty() {
        lines.push(note("none found".into()));
    }
    for instruction in &facts.context.instructions {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::raw(path(&instruction.path, facts.home)),
        ]));
    }
    lines.push(Line::default());
    lines.push(title("skills"));
    if facts.context.skills.is_empty() {
        lines.push(note("none found".into()));
    }
    let width = facts
        .context
        .skills
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);
    for skill in facts.context.skills.iter() {
        let mut line = row(
            Span::raw(icons().tool.skill),
            format!("{:width$}", skill.name),
            skill.source.name().into(),
        );
        line.spans[2].style = Style::new().fg(Color::Cyan);
        lines.push(line);
    }
    lines.push(Line::default());
    lines.push(title("agents"));
    let width = facts
        .context
        .agents
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    for agent in facts.context.agents.iter() {
        let mut line = row(
            Span::raw(icons().subagent),
            format!("{:width$}", agent.name),
            agent.source.name().into(),
        );
        line.spans[2].style = Style::new().fg(Color::Cyan);
        lines.push(line);
    }
    for warning in &facts.context.warnings {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::styled(format!("! {warning}"), Style::new().fg(Color::Yellow)),
        ]));
    }
}

/// What was looked up, or a note on why there is nothing yet.
fn checked<'a, T>(
    lines: &mut Vec<Line<'static>>,
    checks: bool,
    found: Option<&'a Vec<T>>,
) -> Option<&'a Vec<T>> {
    match found {
        _ if !checks => lines.push(note("not checked in this session".into())),
        None => lines.push(note("checking…".into())),
        Some(_) => {}
    }
    found.filter(|_| checks)
}

fn title(text: &'static str) -> Line<'static> {
    Line::styled(text, Style::new().add_modifier(Modifier::BOLD))
}

fn note(text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(text, theme::dim()),
    ])
}

/// `mark name  about`, the name in the default fg and the about dim, as
/// `nth formatters` and `nth lsp` print them.
fn row(mark: Span<'static>, name: String, about: String) -> Line<'static> {
    Line::from(vec![
        Span::raw(theme::INDENT),
        mark,
        Span::raw(format!(" {name}")),
        Span::styled(format!("  {about}"), theme::dim()),
    ])
}

fn tick() -> Span<'static> {
    Span::styled(icons().ok, Style::new().fg(Color::Green))
}

/// A server or formatter that does not run here, dim but for its cross.
fn unavailable(name: String, reason: String) -> Line<'static> {
    let mut line = row(
        Span::styled(icons().fail, Style::new().fg(Color::Red)),
        name,
        reason,
    );
    line.spans[2].style = theme::dim();
    line
}

fn path(path: &std::path::Path, home: Option<&str>) -> String {
    status::place(path, home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(diagnostics: &Diagnostics, facts: &Facts) -> Vec<String> {
        diagnostics
            .lines(facts)
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// Where the section titled `title` starts.
    fn section(text: &[String], title: &str) -> usize {
        text.iter()
            .position(|l| l == title)
            .unwrap_or_else(|| panic!("{title} section"))
    }

    fn facts<'a>(context: &'a Context, running: &'a [ServerStatus]) -> Facts<'a> {
        Facts {
            modes: [
                ModeModel {
                    mode: Mode::Plan,
                    model: "kimi".into(),
                    effort: None,
                    context_window: None,
                    current: false,
                },
                ModeModel {
                    mode: Mode::Act,
                    model: "glm".into(),
                    effort: Some("high"),
                    context_window: Some(128_000),
                    current: true,
                },
            ],
            llms: Some(3),
            failed: &[],
            listing: false,
            checks: true,
            running,
            context,
            home: None,
            usage: Vec::new(),
        }
    }

    #[test]
    fn shows_what_runs_first() {
        let context = Context::default();
        let running = [ServerStatus {
            id: "rust".into(),
            root: "/repo".into(),
            state: ServerState::Broken("exited".into()),
        }];
        let diagnostics = Diagnostics {
            servers: Some(vec![
                ServerInfo {
                    id: "gopls".into(),
                    extensions: vec![],
                    program: None,
                    root: None,
                },
                ServerInfo {
                    id: "rust".into(),
                    extensions: vec![],
                    program: Some("/bin/rust-analyzer".into()),
                    root: Some("/repo".into()),
                },
            ]),
            formatters: Some(vec![FormatterStatus {
                name: "rustfmt".into(),
                extensions: vec![],
                command: Ok(vec!["rustfmt".into(), "$FILE".into()]),
            }]),
            ..Diagnostics::default()
        };

        let text = text(&diagnostics, &facts(&context, &running));
        assert_eq!(text[3], "  3 models served");
        let servers = section(&text, "language servers");
        assert!(servers > section(&text, "agents"), "checks come last");
        assert_eq!(
            text[servers + 1],
            "  ● rust   /bin/rust-analyzer  → /repo  exited"
        );
        assert_eq!(text[servers + 2], "  ✗ gopls  not on PATH");
        let formatters = section(&text, "formatters");
        assert_eq!(text[formatters + 1], "  ✓ rustfmt  rustfmt $FILE");
        assert!(text.contains(&"  none found".to_string()));
    }

    #[test]
    fn shows_the_model_of_each_mode() {
        let context = Context::default();
        let mut facts = facts(&context, &[]);
        let acting = text(&Diagnostics::default(), &facts);
        assert_eq!(
            acting[1..3],
            ["    plan  kimi", "  ▸ act   glm · high · 128k context"]
        );

        facts.modes[0].current = true;
        facts.modes[1].current = false;
        let planning = text(&Diagnostics::default(), &facts);
        assert_eq!(
            planning[1..3],
            ["  ▸ plan  kimi", "    act   glm · high · 128k context"]
        );
    }

    #[test]
    fn lists_the_agents_after_the_skills() {
        // Nothing to find anywhere, so only the built-in agents.
        let context = Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        );

        let text = text(&Diagnostics::default(), &facts(&context, &[]));
        let agents = text
            .iter()
            .position(|l| l == "agents")
            .expect("agents section");
        assert!(text[..agents].contains(&"skills".to_string()));
        assert_eq!(text[agents + 1], "  ↳ explore  builtin");
        assert_eq!(text[agents + 2], "  ↳ general  builtin");
    }

    #[test]
    fn says_why_there_is_nothing_yet() {
        let context = Context::default();
        let mut facts = facts(&context, &[]);
        let checking = text(&Diagnostics::default(), &facts);
        let servers = section(&checking, "language servers");
        assert_eq!(checking[servers + 1], "  checking…");

        facts.checks = false;
        let unchecked = text(&Diagnostics::default(), &facts);
        assert_eq!(unchecked[servers + 1], "  not checked in this session");
    }

    #[test]
    fn graphs_the_turns_on_each_model() {
        let context = Context::default();
        let mut facts = facts(&context, &[]);
        let empty = text(&Diagnostics::default(), &facts);
        assert_eq!(empty[5..7], ["model usage", "  no turns yet"]);

        facts.usage = vec![("glm", 120), ("kimi", 30), ("qwen", 1)];
        let text = text(&Diagnostics::default(), &facts);
        let row = |model: &str, bar: &str, turns: &str| format!("  {model}  {bar:30}  {turns}");
        assert_eq!(text[6], row("glm ", &"█".repeat(30), "120"));
        assert_eq!(text[7], row("kimi", "███████▌", " 30"));
        assert_eq!(text[8], row("qwen", "▎", "  1"), "every used model shows");
    }
}
