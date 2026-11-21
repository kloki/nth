//! Searchable profile selection in the input panel.
//!
//! The picker owns a snapshot of the profiles. Applying a choice must still
//! check availability against the current connectivity in the caller.

use nth_config::Profile;
use nth_icons::icons;
use nth_protocol::Llm;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    fuzzy::{self, Filter},
    theme::{self, BAR_WIDTH, dim, panel_row, panel_title, pick},
};

const ACCENT: Color = Color::Blue;
const TITLE: &str = "profiles";
const KEYS: &str = "type to filter · ↑↓ profile · ←→ slot · enter · esc";
const HEADERS: [&str; 5] = ["Profile", "Plan", "Act", "Subagent", "Clerk"];
const PREFIX: usize = 4; // arrow, active mark and their spaces
const GAP: usize = 2;

#[derive(Debug)]
pub struct ProfilePicker {
    profiles: Vec<Profile>,
    last: Option<usize>,
    active: Option<usize>,
    connected: bool,
    filter: Filter,
    shown: Vec<usize>,
    selected: usize,
    detail_slot: usize,
}

impl ProfilePicker {
    /// `last` is the last applied profile, even if its slots were customized;
    /// `active` marks it only while its slots still match. Indices refer to
    /// `profiles` in configuration order.
    pub fn new(profiles: &[Profile], last: Option<usize>, active: bool, connected: bool) -> Self {
        let last = last.filter(|&i| i < profiles.len());
        Self {
            profiles: profiles.to_vec(),
            last,
            active: if active { last } else { None },
            connected,
            filter: Filter::default(),
            shown: (0..profiles.len()).collect(),
            selected: last.unwrap_or(0),
            detail_slot: 0,
        }
    }

    pub fn next(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + 1) % self.shown.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + self.shown.len() - 1) % self.shown.len();
        }
    }

    pub fn insert(&mut self, c: char) {
        self.refilter(|f| f.push(c));
    }
    pub fn backspace(&mut self) {
        self.refilter(Filter::pop);
    }

    /// Clears a nonempty query, returning whether it was cleared.
    pub fn clear_query(&mut self) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        self.refilter(Filter::clear);
        true
    }

    fn refilter(&mut self, change: impl FnOnce(&mut Filter)) {
        change(&mut self.filter);
        self.shown = self
            .filter
            .rank(self.profiles.iter().map(|p| p.name.as_str()));
        self.selected = if self.filter.is_empty() {
            self.last.unwrap_or(0)
        } else {
            0
        };
    }

    /// Index of the highlighted profile, including unavailable profiles.
    pub fn chosen(&self) -> Option<usize> {
        self.shown.get(self.selected).copied()
    }

    /// Refresh connectivity while the picker is open.
    pub fn next_detail(&mut self, forward: bool) {
        self.detail_slot = (self.detail_slot + if forward { 1 } else { 3 }) % 4;
    }
}

fn reason(profile: &Profile, connected: bool) -> Option<&str> {
    profile.unavailable_reason(connected)
}

fn slot(llm: &Llm) -> String {
    format!("{} · {}", llm.model, llm.effort.wire().unwrap_or("default"))
}

fn cells(profile: &Profile) -> [String; 5] {
    [
        profile.name.clone(),
        slot(&profile.slots.plan),
        slot(&profile.slots.act),
        profile
            .slots
            .subagent
            .as_ref()
            .map(slot)
            .unwrap_or_else(|| "inherit current".into()),
        profile
            .slots
            .clerk
            .as_ref()
            .map(slot)
            .unwrap_or_else(|| "inherit subagent".into()),
    ]
}

