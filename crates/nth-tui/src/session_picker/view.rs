//! Draws the picker in the prompt's place: a header with its keys, the
//! query, then the sessions matching it, newest first while it is empty,
//! scrolled to the highlighted one.

use std::time::SystemTime;

use nth_icons::icons;
use nth_session::Summary;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{SessionPicker, State, age, haystack};
use crate::{
    fuzzy::{self, Filter, query_row},
    status,
    theme::{BAR, dim, panel_row, panel_title, pick},
};

const TITLE: &str = "resume session";
const KEYS: &str = "type to filter · ↑↓ session · enter · esc";

/// The picker's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;
/// Room for the widest age, `999d`.
const AGE_WIDTH: usize = 4;

pub fn draw(frame: &mut Frame, area: Rect, picker: &SessionPicker) {
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(panel_title(TITLE, ACCENT)), header);
    // Dropped rather than drawn over the title when the row is too narrow.
    let room = usize::from(header.width);
    if room > BAR.chars().count() + TITLE.len() + KEYS.chars().count() + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(KEYS, dim())).alignment(Alignment::Right),
            header,
        );
    }

    let (lines, list) = match &picker.state {
        State::Loading => (vec![note(Span::styled("loading sessions…", dim()))], body),
        State::Failed(error) => (
            vec![note(Span::styled(
                format!("{} {error}", icons().fail),
                Style::new().fg(Color::Red),
            ))],
            body,
        ),
        State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } => {
            // The query row only once there is a list to filter.
            let [query, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
            query_row(
                frame,
                query,
                ACCENT,
                filter.query(),
                shown.len(),
                sessions.len(),
            );
            let lines = rows(
                picker,
                Listed {
                    sessions,
                    filter,
                    shown,
                },
                *selected,
                usize::from(list.width),
                usize::from(list.height),
            );
            (lines, list)
        }
    };
    frame.render_widget(Paragraph::new(lines), list);
}

fn note(text: Span<'_>) -> Line<'_> {
    panel_row(ACCENT, [text])
}

/// The ready list, as `rows` draws it.
struct Listed<'a> {
    sessions: &'a [Summary],
    filter: &'a Filter,
    shown: &'a [usize],
}

/// One row per session matching the query: how long ago it was used, a ✓
/// on the one in use, a star for a manually named session, its title, and
/// the directory it runs in, with the characters the query matched marked.
fn rows<'a>(
    picker: &SessionPicker,
    listed: Listed<'_>,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let Listed {
        sessions,
        filter,
        shown,
    } = listed;
    if shown.is_empty() {
        let message = if sessions.is_empty() && filter.is_empty() {
            "  no saved sessions yet"
        } else {
            "  no matches"
        };
        return vec![note(Span::styled(message, dim()))];
    }
    let now = SystemTime::now();
    let home = std::env::var("HOME").ok();
    let first = selected.saturating_sub(height.saturating_sub(1));
    let pick = pick();

    shown
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(row, &i)| {
            let session = &sessions[i];
            let (arrow, title) = if row == selected {
                (format!("{} ", icons().pick), pick)
            } else {
                ("  ".to_string(), Style::new().fg(Color::Blue))
            };
            let active = if session.id == picker.current {
                icons().ok
            } else {
                " "
            };
            let place = status::place(&session.cwd, home.as_deref());
            // Keep at least half the available columns for the title when
            // the directory is long, giving unused title room to the path.
            let fixed = BAR.width() + 2 + AGE_WIDTH + 1 + 2 + 2;
            let available = width.saturating_sub(fixed);
            let gap = 2.min(available.saturating_sub(1));
            let content = available - gap;
            let title_room = session.title.width().min(
                content
                    .saturating_sub(place.width())
                    .max(content.div_ceil(2)),
            );
            let text = clip(&session.title, title_room);
            let place_room = content.saturating_sub(text.width());
            let place_text = clip(&place, place_room);
            let hits = filter.indices(&haystack(session, home.as_deref()));
            let mut spans = vec![
                Span::styled(arrow, pick),
                Span::styled(
                    format!("{:>AGE_WIDTH$} ", age(now, session.updated_at)),
                    dim(),
                ),
                Span::styled(active, Style::new().fg(Color::Green)),
                Span::raw(" "),
                Span::styled(if session.renamed { icons().star } else { " " }, dim()),
                Span::raw(" "),
            ];
            spans.extend(fuzzy::highlight(text, &hits, 0, title, hit(title)));
            // Right-align the directory when it fits, as in the uncut list.
            spans.push(Span::raw(
                " ".repeat(gap + place_room.saturating_sub(place_text.width())),
            ));
            let offset = session.title.chars().count() + 1;
            spans.extend(fuzzy::highlight(
                place_text,
                &hits,
                offset,
                dim(),
                hit(dim()),
            ));
            panel_row(ACCENT, spans)
        })
        .collect()
}

/// The longest whole-grapheme prefix that fits in `width` terminal columns.
fn clip(text: &str, width: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (at, grapheme) in text.grapheme_indices(true) {
        let next = used + grapheme.width();
        if next > width {
            break;
        }
        used = next;
        end = at + grapheme.len();
    }
    &text[..end]
}

