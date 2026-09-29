//! Hard-wraps the prompt text for the prompt bar, tracking the cursor.

use unicode_width::UnicodeWidthChar;

use super::Prompt;

/// The prompt wrapped to a width: its rows and where the cursor sits.
#[derive(Debug, PartialEq)]
pub struct Wrapped {
    pub rows: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl Prompt {
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
    use crate::prompt::tests::prompt;

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
