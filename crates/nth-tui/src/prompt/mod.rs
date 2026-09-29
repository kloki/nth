//! The prompt bar: the text being typed, with a cursor.

mod view;
mod wrap;

pub use view::{PROMPT_ROWS, draw};

#[derive(Debug, Default)]
pub struct Prompt {
    text: String,
    /// Byte offset into `text`, always on a char boundary.
    cursor: usize,
}

impl Prompt {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
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
    fn home_and_end_stay_on_the_current_line() {
        let mut p = prompt("one\ntwo");
        p.left();
        p.home();
        assert_eq!(p.cursor, 4);

        p.end();
        assert_eq!(p.cursor, 7);
    }
}
