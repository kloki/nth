//! The diagnostics tab: nth's own state, for when something does not work
//! as expected. The model, the language servers and formatters that check
//! writes, and the instruction files, skills and agents found for the
//! project.

use nth_context::Context;
use nth_format::FormatterStatus;
use nth_lsp::{ServerInfo, ServerState, ServerStatus};
use nth_protocol::Failed;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
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

/// Everything the tab shows that lives elsewhere on the app.
pub struct Facts<'a> {
    pub model: &'a str,
    pub effort: Option<&'a str>,
    pub context_window: Option<u64>,
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
        self.servers(&mut lines, facts);
        lines.push(Line::default());
        self.formatters(&mut lines, facts);
        lines.push(Line::default());
        context(&mut lines, facts);
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
                Some(root) => format!("→ {}", path(root, facts.home)),
                None => "no project root here".into(),
            };
            let about = format!("{}  {root}", path(program, facts.home));
            let running = facts.running.iter().find(|s| s.id == server.id);
            let mut line = match running {
                Some(status) => row(
                    Span::styled("●", Style::new().fg(status::state_colour(&status.state))),
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

fn model(lines: &mut Vec<Line<'static>>, facts: &Facts) {
    lines.push(title("model"));
    let mut about = vec![facts.model.to_string()];
    about.extend(facts.effort.map(String::from));
    if let Some(window) = facts.context_window {
        about.push(format!("{}k context", window / 1000));
    }
    lines.push(Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(about.join(" · "), Style::new().fg(Color::Blue)),
    ]));
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
            Span::raw("✦"),
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
            Span::raw("↳"),
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
    Span::styled("✓", Style::new().fg(Color::Green))
}

/// A server or formatter that does not run here, dim but for its cross.
fn unavailable(name: String, reason: String) -> Line<'static> {
    let mut line = row(Span::styled("✗", Style::new().fg(Color::Red)), name, reason);
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

    fn facts<'a>(context: &'a Context, running: &'a [ServerStatus]) -> Facts<'a> {
        Facts {
            model: "glm",
            effort: Some("high"),
            context_window: Some(128_000),
            llms: Some(3),
            failed: &[],
            listing: false,
            checks: true,
            running,
            context,
            home: None,
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
        assert_eq!(text[1], "  glm · high · 128k context");
        assert_eq!(text[2], "  3 models served");
        assert_eq!(text[5], "  ● rust   /bin/rust-analyzer  → /repo  exited");
        assert_eq!(text[6], "  ✗ gopls  not on PATH");
        assert_eq!(text[9], "  ✓ rustfmt  rustfmt $FILE");
        assert!(text.contains(&"  none found".to_string()));
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
        assert_eq!(checking[5], "  checking…");

        facts.checks = false;
        let unchecked = text(&Diagnostics::default(), &facts);
        assert_eq!(unchecked[5], "  not checked in this session");
    }
}
