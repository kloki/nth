//! The text being typed in the prompt bar, with a cursor.

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default)]
pub struct Prompt {
    text: String,
    /// Byte offset into `text`, always on a char boundary.
    cursor: usize,
}

/// The prompt wrapped to a width: its rows and where the cursor sits.
#[derive(Debug, PartialEq)]
pub struct Wrapped {
    pub rows: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
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

    /// Hard-wraps at `width` cells, character by character, so the cursor
    /// position is exact even inside a long word.
    pub fn wrap(&self, width: usize) -> Wrapped {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let (mut cursor_row, mut cursor_col) = (0, 0);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                (cursor_row, cursor_col) = (rows.len() - 1, col);
            }
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
                if i == self.cursor {
                    (cursor_row, cursor_col) = (rows.len() - 1, 0);
                }
            }
            if let Some(row) = rows.last_mut() {
                row.push(c);
            }
            col += w;
        }
        if self.cursor == self.text.len() {
            // A cursor past the last cell starts the next row.
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            (cursor_row, cursor_col) = (rows.len() - 1, col);
        }
        Wrapped {
            rows,
            cursor_row,
            cursor_col,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Prompt {
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

    #[test]
    fn wraps_rows_and_tracks_the_cursor() {
        let mut p = prompt("abcdef\nxy");

        assert_eq!(
            p.wrap(4),
            Wrapped {
                rows: vec!["abcd".into(), "ef".into(), "xy".into()],
                cursor_row: 2,
                cursor_col: 2,
            }
        );

        p.home();
        p.left();
        p.left();
        assert_eq!((p.wrap(4).cursor_row, p.wrap(4).cursor_col), (1, 1));
    }

    #[test]
    fn cursor_after_a_full_row_moves_to_the_next() {
        let w = prompt("abcd").wrap(4);

        assert_eq!(w.rows, ["abcd", ""]);
        assert_eq!((w.cursor_row, w.cursor_col), (1, 0));
    }
}
