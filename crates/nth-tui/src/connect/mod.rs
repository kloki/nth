//! `/connect`: signing in to a provider that takes an account rather than
//! an API key. The panel lists them with whether each is connected, then
//! waits on the browser login Enter started, showing its URL in case the
//! browser did not open.

mod view;

pub use view::draw;

/// A provider `/connect` signs in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectable {
    ChatGpt,
}

impl Connectable {
    pub const ALL: [Connectable; 1] = [Connectable::ChatGpt];

    pub fn name(self) -> &'static str {
        match self {
            Connectable::ChatGpt => "ChatGPT",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Connectable::ChatGpt => "a Plus or Pro subscription",
        }
    }
}

/// The waiting panel: its title, the provider, the URL and what it waits on.
const WAITING_ROWS: u16 = 4;

/// The providers to pick from, then the login running for one.
#[derive(Debug)]
pub struct ConnectPanel {
    /// Whether each of `Connectable::ALL` is connected, as of opening.
    connected: [bool; Connectable::ALL.len()],
    selected: usize,
    waiting: Option<Waiting>,
}

/// A login started from the panel.
#[derive(Debug)]
pub struct Waiting {
    pub to: Connectable,
    /// Where the browser signs in; `None` until the login has a port.
    pub url: Option<String>,
}

impl ConnectPanel {
    pub fn new(connected: [bool; Connectable::ALL.len()]) -> Self {
        Self {
            connected,
            selected: 0,
            waiting: None,
        }
    }

    /// The title and a row per provider, or the waiting panel's rows.
    pub fn rows(&self) -> u16 {
        match self.waiting {
            Some(_) => WAITING_ROWS,
            None => 1 + Connectable::ALL.len() as u16,
        }
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1).min(Connectable::ALL.len() - 1);
    }

    pub fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected(&self) -> Connectable {
        Connectable::ALL[self.selected]
    }

    pub fn is_connected(&self, to: Connectable) -> bool {
        Connectable::ALL
            .iter()
            .position(|&c| c == to)
            .is_some_and(|i| self.connected[i])
    }

    /// Waits on a login to the highlighted provider, which it returns.
    pub fn start(&mut self) -> Connectable {
        let to = self.selected();
        self.waiting = Some(Waiting { to, url: None });
        to
    }

    pub fn waiting(&self) -> Option<&Waiting> {
        self.waiting.as_ref()
    }

    /// Shows where the browser signs in, once the login knows.
    pub fn set_url(&mut self, url: String) {
        if let Some(waiting) = &mut self.waiting {
            waiting.url = Some(url);
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.waiting.as_ref()?.url.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_to_the_waiting_panel_once_started() {
        let mut panel = ConnectPanel::new([true]);
        assert_eq!(panel.rows(), 2);
        assert!(panel.is_connected(Connectable::ChatGpt));
        panel.next();
        assert_eq!(panel.start(), Connectable::ChatGpt);
        assert_eq!(panel.rows(), WAITING_ROWS);
        assert_eq!(panel.url(), None);

        panel.set_url("https://auth.example/authorize".into());
        assert_eq!(panel.url(), Some("https://auth.example/authorize"));
    }
}
