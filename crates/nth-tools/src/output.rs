//! Cutting tool output down to what the model can take in. A fetched page
//! keeps its head, where the content is; a run of a command keeps its tail,
//! where the errors are, as it arrives (`process::Tail`).

/// Longer output than this keeps only one end: bash's `max_output_chars`
/// defaults to it, webfetch always uses it.
pub(crate) const MAX_CHARS: usize = 30_000;

/// The first `max_chars` of `text`, with a note on how much was cut.
pub(crate) fn head(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n\n...output truncated, showing the first {max_chars} of {count} characters...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_head_and_counts_characters_not_bytes() {
        assert_eq!(head("abc", 3), "abc");
        assert_eq!(
            head("abcdé", 2),
            "ab\n\n...output truncated, showing the first 2 of 5 characters..."
        );
    }
}
