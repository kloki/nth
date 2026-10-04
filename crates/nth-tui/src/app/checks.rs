//! What checks the tools' writes, as the app shows it: the servers' states
//! on the status bar, and what applies to the project in the diagnostics
//! tab.

use nth_lsp::ServerStatus;
use tokio::sync::watch;

use super::App;
use crate::Checkers;

impl App {
    /// Shows the servers' states on the status bar, and what applies to
    /// the project in the diagnostics tab.
    pub fn with_checkers(self, checkers: Checkers) -> Self {
        let lsp = checkers.lsp.status();
        Self {
            checkers: Some(checkers),
            ..self
        }
        .with_lsp(lsp)
    }

    /// Shows the states `lsp` sends on the status bar.
    pub fn with_lsp(mut self, mut lsp: watch::Receiver<Vec<ServerStatus>>) -> Self {
        self.servers = lsp.borrow_and_update().clone();
        self.lsp = Some(lsp);
        self
    }

    /// Looks again at what applies, for the diagnostics tab: servers and
    /// formatters may have been installed since the last time.
    pub(super) fn diagnose(&mut self) {
        self.list_llms();
        let Some(checkers) = &self.checkers else {
            return;
        };
        self.diagnostics.servers = None;
        self.diagnostics.formatters = None;
        // Finding programs and roots walks PATH and the directory tree.
        let (lsp, cwd) = (checkers.lsp.clone(), self.cwd.clone());
        self.servers_lookup
            .start(|_| tokio::task::spawn_blocking(move || lsp.servers_for(&cwd)));
        let (formatters, cwd) = (checkers.formatters.clone(), self.cwd.clone());
        self.formatters_lookup
            .start(|_| tokio::spawn(async move { formatters.status(&cwd).await }));
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nth_lsp::ServerState;
    use ratatui::style::Color;

    use super::*;
    use crate::app::tests::{app, buffer, rows};

    fn server(id: &str, state: ServerState) -> ServerStatus {
        ServerStatus {
            id: id.into(),
            root: "/repo".into(),
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
                .unwrap()
        };
        let (tx, rx) = watch::channel(vec![server("rust", ServerState::Starting)]);
        let mut app = app().with_lsp(rx);
        let starting = buffer(&mut app);
        let row = rows(&mut app)[15].clone();
        assert_eq!(row.trim(), "● rust");
        assert!(
            row.trim_end().ends_with("● rust"),
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

        let row = rows(&mut app)[15].clone();
        assert!(row.trim_end().ends_with("● rust  ● bash"), "{row:?}");
        assert_eq!(after[(dot(&row, 0), 15)].fg, Color::Green, "connected");
        assert_eq!(after[(dot(&row, 1), 15)].fg, Color::Red, "broken");

        app.queue = ["lint".into()].into();
        let row = rows(&mut app)[15].clone();
        assert!(row.starts_with(" ⏵ 1 queued · lint "), "{row:?}");
        assert!(
            row.trim_end().ends_with("● rust  ● bash"),
            "the queue shares it: {row:?}"
        );

        app.queue = ["fix the build".into()].into();
        let row = rows(&mut app)[15].clone();
        assert!(
            row.starts_with(" ⏵ 1 queued · fix the build "),
            "the servers are cut first: {row:?}"
        );
    }

    #[tokio::test]
    async fn the_status_bar_follows_the_servers_until_the_sender_is_gone() {
        let (tx, rx) = watch::channel(Vec::new());
        let mut app = app().with_lsp(rx);
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
