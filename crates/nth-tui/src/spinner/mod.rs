//! The braille spinner shown in place of the mode while a turn runs.

use std::{sync::LazyLock, time::Duration};

/// How long each frame shows.
pub const FRAME: Duration = Duration::from_millis(80);

static FRAMES: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| include_str!("frames.txt").lines().collect());

/// The frame to show `elapsed` after the spinner started. Picking by time
/// rather than counting draws keeps the speed steady however often we draw.
pub fn frame(elapsed: Duration) -> &'static str {
    let index = elapsed.as_millis() / FRAME.as_millis();
    FRAMES[index as usize % FRAMES.len()]
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
}
