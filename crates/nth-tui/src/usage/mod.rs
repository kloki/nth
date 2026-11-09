//! The usage tab with what it reads beyond the running session: every
//! saved session's tool totals, read again each time it opens and after a
//! turn while it shows.

mod view;

use std::sync::Arc;

use nth_protocol::Cost;
use nth_session::{Ledger, Store, ToolUse, store};
use ratatui::{Frame, layout::Rect};
use tokio::task::JoinError;
pub use view::UsageView;

use crate::app::job::Job;

/// Every tool's all-time totals, most called first.
pub type ToolTotals = Vec<(String, ToolUse)>;

/// The tab's view, the all-time tool totals and the read that refreshes
/// them.
#[derive(Default)]
pub struct UsageTab {
    pub view: UsageView,
    /// Empty until the first read lands; a failed read leaves the last.
    all_time: ToolTotals,
    tools: Job<Result<ToolTotals, store::Error>>,
}

impl UsageTab {
    /// Re-reads every saved session's tool totals from `store`, unless a
    /// read already runs: the sessions it reads change only at a turn's
    /// end, so a second read would find the same.
    pub fn load_tools(&mut self, store: Arc<Store>) {
        if self.tools.is_running() {
            return;
        }
        self.tools.start(|_| {
            tokio::spawn(async move {
                let spending = store.spending().await?;
                let spends: Vec<&nth_session::Spend> = spending
                    .iter()
                    .flat_map(|session| session.usage.spends())
                    .collect();
                let mut tools: ToolTotals = nth_session::usage::tools(spends)
                    .into_iter()
                    .map(|(name, use_of)| (name.to_string(), use_of))
                    .collect();
                tools.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then_with(|| a.0.cmp(&b.0)));
                Ok(tools)
            })
        });
    }

    /// Resolves when the running read ends. Never resolves while idle, so
    /// a `select!` arm needs no guard; cancel-safe.
    pub async fn next(&mut self) -> Result<Result<ToolTotals, store::Error>, JoinError> {
        self.tools.join().await
    }

    pub fn found(&mut self, tools: ToolTotals) {
        self.all_time = tools;
    }

    /// `ledger` is what the session spent, `turns` how many turns ran on
    /// each model, most first, and `cost` what each model costs, when the
    /// catalogue says.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ledger: &Ledger,
        turns: &[(&str, u64)],
        cost: &dyn Fn(&str) -> Option<Cost>,
    ) {
        self.view
            .draw(frame, area, ledger, turns, &self.all_time, cost);
    }
}
