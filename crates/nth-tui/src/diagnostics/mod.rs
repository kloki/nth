//! The diagnostics tab with what it looks up about the project each time
//! it opens: the servers and formatters that apply, and the programs the
//! tools run through, found together in one background look.

mod view;

use std::path::PathBuf;

use nth_format::FormatterStatus;
use nth_lsp::ServerInfo;
use ratatui::{Frame, layout::Rect};
use tokio::task::JoinError;
pub use view::{DiagnosticsView, Facts, ModeModel};

use crate::{Checkers, job::Job};

/// What looking a program up on PATH found.
#[derive(Debug)]
pub enum Program {
    /// On PATH.
    Found(PathBuf),
    /// Not on PATH.
    Missing,
}

impl From<Option<PathBuf>> for Program {
    fn from(found: Option<PathBuf>) -> Self {
        found.map_or(Self::Missing, Self::Found)
    }
}

/// The programs the diagnostics tab looks up on PATH, in one walk.
#[derive(Debug)]
pub struct Programs {
    /// The `rtk` bash would rewrite the model's commands through; `None`
    /// when the config turns rewriting off, so where it is does not matter.
    pub rtk: Option<Program>,
    /// The `agent-browser` the browser skill drives a browser with; nothing
    /// in the config turns that off, so it is always worth knowing.
    pub browser: Program,
    /// The Forgejo and GitHub command-line tools the status bar asks for
    /// the branch's pull request.
    pub tea: Program,
    pub gh: Program,
}

impl Programs {
    /// Walks PATH for each program, so it belongs on a blocking thread.
    pub fn find(rtk: bool) -> Self {
        Self {
            rtk: rtk.then(|| nth_context::which("rtk").into()),
            browser: nth_context::which("agent-browser").into(),
            tea: nth_context::which("tea").into(),
            gh: nth_context::which("gh").into(),
        }
    }
}

/// What one look at the project found.
#[derive(Debug)]
pub struct Lookup {
    pub servers: Vec<ServerInfo>,
    pub formatters: Vec<FormatterStatus>,
    pub programs: Programs,
}

/// The tab's view, what the last look found and the look that runs.
#[derive(Default)]
pub struct DiagnosticsTab {
    pub view: DiagnosticsView,
    /// `None` while a look runs, and before the first.
    found: Option<Lookup>,
    lookup: Job<Result<Lookup, JoinError>>,
}

impl DiagnosticsTab {
    /// Looks again at what applies in `cwd`, dropping what the last look
    /// found: servers, formatters and programs may have been installed
    /// since. A look still running is stopped for this one.
    pub fn diagnose(&mut self, checkers: &Checkers, cwd: PathBuf) {
        self.found = None;
        let (lsp, formatters, rtk) = (
            checkers.lsp.clone(),
            checkers.formatters.clone(),
            checkers.rtk,
        );
        self.lookup.start(|_| {
            tokio::spawn(async move {
                // Finding programs and roots walks PATH and the directory
                // tree; the formatters' probes run beside it.
                let blocking_cwd = cwd.clone();
                let walk = tokio::task::spawn_blocking(move || {
                    (lsp.servers_for(&blocking_cwd), Programs::find(rtk))
                });
                let formatters = formatters.status(&cwd).await;
                let (servers, programs) = walk.await?;
                Ok(Lookup {
                    servers,
                    formatters,
                    programs,
                })
            })
        });
    }

    /// Resolves once the running look ends, with what it found, or the
    /// error of the part of it that panicked. Never resolves while idle,
    /// so a `select!` arm needs no guard; cancel-safe.
    pub async fn next(&mut self) -> Result<Lookup, JoinError> {
        self.lookup.join().await.and_then(|found| found)
    }

    pub fn found(&mut self, lookup: Lookup) {
        self.found = Some(lookup);
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, facts: &Facts) {
        self.view.draw(frame, area, self.found.as_ref(), facts);
    }

    #[cfg(test)]
    pub fn looking(&self) -> bool {
        self.lookup.is_running()
    }

    #[cfg(test)]
    pub fn shown(&self) -> Option<&Lookup> {
        self.found.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_context::Context;
    use nth_format::Formatters;
    use nth_lsp::Lsp;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn checkers() -> Checkers {
        Checkers {
            lsp: Lsp::new(&Default::default()),
            formatters: Arc::new(Formatters::new(&Default::default())),
            rtk: true,
        }
    }

    fn facts(context: &Context) -> Facts<'_> {
        let model = |label| ModeModel {
            label,
            model: "glm".into(),
            effort: None,
            context_window: None,
            current: false,
            inherits: None,
        };
        Facts {
            models: [
                model("plan"),
                model("act"),
                model("subagent"),
                model("clerk"),
            ],
            llms: None,
            failed: &[],
            listing: false,
            checks: true,
            rtk: true,
            running: &[],
            lsp_usage: &[],
            agent_label: &|caller| caller.session.clone(),
            context,
            home: None,
        }
    }

    /// The tab drawn into a terminal tall enough for all of it.
    fn rows(tab: &mut DiagnosticsTab, facts: &Facts) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(100, 200)).expect("terminal");
        terminal
            .draw(|frame| tab.draw(frame, frame.area(), facts))
            .expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn one_lookup_fills_servers_formatters_and_programs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = DiagnosticsTab::default();
        tab.diagnose(&checkers(), dir.path().to_path_buf());
        let lookup = tab.next().await.expect("looks");
        assert!(!tab.looking(), "one look, one answer");
        assert!(
            !lookup.servers.is_empty(),
            "every known server, with or without a program"
        );
        assert!(!lookup.formatters.is_empty(), "every known formatter");
        assert!(lookup.programs.rtk.is_some(), "rtk is looked up when on");

        tab.found(lookup);
        let context = Context::default();
        let shown = rows(&mut tab, &facts(&context));
        let tools = shown
            .iter()
            .position(|row| row.ends_with("tools"))
            .expect("tools section");
        for (row, name) in
            shown[tools + 1..tools + 5]
                .iter()
                .zip(["rtk", "agent-browser", "tea", "gh"])
        {
            assert!(
                !row.contains("checking…") && row.contains(&format!(" {name} ")),
                "{name} looked up: {row:?}"
            );
        }
        assert!(
            !shown.iter().any(|row| row.contains("checking…")),
            "nothing left to look up: {shown:#?}"
        );
    }

    #[tokio::test]
    async fn opening_the_tab_again_looks_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tab = DiagnosticsTab::default();
        tab.diagnose(&checkers(), dir.path().to_path_buf());
        let first = tab.lookup.abort_handle().expect("looking");
        tab.found(Lookup {
            servers: Vec::new(),
            formatters: Vec::new(),
            programs: Programs {
                rtk: Some(Program::Found("/bin/rtk".into())),
                browser: Program::Found("/bin/agent-browser".into()),
                tea: Program::Found("/bin/tea".into()),
                gh: Program::Found("/bin/gh".into()),
            },
        });

        tab.diagnose(&checkers(), dir.path().to_path_buf());
        tokio::task::yield_now().await;
        assert!(first.is_finished(), "the first look is stopped");
        assert!(tab.looking(), "a second one runs");
        let context = Context::default();
        let shown = rows(&mut tab, &facts(&context));
        assert!(
            shown
                .iter()
                .any(|row| row.ends_with("rtk            checking…")),
            "what the first found is dropped: {shown:#?}"
        );
        tab.next().await.expect("looks");
    }
}
