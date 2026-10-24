//! The word on the status bar about what the last key did or did not do.
//! It comes in as a wave, a reversed head typing it out left to right,
//! since one that appears unannounced is easy to miss, and goes after a
//! while the same way, the head wiping it out, since one left from
//! minutes ago misleads.

use std::{ops::Deref, time::Duration};

use ratatui::{
    style::{Color, Style},
    text::Span,
};
use tokio::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::App;
use crate::spinner;

/// How many ticks a wave takes to cross the hint, whatever its length.
const TICKS: u32 = 10;
/// How many columns wide the reversed head is.
const CREST: f64 = 2.0;
/// How long a wave runs, in and out alike.
const WAVE: Duration = spinner::FRAME.saturating_mul(TICKS);
/// How long a hint stays when no key clears it first, the wave out
/// starting then.
const LIFE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    text: String,
    since: Instant,
}

/// Where a cell is against the wave's head.
#[derive(Clone, Copy)]
enum Cell {
    Ahead,
    Crest,
    Behind,
}

impl Hint {
    /// Whether a wave is running, which needs a redraw every tick.
    pub fn moving(&self) -> bool {
        let elapsed = self.since.elapsed();
        elapsed < WAVE || elapsed >= LIFE
    }

    pub fn expires(&self) -> Instant {
        self.since + LIFE + WAVE
    }

    /// The hint as it shows now. Coming in, the cells the head has not
    /// reached yet are blank; going, the ones it has passed are.
    pub fn spans(&self) -> Vec<Span<'static>> {
        let plain = Style::new().fg(Color::Yellow);
        let elapsed = self.since.elapsed();
        let (tick, leaving) = match elapsed {
            e if e < WAVE => (ticks(e), false),
            e if e >= LIFE => (ticks(e - LIFE), true),
            _ => return vec![Span::styled(self.text.clone(), plain)],
        };
        if tick >= TICKS {
            return Vec::new();
        }
        let head = -CREST / 2.0
            + (self.text.width() as f64 + CREST) * (f64::from(tick) + 0.5) / f64::from(TICKS);
        let mut runs: Vec<(Style, String)> = Vec::new();
        let mut at = 0.0;
        for ch in self.text.chars() {
            let wide = ch.width().unwrap_or(0);
            let middle = at + wide as f64 / 2.0;
            at += wide as f64;
            let cell = match middle {
                m if m > head + CREST / 2.0 => Cell::Ahead,
                m if m >= head - CREST / 2.0 => Cell::Crest,
                _ => Cell::Behind,
            };
            let (style, shown) = match (cell, leaving) {
                (Cell::Crest, _) => (Style::new().fg(Color::Black).bg(Color::Yellow), ch.into()),
                (Cell::Ahead, false) | (Cell::Behind, true) => (Style::new(), " ".repeat(wide)),
                _ => (plain, ch.into()),
            };
            match runs.last_mut() {
                Some((last, text)) if *last == style => text.push_str(&shown),
                _ => runs.push((style, shown)),
            }
        }
        runs.into_iter()
            .map(|(style, text)| Span::styled(text, style))
            .collect()
    }
}

/// The ticks `elapsed` spans, so a frame shows the same whenever in its
/// tick the redraw lands.
fn ticks(elapsed: Duration) -> u32 {
    u32::try_from(elapsed.as_millis() / spinner::FRAME.as_millis()).unwrap_or(u32::MAX)
}

impl App {
    pub(super) fn hint_expired(&mut self) {
        self.hint = None;
    }
}

/// Made the moment it is said, which is when its wave and life start.
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

    /// Halfway through tick `k`, so the test's own time stays in it.
    fn tick(k: u32) -> Duration {
        spinner::FRAME * k + spinner::FRAME / 2
    }

    const PLAIN: (Color, Color) = (Color::Yellow, Color::Reset);
    const HEAD: (Color, Color) = (Color::Black, Color::Yellow);

    /// The colours and symbol of the six cells from `at` on row 15.
    fn cells(app: &mut crate::app::App, at: u16) -> Vec<(Color, Color, String)> {
        let buffer = buffer(app);
        (at..at + 6)
            .map(|x| {
                let cell = &buffer[(x, 15)];
                (cell.fg, cell.bg, cell.symbol().to_string())
            })
            .collect()
    }

    #[tokio::test]
    async fn a_hint_waves_in_then_waves_out_and_goes() {
        let mut app = app();
        app.hint = Some(said("copied", WAVE));
        let row = rows(&mut app)[15].clone();
        let at = u16::try_from(row.find("copied").expect("the hint shows")).unwrap();
        assert!(
            cells(&mut app, at)
                .iter()
                .all(|(fg, bg, _)| (*fg, *bg) == PLAIN),
            "plain once the wave has passed"
        );

        // Three ticks in, the head is on `op`: `c` behind it, `ied` to come.
        app.hint = Some(said("copied", tick(3)));
        let wave = cells(&mut app, at);
        assert_eq!((wave[0].0, wave[0].1), PLAIN);
        assert_eq!((wave[1].0, wave[1].1), HEAD);
        assert_eq!((wave[2].0, wave[2].1), HEAD);
        assert!(
            wave[3..].iter().all(|(_, _, s)| s == " "),
            "not reached yet"
        );

        // Going, the same head wipes `c` away and leaves `ied` standing.
        app.hint = Some(said("copied", LIFE + tick(3)));
        let wave = cells(&mut app, at);
        assert_eq!(wave[0].2, " ", "wiped");
        assert_eq!((wave[1].0, wave[1].1), HEAD);
        assert_eq!((wave[2].0, wave[2].1), HEAD);
        assert!(wave[3..].iter().all(|(fg, bg, _)| (*fg, *bg) == PLAIN));

        app.hint = Some(said("copied", LIFE + WAVE));
        crate::app::monitor::due(app.hint.as_ref().map(Hint::expires)).await;
        app.hint_expired();
        assert_eq!(app.hint, None);
        assert!(rows(&mut app)[15].trim().is_empty());
    }
}
