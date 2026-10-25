//! The review shell's assets against what they copy from elsewhere.

/// The viewer draws nth's TUI spinner from its own copy of the frames.
#[test]
fn the_viewer_spinner_is_the_tuis() {
    let tui = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nth-tui/src/spinner/spinner_big.txt"),
    )
    .expect("the TUI's spinner frames");
    let script = include_str!("../assets/viewer/viewer.js");
    let start = script
        .find("const SPINNER = [")
        .expect("the viewer's SPINNER");
    let end = start + script[start..].find("];").expect("the end of SPINNER");
    let viewer: Vec<&str> = script[start..end].split('"').skip(1).step_by(2).collect();
    let frames: Vec<&str> = tui.lines().collect();
    assert_eq!(viewer, frames);
}
