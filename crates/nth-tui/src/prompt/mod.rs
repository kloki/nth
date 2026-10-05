//! The prompt: the text being typed, with a cursor, and the mode it goes to.

mod view;
mod wrap;

use std::ops::Range;

pub use view::{ROWS, draw, position};

/// A paste enters the prompt whole until it is this many lines...
const PASTE_LINES: usize = 3;
/// ...or this many bytes; past either, it collapses to a placeholder.
const PASTE_BYTES: usize = 150;

/// A collapsed paste: the range its `[pasted N lines]` placeholder fills in
/// `text`, and the text it stands for.
#[derive(Debug, PartialEq)]
struct Paste {
    range: Range<usize>,
    text: String,
}

#[derive(Debug, Default)]
pub struct Prompt {
    text: String,
    /// Byte offset into `text`, always on a char boundary.
    cursor: usize,
    /// Collapsed pastes, in the order they were made. Their ranges shift
    /// with every edit; an edit that crosses one drops it.
    pastes: Vec<Paste>,
}

impl Prompt {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
    }

    /// Replaces the whole text, with the cursor at its end.
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.pastes.clear();
    }

    /// Replaces `range` of the text, with the cursor after the new part.
    pub fn replace(&mut self, range: Range<usize>, with: &str) {
        self.splice(range.start, range.len(), with.len());
        self.cursor = range.start + with.len();
        self.text.replace_range(range, with);
    }

    /// Inserts pasted text. A large one — [`PASTE_LINES`] lines or more, or
    /// over [`PASTE_BYTES`] bytes — enters as a `[pasted N lines]`
    /// placeholder that [`Prompt::take`] swaps back before sending.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = text.matches('\n').count() + 1;
        if lines < PASTE_LINES && text.len() <= PASTE_BYTES {
            self.insert_str(&text);
            return;
        }
        let placeholder = format!("[pasted {lines} lines]");
        let start = self.cursor;
        self.insert_str(&placeholder);
        self.insert(' ');
        self.pastes.push(Paste {
            range: start..start + placeholder.len(),
            text,
        });
    }

    /// The text as it should be sent: every placeholder swapped back for the
    /// paste it stands for.
    pub fn expanded(&self) -> String {
        let mut out = String::with_capacity(self.text.len());
        let mut at = 0;
        for paste in self.ordered() {
            out.push_str(&self.text[at..paste.range.start]);
            out.push_str(&paste.text);
            at = paste.range.end;
        }
        out.push_str(&self.text[at..]);
        out
    }

    pub fn take(&mut self) -> String {
        let text = self.expanded();
        self.clear();
        text
    }

    pub fn insert(&mut self, c: char) {
        self.splice(self.cursor, 0, c.len_utf8());
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.splice(self.cursor, 0, s.len());
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.splice(self.cursor - c.len_utf8(), c.len_utf8(), 0);
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.splice(self.cursor, c.len_utf8(), 0);
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// Start of the current line, not of the whole prompt.
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
    }

    /// Shifts the pastes across one edit: `removed` bytes at `at` become
    /// `inserted`. A paste the edit crosses is dropped, leaving its
    /// placeholder as plain text.
    fn splice(&mut self, at: usize, removed: usize, inserted: usize) {
        self.pastes.retain_mut(|paste| {
            if paste.range.end <= at {
                true
            } else if paste.range.start >= at + removed {
                paste.range.start = paste.range.start - removed + inserted;
                paste.range.end = paste.range.end - removed + inserted;
                true
            } else {
                false
            }
        });
    }

    /// The pastes by range, earliest first.
    fn ordered(&self) -> Vec<&Paste> {
        let mut pastes: Vec<&Paste> = self.pastes.iter().collect();
        pastes.sort_by_key(|paste| paste.range.start);
        pastes
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(super) fn prompt(text: &str) -> Prompt {
        let mut p = Prompt::default();
        p.insert_str(text);
        p
    }

    #[test]
    fn edits_at_the_cursor() {
        let mut p = prompt("héllo");
        p.left();
        p.left();
        p.backspace();
        p.insert('L');
        p.delete();

        assert_eq!(p.text, "héLo");
        assert_eq!(p.take(), "héLo");
        assert!(p.is_empty());
    }

    #[test]
    fn replace_leaves_the_cursor_after_the_new_text() {
        let mut p = prompt("see @ke and");
        p.replace(4..7, "@src/keys.rs ");
        assert_eq!(p.text, "see @src/keys.rs  and");
        assert_eq!(p.cursor, 17);
    }

    #[test]
    fn home_and_end_stay_on_the_current_line() {
        let mut p = prompt("one\ntwo");
        p.left();
        p.home();
        assert_eq!(p.cursor, 4);

        p.end();
        assert_eq!(p.cursor, 7);
    }

    #[test]
    fn a_small_paste_goes_in_as_typed() {
        let mut p = Prompt::default();
        p.paste("one\ntwo");

        assert_eq!(p.text, "one\ntwo");
        assert_eq!(p.take(), "one\ntwo");
    }

    #[test]
    fn a_large_paste_collapses_and_expands_on_take() {
        let pasted = (1..=5)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = Prompt::default();
        p.paste(&pasted);

        assert_eq!(p.text, "[pasted 5 lines] ");
        assert_eq!(p.take(), format!("{pasted} "), "the placeholder's space");
    }

    #[test]
    fn a_long_single_line_collapses() {
        let pasted = "x".repeat(PASTE_BYTES + 1);
        let mut p = Prompt::default();
        p.paste(&pasted);

        assert_eq!(p.text, "[pasted 1 lines] ");
        assert_eq!(p.take(), format!("{pasted} "));
    }

    #[test]
    fn crlf_and_cr_are_normalized_before_collapsing() {
        // Three lines only after `\r\n` and `\r` become `\n`.
        let mut p = Prompt::default();
        p.paste("a\r\nb\rc");

        assert_eq!(p.text, "[pasted 3 lines] ");
        assert_eq!(p.take(), "a\nb\nc ");
    }

    #[test]
    fn several_pastes_expand_in_place() {
        let mut p = Prompt::default();
        p.paste("a\nb\nc");
        p.paste("d\ne\nf");

        assert_eq!(p.take(), "a\nb\nc d\ne\nf ");
    }

    #[test]
    fn editing_around_a_placeholder_keeps_it() {
        let mut p = Prompt::default();
        p.insert_str("see ");
        p.paste("a\nb\nc");
        p.insert_str("done");

        assert_eq!(p.take(), "see a\nb\nc done");
    }

    #[test]
    fn editing_inside_a_placeholder_drops_it() {
        let mut p = Prompt::default();
        p.paste("a\nb\nc");
        p.home();
        p.right();
        p.insert('x');

        assert_eq!(p.take(), "[xpasted 3 lines] ");
    }

    #[test]
    fn clearing_or_replacing_forgets_the_pastes() {
        let mut p = Prompt::default();
        p.paste("a\nb\nc");
        p.clear();
        assert_eq!(p.take(), "");

        p.paste("a\nb\nc");
        p.set("plain");
        assert_eq!(p.take(), "plain");
    }
}
