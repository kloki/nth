//! The completion popup shared by commands and file mentions: a short list
//! of candidates with one highlighted.

mod view;

pub use view::draw;

#[derive(Debug)]
pub struct Popup<T> {
    items: Vec<T>,
    selected: usize,
}

impl<T> Popup<T> {
    /// `None` when there is nothing to pick from.
    pub fn new(items: Vec<T>) -> Option<Self> {
        (!items.is_empty()).then_some(Self { items, selected: 0 })
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn prev(&mut self) {
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    pub fn selected(&self) -> &T {
        &self.items[self.selected]
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_both_ways() {
        assert!(Popup::<u8>::new(Vec::new()).is_none());

        let mut popup = Popup::new(vec!['a', 'b']).expect("items");
        assert_eq!(popup.selected(), &'a');
        popup.next();
        assert_eq!(popup.selected(), &'b');
        popup.next();
        assert_eq!(popup.selected(), &'a');
        popup.prev();
        assert_eq!(popup.selected(), &'b');
    }
}
