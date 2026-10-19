//! Text the terminal listings share.

/// Longest description shown in a terminal listing.
const DESCRIPTION_CHARS: usize = 72;

/// The first line of `text`, cut to fit one terminal row.
pub fn shorten(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}
