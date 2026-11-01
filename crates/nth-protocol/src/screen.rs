use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// A view the front-end's content panel can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Panel {
    Chat,
    Diagnostics,
    Plan,
    Work,
    Context,
}

impl Panel {
    pub const ALL: [Panel; 5] = [
        Panel::Chat,
        Panel::Diagnostics,
        Panel::Plan,
        Panel::Work,
        Panel::Context,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Panel::Chat => "chat",
            Panel::Diagnostics => "diagnostics",
            Panel::Plan => "plan",
            Panel::Work => "work",
            Panel::Context => "context",
        }
    }
}

/// What a tool puts in front of the user: one of the front-end's panels,
/// or a file in a tab of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Show {
    /// Opaque identity only: the front-end opens its host lazily.
    Curio {
        session_id: String,
        curio_id: String,
        revision: Option<u64>,
    },
    Panel(Panel),
    /// An absolute path to a file that exists.
    File(PathBuf),
}

/// Lets a tool switch what the front-end's content panel shows, as
/// [`Asker`](crate::Asker) lets it ask questions. The default screen is
/// nowhere, as in a headless run.
#[derive(Debug, Clone, Default)]
pub struct Screen(Option<mpsc::Sender<Show>>);

impl Screen {
    pub async fn show_curio(
        &self,
        session_id: String,
        curio_id: String,
        revision: Option<u64>,
    ) -> bool {
        self.send(Show::Curio {
            session_id,
            curio_id,
            revision,
        })
        .await
    }
    pub fn new(front_end: mpsc::Sender<Show>) -> Self {
        Self(Some(front_end))
    }

    /// Whether a front-end took the panel to show.
    pub async fn show(&self, panel: Panel) -> bool {
        self.send(Show::Panel(panel)).await
    }

    /// Whether a front-end took the file to show.
    pub async fn show_file(&self, path: PathBuf) -> bool {
        self.send(Show::File(path)).await
    }

    async fn send(&self, show: Show) -> bool {
        match &self.0 {
            Some(front_end) => front_end.send(show).await.is_ok(),
            None => false,
        }
    }
}
