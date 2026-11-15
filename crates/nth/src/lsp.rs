//! `nth lsp`: which language servers apply here, and what they say about a
//! file. The same touch the write tool does, from the terminal.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Subcommand;
use nth_config::Config;
use nth_icons::icons;
use nth_lsp::{Diagnostic, Lsp, ServerInfo, ServerState, ServerStatus, report};
use owo_colors::OwoColorize;

use crate::output::{self, Report};

#[derive(Subcommand)]
pub enum LspCommand {
    /// Start the file's language servers and print the errors they report
    Diagnostics { file: PathBuf },
}

pub async fn run(command: Option<LspCommand>, json: bool, config: &Config) -> Result<()> {
    let lsp = Lsp::new(&config.lsp);
    match command {
        None => {
            let cwd = std::env::current_dir().context("no working directory")?;
            output::print(&ServersReport::read(lsp, &cwd).await?, json)
        }
        Some(LspCommand::Diagnostics { file }) => {
            // Only a relative path is resolved against the working
            // directory, so an absolute one still works from a directory
            // that was removed; joining it to the empty path keeps it whole.
            let cwd = match file.is_relative() {
                true => std::env::current_dir().context("no working directory")?,
                false => PathBuf::new(),
            };
            output::print(&DiagnosticsReport::read(lsp, &cwd, &file).await?, json)
        }
    }
}

/// Every server, whether its program is on PATH, and the root it would run
/// in for a file in the working directory.
struct ServersReport {
    servers: Vec<ServerInfo>,
}

impl ServersReport {
    async fn read(lsp: Lsp, cwd: &Path) -> Result<Self> {
        // Finding programs and roots walks PATH and the directory tree.
        let cwd = cwd.to_path_buf();
        let servers = tokio::task::spawn_blocking(move || lsp.servers_for(&cwd))
            .await
            .context("server lookup failed")?;
        Ok(Self { servers })
    }
}

