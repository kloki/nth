//! What checks the tools' writes and rewrites their commands, as the app
//! shows it: the servers' states on the status bar, and what applies to
//! the project in the diagnostics tab.

use nth_lsp::ServerStatus;
use tokio::sync::watch;

use super::App;

impl App {
    /// Looks again at what applies, for the diagnostics tab: servers,
    /// formatters and programs may have been installed since the last time.
    pub(super) fn diagnose(&mut self) {
        self.list_llms();
        if let Some(checkers) = &self.checkers {
            self.diagnostics.diagnose(checkers, self.cwd.clone());
        }
    }

    /// Takes the servers' new states, or stops listening once the sender
    /// is gone, so the loop's arm doesn't spin.
    pub(super) fn servers_changed(&mut self, changed: bool) {
        match (&mut self.lsp, changed) {
            (Some(lsp), true) => self.servers = lsp.borrow_and_update().clone(),
            _ => self.lsp = None,
        }
    }
}

/// Resolves when the servers' states change, with `false` once the sender
/// is gone. Never resolves after that, so the loop's arm doesn't spin.
pub(super) async fn lsp_changed(lsp: &mut Option<watch::Receiver<Vec<ServerStatus>>>) -> bool {
    match lsp {
        Some(lsp) => lsp.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

pub(super) async fn usage_changed(
    usage: &mut Option<watch::Receiver<Vec<nth_lsp::ProcessUsage>>>,
) -> bool {
    match usage {
        Some(usage) => usage.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nth_lsp::ServerState;
    use ratatui::style::Color;

    use super::*;
    use crate::app::{
        Queued,
        tests::{buffer, rows, test_app},
    };

    fn server(id: &str, state: ServerState) -> ServerStatus {
        server_in(id, "/repo", state)
    }

    /// An app whose status bar shows the states `lsp` sends.
    fn showing(lsp: watch::Receiver<Vec<ServerStatus>>) -> App {
        let mut app = test_app();
        app.lsp = Some(lsp);
        app.servers_changed(true);
        app
    }

    fn server_in(id: &str, root: &str, state: ServerState) -> ServerStatus {
        ServerStatus {
            id: id.into(),
            root: root.into(),
            state,
        }
    }

    #[tokio::test]
    async fn status_line_two_shows_the_servers_on_the_right() {
        // The column of the `n`th dot on the line, for its colour.
        let dot = |row: &str, n: usize| {
            row.chars()
                .enumerate()
                .filter(|(_, c)| *c == '●')
                .nth(n)
                .map(|(i, _)| i as u16)
                .expect("a dot on the row")
        };
        let (tx, rx) = watch::channel(vec![server("rust", ServerState::Starting)]);
        let mut app = showing(rx);
        let starting = buffer(&mut app);
        let row = crate::app::tests::status_row(&rows(&mut app), 1).to_string();
        assert_eq!(row.trim(), "● lsp");
        assert!(
            row.trim_end().ends_with("● lsp"),
            "against the right edge: {row:?}"
        );
        assert_eq!(starting[(dot(&row, 0), 15)].fg, Color::Yellow, "starting");

        tx.send_replace(vec![
            server("rust", ServerState::Connected),
            server("bash", ServerState::Broken("exited".into())),
        ]);
        let changed = lsp_changed(&mut app.lsp).await;
        app.servers_changed(changed);
        let after = buffer(&mut app);

        let row = crate::app::tests::status_row(&rows(&mut app), 1).to_string();
        assert!(row.trim_end().ends_with("● lsp"), "{row:?}");
        assert_eq!(after[(dot(&row, 0), 15)].fg, Color::Red, "broken");

        // A server in a second root, a worktree, shows once,
        // in the colour of its worst client.
        tx.send_replace(vec![
            server("rust", ServerState::Connected),
            server_in("rust", "/repo/.nth/worktrees/x", ServerState::Starting),
        ]);
        let changed = lsp_changed(&mut app.lsp).await;
        app.servers_changed(changed);
        let twice = buffer(&mut app);
        let row = crate::app::tests::status_row(&rows(&mut app), 1).to_string();
        assert_eq!(row.trim(), "● lsp");
        assert_eq!(twice[(dot(&row, 0), 15)].fg, Color::Yellow, "worst client");

        tx.send_replace(vec![
            server("rust", ServerState::Connected),
            server("bash", ServerState::Broken("exited".into())),
        ]);
        let changed = lsp_changed(&mut app.lsp).await;
        app.servers_changed(changed);

        app.turns.set_queued([Queued::Prompt("lint".into())]);
        let row = crate::app::tests::status_row(&rows(&mut app), 1).to_string();
        assert!(row.starts_with(" > 1 queued · lint "), "{row:?}");
        assert!(
            row.trim_end().ends_with("● lsp"),
            "the queue shares it: {row:?}"
        );

        app.turns
            .set_queued([Queued::Prompt("fix the build".into())]);
        let row = crate::app::tests::status_row(&rows(&mut app), 1).to_string();
        assert!(
            row.starts_with(" > 1 queued · fix the build "),
            "the servers are cut first: {row:?}"
        );
    }

    #[tokio::test]
    async fn installing_a_runtime_drops_the_look_and_what_it_found() {
        let mut app = test_app();
        app.diagnose();
        let found = app.diagnostics.next().await.expect("looks");
        app.diagnostics.found(found);
        app.diagnose();
        let looking = app.diagnostics.looking();

        let session = nth_session::Session::new("m", "/repo".into());
        let provider = std::sync::Arc::new(crate::app::tests::Scripted::idle());
        app.install(crate::app::tests::runtime(session, provider));
        assert!(looking, "a look ran before");
        assert!(!app.diagnostics.looking(), "the old runtime's look is gone");
        assert!(app.diagnostics.shown().is_none(), "and what it found");
    }

    #[tokio::test]
    async fn the_status_bar_follows_the_servers_until_the_sender_is_gone() {
        let (tx, rx) = watch::channel(Vec::new());
        let mut app = showing(rx);
        tx.send_replace(vec![server("rust", ServerState::Connected)]);
        assert!(lsp_changed(&mut app.lsp).await);

        drop(tx);
        let changed = lsp_changed(&mut app.lsp).await;
        assert!(!changed, "the sender is gone");
        app.servers_changed(changed);
        let never = tokio::time::timeout(Duration::from_millis(10), lsp_changed(&mut app.lsp));
        assert!(
            never.await.is_err(),
            "a closed channel never wakes the loop"
        );
    }
}
