use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// A view the front-end's content panel can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Panel {
    Chat,
    Diagnostics,
    Plan,
}

impl Panel {
    pub const ALL: [Panel; 3] = [Panel::Chat, Panel::Diagnostics, Panel::Plan];

    pub fn name(self) -> &'static str {
        match self {
            Panel::Chat => "chat",
            Panel::Diagnostics => "diagnostics",
            Panel::Plan => "plan",
        }
    }
}

/// Lets a tool switch what the front-end's content panel shows, as
/// [`Asker`](crate::Asker) lets it ask questions. The default screen is
/// nowhere, as in a headless run.
#[derive(Debug, Clone, Default)]
pub struct Screen(Option<mpsc::Sender<Panel>>);

impl Screen {
    pub fn new(front_end: mpsc::Sender<Panel>) -> Self {
        Self(Some(front_end))
    }

    /// Whether a front-end took the panel to show.
    pub async fn show(&self, panel: Panel) -> bool {
        match &self.0 {
            Some(front_end) => front_end.send(panel).await.is_ok(),
            None => false,
        }
    }
}
