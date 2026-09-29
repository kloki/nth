//! The brailleSparkle spinner from the spinners collection.

use std::time::Duration;

const FRAMES: [&str; 7] = [
    "⠁⠀⠀⠀⠁⠀⠀⡀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠐⠀⠀⠀",
    "⠀⡀⠀⠂⠀⠀⠀⠀⠄⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠈⠀⠀",
    "⠀⠀⠠⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⡀⠀⠀⠀⠀⠀⢀⠄",
    "⠀⠀⠀⠐⠀⡀⠁⠀⠀⠀",
];

pub const INTERVAL: Duration = Duration::from_millis(80);

/// Derived from elapsed time rather than a frame counter, so the spinner
/// runs at the same speed however often the screen redraws.
pub fn frame(elapsed: Duration) -> &'static str {
    let index = elapsed.as_millis() / INTERVAL.as_millis();
    FRAMES[index as usize % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn every_frame_is_ten_cells_wide() {
        assert!(FRAMES.iter().all(|f| f.width() == 10));
    }

    #[test]
    fn advances_one_frame_per_interval_and_wraps() {
        assert_eq!(frame(Duration::ZERO), FRAMES[0]);
        assert_eq!(frame(Duration::from_millis(79)), FRAMES[0]);
        assert_eq!(frame(Duration::from_millis(80)), FRAMES[1]);
        assert_eq!(frame(INTERVAL * 7), FRAMES[0]);
    }
}
