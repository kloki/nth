//! The word on the status bar about what the last key did or did not do.
//! It flashes as it comes in, since one that appears unannounced is easy
//! to miss, and goes after a while, since one left from minutes ago
//! misleads.

use std::{ops::Deref, time::Duration};

use tokio::time::Instant;

use super::App;

/// How long a new hint shows reversed.
const FLASH: Duration = Duration::from_millis(300);
/// How long a hint stays when no key clears it first.
const LIFE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    text: String,
    since: Instant,
}

impl Hint {
    pub fn flashing(&self) -> bool {
        self.since.elapsed() < FLASH
    }

    pub fn expires(&self) -> Instant {
        self.since + LIFE
    }
}

impl App {
    pub(super) fn hint_expired(&mut self) {
        self.hint = None;
    }
}

/// Made the moment it is said, which is when its flash and life start.
impl From<String> for Hint {
    fn from(text: String) -> Self {
        Self {
            text,
            since: Instant::now(),
        }
    }
}

impl From<&str> for Hint {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

impl Deref for Hint {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;
    use crate::app::tests::{app, buffer, rows};

    /// A hint said `ago`, as the clock would have it by now.
    fn said(text: &str, ago: Duration) -> Hint {
        Hint {
            text: text.into(),
            since: Instant::now() - ago,
        }
    }

    #[tokio::test]
    async fn a_hint_flashes_then_goes_after_a_while() {
        let mut app = app();
        app.hint = Some("copied".into());
        let row = rows(&mut app)[15].clone();
        let at = u16::try_from(row.find("copied").expect("the hint shows")).unwrap();
        assert_eq!(buffer(&mut app)[(at, 15)].bg, Color::Yellow, "flashing");

        app.hint = Some(said("copied", FLASH));
        let cell = buffer(&mut app)[(at, 15)].clone();
        assert_eq!((cell.fg, cell.bg), (Color::Yellow, Color::Reset), "plain");

        app.hint = Some(said("copied", LIFE));
        crate::app::monitor::due(app.hint.as_ref().map(Hint::expires)).await;
        app.hint_expired();
        assert_eq!(app.hint, None);
        assert!(rows(&mut app)[15].trim().is_empty());
    }
}