/// Column widths are calculated from every profile, not just the matches.
/// On narrow screens only complete leading headers are offered.
fn widths(profiles: &[Profile], room: usize) -> Vec<usize> {
    let mut desired = HEADERS.map(str::len);
    for profile in profiles {
        for (i, cell) in cells(profile).iter().enumerate() {
            desired[i] = desired[i].max(cell.width());
        }
    }
    desired[0] = desired[0].min(24);
    let mut widths = Vec::new();
    let mut left = room;
    for header in HEADERS {
        let gap = if widths.is_empty() { 0 } else { GAP };
        if left < gap + header.len() {
            break;
        }
        left -= gap + header.len();
        widths.push(header.len());
    }
    // Give each column one cell at a time, redistributing unused space.
    while left > 0 {
        let mut grew = false;
        for (i, width) in widths.iter_mut().enumerate() {
            if left == 0 {
                break;
            }
            if *width < desired[i] {
                *width += 1;
                left -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    widths
}

/// Clip by terminal columns, leaving room for the ellipsis.
fn clipped(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        result.push(c);
        used += w;
    }
    result.push('…');
    result
}

fn padded(text: &str, width: usize) -> String {
    let text = clipped(text, width);
    let padding = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(padding))
}

fn table_header(widths: &[usize]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(PREFIX))];
    for (i, &width) in widths.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(GAP)));
        }
        spans.push(Span::styled(padded(HEADERS[i], width), dim()));
    }
    panel_row(ACCENT, spans)
}

