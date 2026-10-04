//! The session picker: which saved session to pick back up. Opened over the
//! prompt by `/resume`, it waits for the list if it isn't in yet.

mod view;

use std::time::{Duration, SystemTime};

use nth_session::Summary;
use uuid::Uuid;
pub use view::draw;

#[derive(Debug)]
pub struct SessionPicker {
    /// The session in use, marked in the list.
    current: Uuid,
    state: State,
}

#[derive(Debug)]
enum State {
    Loading,
    Failed(String),
    Ready {
        sessions: Vec<Summary>,
        selected: usize,
    },
}

impl SessionPicker {
    pub fn new(current: Uuid) -> Self {
        Self {
            current,
            state: State::Loading,
        }
    }

    /// Fills in the list, newest first, highlighting the newest.
    pub fn load(&mut self, sessions: Result<Vec<Summary>, String>) {
        self.state = match sessions {
            Ok(sessions) if sessions.is_empty() => State::Failed("no saved sessions yet".into()),
            Ok(sessions) => State::Ready {
                sessions,
                selected: 0,
            },
            Err(error) => State::Failed(error),
        };
    }

    pub fn next(&mut self) {
        if let State::Ready { sessions, selected } = &mut self.state {
            *selected = (*selected + 1) % sessions.len();
        }
    }

    pub fn prev(&mut self) {
        if let State::Ready { sessions, selected } = &mut self.state {
            *selected = (*selected + sessions.len() - 1) % sessions.len();
        }
    }

    /// The highlighted session.
    pub fn chosen(&self) -> Option<Uuid> {
        match &self.state {
            State::Ready { sessions, selected } => sessions.get(*selected).map(|s| s.id),
            _ => None,
        }
    }
}

/// How long ago `then` was, in its largest whole unit: `now`, `5m`, `3h`, `2d`.
fn age(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn summary(title: &str, secs_ago: u64) -> Summary {
        Summary {
            id: Uuid::new_v4(),
            title: title.into(),
            cwd: "/repo".into(),
            model: "glm".into(),
            updated_at: SystemTime::now() - Duration::from_secs(secs_ago),
        }
    }

    #[test]
    fn opens_on_the_newest_and_wraps_around() {
        let sessions = vec![summary("new", 1), summary("old", 100)];
        let ids: Vec<_> = sessions.iter().map(|s| s.id).collect();
        let mut picker = SessionPicker::new(Uuid::new_v4());
        assert_eq!(picker.chosen(), None);

        picker.load(Ok(sessions));
        assert_eq!(picker.chosen(), Some(ids[0]));
        picker.prev();
        assert_eq!(picker.chosen(), Some(ids[1]));
        picker.next();
        assert_eq!(picker.chosen(), Some(ids[0]));
    }

    #[test]
    fn an_empty_list_chooses_nothing() {
        let mut picker = SessionPicker::new(Uuid::new_v4());
        picker.load(Ok(Vec::new()));
        picker.next();

        assert_eq!(picker.chosen(), None);
    }

    #[test]
    fn ages_round_down_to_the_largest_unit() {
        let now = SystemTime::now();
        let ago = |secs| age(now, now - Duration::from_secs(secs));

        assert_eq!(ago(5), "now");
        assert_eq!(ago(5 * 60 + 59), "5m");
        assert_eq!(ago(3 * 3_600), "3h");
        assert_eq!(ago(2 * 86_400 + 1), "2d");
    }
}
