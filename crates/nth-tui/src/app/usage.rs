//! What the session spent, as the usage tab, the status bar and the
//! model picker show it. The session's ledger is the one record: while a
//! turn's task holds the session, the app draws the copy it took then and
//! what the running turn reported spending so far; a subagent's turn
//! shows once its spend reaches that ledger, when the parent's turn ends,
//! or at once while the parent is idle.

use std::borrow::Cow;

use nth_session::{Ledger, Session, Spend, store};

use super::App;
use crate::usage::ToolTotals;

impl App {
    /// What the session spent, its subagents' finished turns included,
    /// with the running turn so far.
    pub(crate) fn spent(&self) -> Cow<'_, Ledger> {
        spent(self.session.as_ref(), &self.ledger, self.running.as_ref())
    }

    /// Re-reads every saved session's tool totals, for the usage tab's
    /// all-time section; the store's absence leaves what is shown.
    pub(super) fn load_tools(&mut self) {
        if let Some(store) = self.store.clone() {
            self.usage_tab.load_tools(store);
        }
    }

    /// The tool totals arrived; a failure keeps what was already shown and
    /// says why.
    pub(super) fn tools_found(&mut self, tools: Result<ToolTotals, store::Error>) {
        match tools {
            Ok(tools) => self.usage_tab.found(tools),
            Err(e) => self.hint = Some(format!("tool usage: {e}").into()),
        }
    }
}

/// The session's ledger while it is here, else `ledger`, the copy taken
/// when its turn's task took it, with `running` on top: apart from `App`,
/// so a view on `App` can be drawn into beside it.
pub(super) fn spent<'a>(
    session: Option<&'a Session>,
    ledger: &'a Ledger,
    running: Option<&Spend>,
) -> Cow<'a, Ledger> {
    let ledger = session.map_or(ledger, |session| &session.usage);
    match running {
        None => Cow::Borrowed(ledger),
        Some(running) => {
            let mut ledger = ledger.clone();
            ledger.record(running.clone());
            Cow::Owned(ledger)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_protocol::{Event, StreamEvent, Usage};
    use nth_session::{Session, Total};

    use super::*;
    use crate::app::{
        Tab,
        tests::{Scripted, rows},
    };

    fn usage(input: u64) -> StreamEvent {
        StreamEvent::Usage(Usage {
            input,
            output: 10,
            ..Usage::default()
        })
    }

    fn total(steps: u32, input: u64, output: u64) -> Total {
        Total {
            steps,
            tokens: Usage {
                input,
                output,
                ..Usage::default()
            },
        }
    }

    /// An idle app whose requests get `replies` in turn, then "ok".
    fn app(replies: impl IntoIterator<Item = Vec<StreamEvent>>) -> App {
        let session = Session::new("glm", "/repo".into());
        App::bare(
            session,
            Arc::new(Scripted::new(replies)),
            Arc::new(Vec::new()),
        )
    }

    /// Hears what the turn sent, as the loop does before the turn's end
    /// arrives, and what the app drew as spent after each spend report.
    fn hear(app: &mut App) -> Vec<Total> {
        let mut drawn = Vec::new();
        while let Ok(event) = app.events_rx.try_recv() {
            let spend = matches!(event, Event::Spend(_));
            app.on_session(event);
            if spend {
                drawn.push(app.spent().total());
            }
        }
        drawn
    }

    #[tokio::test]
    async fn the_usage_tab_follows_the_running_turn() {
        let mut app = app([vec![
            StreamEvent::TextDelta("done".into()),
            usage(100),
            usage(50),
        ]]);
        app.open_content(Tab::Usage);
        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn ends");

        assert_eq!(
            hear(&mut app),
            [total(1, 100, 10), total(2, 150, 20)],
            "each report shows while the turn runs"
        );
        // The usage tab's rows; the input panel below shows the spinner.
        let tab = |app: &mut App| rows(app)[..9].to_vec();
        let running = tab(&mut app);
        assert!(running[3].contains("all  2 steps  150 in"), "{running:#?}");
        app.end_turn(ended);
        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.usage.total(), total(2, 150, 20));
        assert_eq!(app.spent().total(), session.usage.total());
        assert_eq!(app.running, None);
        assert_eq!(tab(&mut app), running, "the same totals, from the session");
    }

    #[tokio::test]
    async fn a_compaction_is_its_own_spend() {
        let mut app = app([
            vec![StreamEvent::TextDelta("done".into()), usage(100)],
            vec![StreamEvent::TextDelta("the summary".into()), usage(900)],
        ]);
        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn ends");
        app.end_turn(ended);

        app.prompt.insert_str("/compact");
        app.submit();
        assert!(app.is_busy(), "the compaction runs as a turn");
        let ended = app.turn.join().await.expect("compaction ends");
        hear(&mut app);
        let spends = |ledger: &Ledger| -> Vec<u64> {
            ledger.spends().iter().map(|s| s.tokens.input).collect()
        };
        assert_eq!(
            spends(&app.spent()),
            [100, 900],
            "not booked on the turn before it"
        );
        app.end_turn(ended);
        let session = app.session.as_ref().expect("session came back");
        assert_eq!(spends(&session.usage), [100, 900]);
        assert_eq!(*app.spent(), session.usage);
    }
}