/// A matched character: `style`, underlined and bold.
fn hit(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{super::tests::summary, *};

    fn loading(sessions: Result<Vec<Summary>, String>) -> SessionPicker {
        let mut picker = SessionPicker::new(Uuid::new_v4());
        picker.load(sessions);
        picker
    }

    fn drawn(picker: &SessionPicker) -> Vec<String> {
        let State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } = &picker.state
        else {
            panic!("not ready");
        };
        rows(
            picker,
            Listed {
                sessions,
                filter,
                shown,
            },
            *selected,
            80,
            10,
        )
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
    }

    #[test]
    fn an_empty_list_is_neutral_and_a_query_without_matches_is_filtering() {
        let picker = loading(Ok(Vec::new()));
        assert!(matches!(picker.state, State::Ready { .. }), "not a failure");
        assert_eq!(drawn(&picker), ["▎   no saved sessions yet"]);

        let mut picker = loading(Ok(vec![summary("fix the parser", 1)]));
        picker.insert('z');
        assert_eq!(drawn(&picker), ["▎   no matches"]);
    }

    #[test]
    fn named_and_active_marks_are_independent_and_aligned_in_recency_order() {
        let first = summary("new title", 1);
        let mut second = summary("named title", 100);
        second.renamed = true;
        let mut third = summary("active named", 200);
        third.renamed = true;
        let mut picker = SessionPicker::new(third.id);
        picker.load(Ok(vec![first, second, third]));
        let lines = drawn(&picker);
        assert!(lines[0].contains("  new title"), "{:?}", lines);
        assert!(lines[1].contains("★ named title"), "{:?}", lines);
        assert!(lines[2].contains("✓ ★ active named"), "{:?}", lines);
        assert_eq!(
            lines[0].split("new title").next().unwrap().chars().count(),
            lines[1]
                .split("named title")
                .next()
                .unwrap()
                .chars()
                .count()
        );
    }

    #[test]
    fn filtering_highlights_title_and_directory_without_shifting_for_star() {
        let mut named = summary("parser", 1);
        named.renamed = true;
        named.cwd = "/unique-directory".into();
        let mut picker = loading(Ok(vec![named, summary("other", 2)]));
        for c in "parser".chars() {
            picker.insert(c);
        }
        let State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } = &picker.state
        else {
            panic!("not ready")
        };
        let lines = rows(
            &picker,
            Listed {
                sessions,
                filter,
                shown,
            },
            *selected,
            80,
            10,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|span| span.content.contains('p')
            && span.style.add_modifier.contains(Modifier::UNDERLINED)));
        assert!(drawn(&picker)[0].contains("★ parser"));

        picker.clear_query();
        for c in "unique".chars() {
            picker.insert(c);
        }
        let State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } = &picker.state
        else {
            panic!("not ready")
        };
        let lines = rows(
            &picker,
            Listed {
                sessions,
                filter,
                shown,
            },
            *selected,
            80,
            10,
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|span| span.content.contains('u')
            && span.style.add_modifier.contains(Modifier::UNDERLINED)));
    }

    #[test]
    fn narrow_rows_clip_titles_without_losing_marks_or_panicking() {
        let mut named = summary("a long title", 1);
        named.renamed = true;
        let mut picker = SessionPicker::new(named.id);
        picker.load(Ok(vec![named]));
        let State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } = &picker.state
        else {
            panic!("not ready")
        };
        for width in 0..30 {
            let lines = rows(
                &picker,
                Listed {
                    sessions,
                    filter,
                    shown,
                },
                *selected,
                width,
                10,
            );
            let text: String = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.contains("✓ ★ "));
            if width < 23 {
                assert!(!text.contains("a long title"));
            }
        }
    }

    #[test]
    fn long_unicode_path_leaves_room_for_title_marks_and_matches() {
        let mut named = summary("界面 title", 1);
        named.renamed = true;
        named.cwd = format!("/資料/{}", "long".repeat(30)).into();
        let mut picker = SessionPicker::new(named.id);
        picker.load(Ok(vec![named]));
        for c in "界資料".chars() {
            picker.insert(c);
        }
        let State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } = &picker.state
        else {
            panic!("not ready")
        };
        assert_eq!(shown.len(), 1);
        for width in 0..=50 {
            let lines = rows(
                &picker,
                Listed {
                    sessions,
                    filter,
                    shown,
                },
                *selected,
                width,
                10,
            );
            let line = &lines[0];
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            if width >= 13 {
                assert!(line.width() <= width, "width {width}: {text:?}");
            }
            if width == 40 {
                assert!(text.contains("✓ ★ 界面 title"), "{text:?}");
                assert!(text.contains("/資料/"), "{text:?}");
                for c in ['界', '資', '料'] {
                    assert!(line.spans.iter().any(|span| span.content.contains(c)
                        && span.style.add_modifier.contains(Modifier::UNDERLINED)));
                }
            }
        }
        assert_eq!(clip("界a", 1), "");
        assert_eq!(clip("界a", 2), "界");
        assert_eq!(clip("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn highlights_title_and_path_after_combining_graphemes() {
        let mut named = summary("e\u{301}x", 1);
        named.renamed = true;
        named.cwd = "/o\u{308}z".into();
        let mut picker = SessionPicker::new(named.id);
        picker.load(Ok(vec![named]));
        for c in "xz".chars() {
            picker.insert(c);
        }
        let State::Ready {
            sessions,
            filter,
            shown,
            selected,
        } = &picker.state
        else {
            panic!("not ready")
        };
        assert_eq!(shown.len(), 1);
        let lines = rows(
            &picker,
            Listed {
                sessions,
                filter,
                shown,
            },
            *selected,
            80,
            10,
        );
        let marked: String = lines[0]
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(marked, "xz");
    }
}
