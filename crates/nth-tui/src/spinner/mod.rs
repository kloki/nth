//! The braille spinners: the wide one shown in place of the mode while a
//! turn runs, and the one-character one a tab shows as its icon.

use std::{sync::LazyLock, time::Duration};

/// How long each frame shows.
pub const FRAME: Duration = Duration::from_millis(80);

static FRAMES: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| include_str!("spinner_big.txt").lines().collect());

/// The frame to show `elapsed` after the spinner started. Picking by time
/// rather than counting draws keeps the speed steady however often we draw.
pub fn frame(elapsed: Duration) -> &'static str {
    let index = elapsed.as_millis() / FRAME.as_millis();
    FRAMES[index as usize % FRAMES.len()]
}

/// The tabs' spinner: one character, so a tab keeps its width whether it
/// spins or shows its icon.
static DOTS: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| include_str!("spinner.txt").lines().collect());

/// The tabs' frame `elapsed` after the spinner started, stepping with the
/// wide one.
pub fn dot(elapsed: Duration) -> &'static str {
    let index = elapsed.as_millis() / FRAME.as_millis();
    DOTS[index as usize % DOTS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_every_frame_and_wraps() {
        assert_eq!(FRAMES.len(), 16);
        assert_eq!(frame(Duration::ZERO), "⠖⠉⠉⠑");
        assert_eq!(frame(FRAME * 3 / 2), "⡠⠖⠉⠉");
        assert_eq!(frame(FRAME * 16), frame(Duration::ZERO));
        assert!(FRAMES.iter().all(|f| f.chars().count() == 4));
    }

    #[test]
    fn the_tabs_spinner_is_one_character() {
        assert_eq!(dot(Duration::ZERO), "⠋");
        assert_eq!(dot(FRAME * 3 / 2), "⠙");
        assert_eq!(DOTS.len(), 10);
        assert_eq!(dot(FRAME * 10), dot(Duration::ZERO));
        assert!(DOTS.iter().all(|f| f.chars().count() == 1));
    }
}
