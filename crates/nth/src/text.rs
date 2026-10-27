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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_uses_only_the_first_line_and_cuts_after_72_characters() {
        assert_eq!(shorten(""), "");
        assert_eq!(shorten("\nsecond"), "");
        assert_eq!(shorten("first\r\nsecond"), "first");
        assert_eq!(shorten(&"a".repeat(71)), "a".repeat(71));
        assert_eq!(shorten(&"a".repeat(72)), "a".repeat(72));
        assert_eq!(shorten(&"a".repeat(73)), format!("{}…", "a".repeat(72)));
    }

    #[test]
    fn shorten_counts_unicode_characters_without_splitting_utf8() {
        let line = format!("{}界🙂extra", "é".repeat(71));
        assert_eq!(shorten(&line), format!("{}界…", "é".repeat(71)));
        assert_eq!(shorten(&"🙂".repeat(72)), "🙂".repeat(72));
    }
}