fn row(picker: &ProfilePicker, index: usize, highlighted: bool, widths: &[usize]) -> Line<'static> {
    let profile = &picker.profiles[index];
    let disabled = reason(profile, picker.connected).is_some();
    let name_style = if disabled {
        dim()
    } else if highlighted {
        pick()
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(
            if highlighted {
                format!("{} ", icons().pick)
            } else {
                "  ".into()
            },
            pick(),
        ),
        Span::styled(
            if picker.active == Some(index) {
                icons().ok
            } else {
                " "
            },
            Style::new().fg(Color::Green),
        ),
        Span::raw(" "),
    ];
    let values = cells(profile);
    for (i, &width) in widths.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(GAP)));
        }
        let text = clipped(&values[i], width);
        let padding = width.saturating_sub(text.width());
        if i == 0 {
            spans.extend(fuzzy::highlight(
                &text,
                &picker.filter.indices(&profile.name),
                0,
                name_style,
                name_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else if disabled
            || i >= 3
                && ((i == 3 && profile.slots.subagent.is_none())
                    || (i == 4 && profile.slots.clerk.is_none()))
        {
            spans.push(Span::styled(text, dim()));
        } else {
            let llm = match i {
                1 => Some(&profile.slots.plan),
                2 => Some(&profile.slots.act),
                3 => profile.slots.subagent.as_ref(),
                _ => profile.slots.clerk.as_ref(),
            };
            if let Some(llm) = llm {
                // Clip styled model spans without losing the provider/effort colours.
                let truncated = values[i].width() > width;
                let content_room = text.width().saturating_sub(usize::from(truncated));
                let mut used = 0;
                let mut stopped = false;
                for span in
                    theme::model_spans(&llm.model, Some(llm.effort.wire().unwrap_or("default")))
                {
                    let part: String = span
                        .content
                        .chars()
                        .take_while(|c| {
                            let w = c.width().unwrap_or(0);
                            if used + w > content_room {
                                stopped = true;
                                false
                            } else {
                                used += w;
                                true
                            }
                        })
                        .collect();
                    if !part.is_empty() {
                        spans.push(Span::styled(part, span.style));
                    }
                    if stopped || used >= content_room {
                        break;
                    }
                }
                if truncated {
                    spans.push(Span::styled("…", dim()));
                }
            }
        }
        spans.push(Span::raw(" ".repeat(padding)));
    }
    panel_row(ACCENT, spans)
}

/// Draw within the input-panel area. The caller reserves a fixed
/// `min(16, screen.height / 2)` rows, as for the other pickers.
pub fn draw(frame: &mut Frame, area: Rect, picker: &ProfilePicker) {
    if area.is_empty() {
        return;
    }
    let line = |offset: u16| Rect::new(area.x, area.y.saturating_add(offset), area.width, 1);
    let header = line(0);
    frame.render_widget(Paragraph::new(panel_title(TITLE, ACCENT)), header);
    if usize::from(area.width) > usize::from(BAR_WIDTH) + TITLE.width() + KEYS.width() + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(KEYS, dim())).alignment(Alignment::Right),
            header,
        );
    }
    if area.height < 2 {
        return;
    }
    fuzzy::query_row(
        frame,
        line(1),
        ACCENT,
        picker.filter.query(),
        picker.shown.len(),
        picker.profiles.len(),
    );
    let remaining = area.height.saturating_sub(2);
    if remaining == 0 {
        return;
    }
    let table = remaining >= 3;
    let detail = remaining >= 3;
    let start = 2 + u16::from(table);
    let list_height = remaining - u16::from(table) - u16::from(detail);
    let widths = widths(
        &picker.profiles,
        usize::from(area.width).saturating_sub(usize::from(BAR_WIDTH) + PREFIX),
    );
    if table {
        frame.render_widget(Paragraph::new(table_header(&widths)), line(2));
    }
    if picker.shown.is_empty() {
        let text = if picker.profiles.is_empty() {
            "no profiles configured"
        } else {
            "no matches"
        };
        frame.render_widget(
            Paragraph::new(panel_row(
                ACCENT,
                [Span::styled(format!("  {text}"), dim())],
            )),
            line(start),
        );
    } else {
        let first = picker
            .selected
            .saturating_sub(usize::from(list_height).saturating_sub(1));
        for (offset, (at, &index)) in picker
            .shown
            .iter()
            .enumerate()
            .skip(first)
            .take(usize::from(list_height))
            .enumerate()
        {
            frame.render_widget(
                Paragraph::new(row(picker, index, at == picker.selected, &widths)),
                line(start + offset as u16),
            );
        }
    }
    if detail {
        let text = picker
            .chosen()
            .map(|i| {
                let profile = &picker.profiles[i];
                match reason(profile, picker.connected) {
                    Some(reason) => format!("{}: unavailable — {reason}", profile.name),
                    None => format!(
                        "{}: {} · {}  (←/→ slot)",
                        profile.name,
                        HEADERS[picker.detail_slot + 1],
                        cells(profile)[picker.detail_slot + 1]
                    ),
                }
            })
            .unwrap_or_default();
        let room = usize::from(area.width.saturating_sub(BAR_WIDTH));
        frame.render_widget(
            Paragraph::new(panel_row(
                ACCENT,
                [Span::styled(clipped(&text, room), dim())],
            )),
            line(area.height - 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use nth_config::Slots;
    use nth_protocol::Effort;

    use super::*;

    fn profile(name: &str) -> Profile {
        let llm = Llm {
            model: "go/model".into(),
            effort: Effort::Default,
        };
        Profile {
            name: name.into(),
            slots: Slots::same(llm),
            unavailable: None,
            needs_chatgpt: false,
        }
    }

    #[test]
    fn navigation_filter_and_connectivity() {
        let mut blocked = profile("deep");
        blocked.needs_chatgpt = true;
        let mut picker = ProfilePicker::new(&[profile("daily"), blocked], Some(1), false, false);
        assert_eq!(picker.chosen(), Some(1));
        assert_eq!(
            reason(&picker.profiles[1], picker.connected),
            Some("ChatGPT is not connected")
        );
        picker.next();
        assert_eq!(picker.chosen(), Some(0));
        picker.insert('d');
        picker.insert('e');
        assert_eq!(picker.chosen(), Some(1));
        assert!(picker.clear_query());
        assert_eq!(picker.chosen(), Some(1));
        assert!(!picker.clear_query());
    }

    #[test]
    fn widths_are_bounded_and_unicode_clips_safely() {
        let profiles = [profile("界界界")];
        for room in 0..100 {
            let widths = widths(&profiles, room);
            assert!(widths.iter().sum::<usize>() + GAP * widths.len().saturating_sub(1) <= room);
        }
        assert_eq!(clipped("界界", 3), "界…");
        assert_eq!(clipped("界", 1), "…");
        assert_eq!(cells(&profiles[0])[3], "inherit current");
    }
}