impl Report for ServersReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        let width = self.servers.iter().map(|s| s.id.len()).max().unwrap_or(0);
        for server in &self.servers {
            match &server.program {
                Some(program) => {
                    let root = match &server.root {
                        Some(root) => format!("{} {}", icons().to, root.display()),
                        None => "no project root here".into(),
                    };
                    writeln!(
                        out,
                        "{} {:width$}  {}  {}",
                        icons().ok.green().bold(),
                        server.id.cyan(),
                        program.display().dimmed(),
                        root.dimmed()
                    )?
                }
                None => writeln!(
                    out,
                    "{} {:width$}  {}",
                    icons().fail.red(),
                    server.id.dimmed(),
                    "not on PATH".dimmed()
                )?,
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for server in &self.servers {
            let line = serde_json::json!({
                "id": server.id,
                "extensions": server.extensions,
                "program": server.program,
                "root": server.root,
            });
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

/// What the write tool would append after writing `file`, with the state
/// of the servers that were asked.
struct DiagnosticsReport {
    file: PathBuf,
    status: Vec<ServerStatus>,
    diagnostics: BTreeMap<PathBuf, Vec<Diagnostic>>,
}

impl DiagnosticsReport {
    /// Starts the file's servers, waits for what they report and stops
    /// them.
    async fn read(lsp: Lsp, cwd: &Path, file: &Path) -> Result<Self> {
        let file = std::path::absolute(cwd.join(file)).context("no working directory")?;
        anyhow::ensure!(file.is_file(), "{} is not a file", file.display());
        let diagnostics = lsp.touch(&file, true).await;
        lsp.shutdown().await;
        let status = lsp.status().borrow().clone();
        Ok(Self {
            file,
            status,
            diagnostics,
        })
    }
}

impl Report for DiagnosticsReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        if self.status.is_empty() {
            writeln!(
                out,
                "{} {}",
                icons().to.cyan().bold(),
                "no language server for this file".dimmed()
            )?;
            return Ok(());
        }
        for server in &self.status {
            if let ServerState::Broken(reason) = &server.state {
                writeln!(out, "{} {} {}", icons().fail.red(), server.id, reason.red())?;
            }
        }
        let text = report::after_write(&self.file, &self.diagnostics);
        match text.trim_start_matches('\n') {
            "" => writeln!(
                out,
                "{} {}",
                icons().ok.green().bold(),
                "no errors".dimmed()
            )?,
            text => writeln!(out, "{text}")?,
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for (path, diagnostics) in &self.diagnostics {
            for diagnostic in diagnostics {
                let mut line = serde_json::to_value(diagnostic)?;
                line["file"] = serde_json::json!(path);
                writeln!(out, "{line}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nth_lsp::types::{Position, Range};
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    fn lines(report: &impl Report) -> Vec<Value> {
        testing::json(report)
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect()
    }

    /// A server on PATH with a root, one on PATH with no root and one not
    /// on PATH.
    fn servers() -> ServersReport {
        let server = |id: &str, program: Option<&str>, root: Option<&str>| ServerInfo {
            id: id.into(),
            extensions: vec![".rs".into()],
            program: program.map(PathBuf::from),
            root: root.map(PathBuf::from),
        };
        ServersReport {
            servers: vec![
                server(
                    "rust-analyzer",
                    Some("/usr/bin/rust-analyzer"),
                    Some("/repo"),
                ),
                server("gopls", Some("/usr/bin/gopls"), None),
                server("pyright", None, None),
            ],
        }
    }

    #[test]
    fn each_server_says_where_it_runs_or_why_it_does_not() {
        insta::assert_snapshot!(testing::text(&servers()));
    }

    #[test]
    fn each_server_is_a_json_line() {
        let lines = lines(&servers());

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["program"], "/usr/bin/rust-analyzer");
        assert_eq!(lines[0]["root"], "/repo");
        assert_eq!(lines[1]["root"], Value::Null);
        assert_eq!(lines[2]["program"], Value::Null);
        insta::assert_json_snapshot!(lines[0]);
    }

    #[tokio::test]
    async fn read_finds_a_configured_server() {
        // The built-in table and PATH decide the rest, so only the
        // config's own server is pinned.
        let dir = tempfile::tempdir().expect("a temp dir");
        let config: nth_lsp::LspConfig =
            toml::from_str("[x]\ncommand = ['no-such-server-nth']\nextensions = ['.x']\n")
                .expect("an lsp config");

        let report = ServersReport::read(Lsp::new(&config), dir.path())
            .await
            .expect("the lookup");

        let x = report
            .servers
            .iter()
            .find(|s| s.id == "x")
            .expect("the config's server");
        assert_eq!(x.extensions, vec![".x".to_string()]);
        assert_eq!(x.program, None);
    }

    fn file() -> PathBuf {
        PathBuf::from("/repo/src/main.rs")
    }

    fn status(state: ServerState) -> Vec<ServerStatus> {
        vec![ServerStatus {
            id: "rust-analyzer".into(),
            root: "/repo".into(),
            state,
        }]
    }

    fn error(line: u32, message: &str) -> Diagnostic {
        let at = Position { line, character: 4 };
        Diagnostic {
            range: Range { start: at, end: at },
            severity: Some(1),
            code: None,
            source: Some("rustc".into()),
            message: message.into(),
        }
    }

    /// Errors in the file itself and in another one.
    fn errors() -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        BTreeMap::from([
            (file(), vec![error(2, "mismatched types")]),
            (
                PathBuf::from("/repo/src/lib.rs"),
                vec![error(9, "cannot find value `x`")],
            ),
        ])
    }

    #[test]
    fn no_server_for_the_file_says_so() {
        let report = DiagnosticsReport {
            file: file(),
            status: Vec::new(),
            diagnostics: BTreeMap::new(),
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn a_broken_server_says_why() {
        let report = DiagnosticsReport {
            file: file(),
            status: status(ServerState::Broken("exited with status 1".into())),
            diagnostics: BTreeMap::new(),
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn a_file_without_errors_says_so() {
        let report = DiagnosticsReport {
            file: file(),
            status: status(ServerState::Connected),
            diagnostics: BTreeMap::from([(file(), Vec::new())]),
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn errors_print_as_the_write_tool_reports_them() {
        let report = DiagnosticsReport {
            file: file(),
            status: status(ServerState::Connected),
            diagnostics: errors(),
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn each_diagnostic_is_a_json_line_naming_its_file() {
        let report = DiagnosticsReport {
            file: file(),
            status: status(ServerState::Connected),
            diagnostics: errors(),
        };

        let lines = lines(&report);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["file"], "/repo/src/lib.rs");
        assert_eq!(lines[1]["file"], "/repo/src/main.rs");
        assert_eq!(lines[1]["message"], "mismatched types");
        insta::assert_json_snapshot!(lines[1]);
    }

    #[tokio::test]
    async fn read_resolves_the_file_against_the_directory_given() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("notes.nolsp"), "text").expect("a file");
        let lsp = Lsp::new(&nth_lsp::LspConfig::default());

        let report = DiagnosticsReport::read(lsp, dir.path(), Path::new("./notes.nolsp"))
            .await
            .expect("the touch");

        assert_eq!(report.file, dir.path().join("notes.nolsp"));
        assert!(report.status.is_empty(), "{:?}", report.status);
        assert!(report.diagnostics.values().all(Vec::is_empty));
    }

    #[tokio::test]
    async fn read_takes_an_absolute_path_without_a_working_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("notes.nolsp");
        std::fs::write(&file, "text").expect("a file");
        let lsp = Lsp::new(&nth_lsp::LspConfig::default());

        let report = DiagnosticsReport::read(lsp, Path::new(""), &file)
            .await
            .expect("the touch");

        assert_eq!(report.file, file);
    }

    #[tokio::test]
    async fn read_refuses_what_is_not_a_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lsp = Lsp::new(&nth_lsp::LspConfig::default());

        let error = DiagnosticsReport::read(lsp, dir.path(), Path::new("missing"))
            .await
            .err()
            .expect("refused");

        assert!(
            error.to_string().ends_with("missing is not a file"),
            "{error}"
        );
    }
}
