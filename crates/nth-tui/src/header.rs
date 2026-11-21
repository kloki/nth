//! The header above the content panel: one line with the content panel's
//! tabs on the left, each coloured by how it is doing and the ones showing
//! bracketed, and the app's name and version on the right. When the panel
//! splits, the chat's tab sits over the left pane and the others start
//! over the right one, with the focused tab in bold.

use nth_icons::icons;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{app::TabState, theme};

/// Always this tall.
pub const ROWS: u16 = 1;

/// One tab as the header shows it.
pub struct TabLabel {
    pub icon: &'static str,
    pub name: String,
    pub state: TabState,
    /// A pane shows it.
    pub shown: bool,
    /// It has the focus, which only needs saying when two tabs show.
    pub focused: bool,
}

impl TabLabel {
    /// `icon name`, inside `[ ]` when showing and inside spaces otherwise,
    /// so moving between tabs never shifts the strip.
    fn text(&self) -> String {
        let (open, close) = if self.shown { ("[", "]") } else { (" ", " ") };
        format!("{open}{} {}{close}", self.icon, self.name)
    }

    fn span(&self, text: String) -> Span<'static> {
        let mut style = match theme::tab_colour(self.state) {
            Some(colour) => Style::new().fg(colour),
            None => Style::new(),
        };
        if self.focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        Span::styled(text, style)
    }
}

/// Shorten on grapheme boundaries, measured in terminal columns.
pub(crate) fn ellipsize(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }

    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let w = grapheme.width();
        if used + w > width - 1 {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out.push('…');
    out
}

pub(crate) fn clean_title(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// A single geometry for rendering and mouse targets. The version owns its
/// columns first; each run then shows a window containing its focused tab.
fn layout(
    tabs: &[TabLabel],
    area: Rect,
    split: Option<u16>,
) -> (Vec<Rect>, Vec<Rect>, Rect, Vec<String>) {
    let name_width = u16::try_from(concat!("nth ", env!("CARGO_PKG_VERSION")).width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let name = Rect::new(area.right() - name_width, area.y, name_width, area.height);
    let tabs_right = name
        .x
        .saturating_sub(u16::from(name_width > 0 && name.x > area.x));
    let mut areas = vec![Rect::new(area.x, area.y, 0, area.height); tabs.len()];
    let mut indicators = Vec::new();
    let mut texts: Vec<String> = tabs.iter().map(TabLabel::text).collect();
    let runs = match split {
        Some(x) if x > area.x && x < tabs_right => vec![
            (area.x, x, 0, tabs.len().min(1)),
            (x, tabs_right, tabs.len().min(1), tabs.len()),
        ],
        _ => vec![(area.x, tabs_right, 0, tabs.len())],
    };
    for (left, right, start, end) in runs {
        if start == end || left >= right {
            continue;
        }
        if start == 0 {
            let tab = &tabs[0];
            let room = usize::from(right - left);
            let overhead = tab.icon.width() + 3;
            let other = if end > 1 {
                texts[1].width().min(room.saturating_sub(overhead + 1))
            } else {
                0
            };
            let overflow = usize::from(end > 2);
            let budget = room.saturating_sub(other + overflow + overhead).min(40);
            let name = ellipsize(&clean_title(&tab.name), budget);
            let (open, close) = if tab.shown { ("[", "]") } else { (" ", " ") };
            texts[0] = format!("{open}{} {name}{close}", tab.icon);
        }
        let width = |i: usize| u16::try_from(texts[i].width()).unwrap_or(u16::MAX);
        let total = (start..end).fold(0u16, |n, i| n.saturating_add(width(i)));
        let room = right - left;
        let focus = (start..end)
            .find(|&i| tabs[i].focused)
            .or_else(|| (start..end).find(|&i| tabs[i].shown))
            .unwrap_or(start);
        let overflow = total > room;
        let usable = room.saturating_sub(u16::from(overflow && room > 1));
        let mut first = start;
        if overflow {
            first = focus;
            let mut used = width(focus).min(usable);
            while first > start && used.saturating_add(width(first - 1)) <= usable {
                first -= 1;
                used += width(first);
            }
        }
        let mut x = left;
        for (i, slot) in areas.iter_mut().enumerate().take(end).skip(first) {
            let w = width(i).min(left.saturating_add(usable).saturating_sub(x));
            *slot = Rect::new(x, area.y, w, area.height);
            x += w;
            if x >= left + usable {
                break;
            }
        }
        if overflow && room > 1 {
            indicators.push(Rect::new(right - 1, area.y, 1, area.height));
        }
    }
    (areas, indicators, name, texts)
}

/// Draws the same clipped tab areas used for mouse hit testing.
pub fn draw(frame: &mut Frame, area: Rect, tabs: &[TabLabel], split: Option<u16>) {
    let (areas, indicators, name, texts) = layout(tabs, area, split);
    for ((tab, area), text) in tabs.iter().zip(areas).zip(texts) {
        frame.render_widget(Paragraph::new(tab.span(text)), area);
    }
    for area in indicators {
        frame.render_widget(Paragraph::new(icons().overflow), area);
    }
    let title = Line::from(vec![
        Span::styled(
            "nth",
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(concat!(" ", env!("CARGO_PKG_VERSION")), theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(title.right_aligned()), name);
}

/// Visible rectangles indexed by the original tab indices.
pub fn tab_areas(tabs: &[TabLabel], area: Rect, split: Option<u16>) -> Vec<Rect> {
    layout(tabs, area, split).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Vec<TabLabel> {
        vec![
            TabLabel {
                icon: "›",
                name: "chat".into(),
                state: TabState::Idle,
                shown: true,
                focused: false,
            },
            TabLabel {
                icon: "≡",
                name: "diagnostics".into(),
                state: TabState::Idle,
                shown: false,
                focused: false,
            },
        ]
    }

    #[test]
    fn each_tab_has_the_columns_it_is_drawn_over() {
        let area = Rect::new(1, 0, 40, 1);
        // `[› chat]` is 8 columns from x = 1, then ` ≡ diagnostics `.
        assert_eq!(
            tab_areas(&labels(), area, None),
            [Rect::new(1, 0, 8, 1), Rect::new(9, 0, 15, 1)]
        );
    }

    #[test]
    fn split_the_other_tabs_start_over_the_right_pane() {
        let area = Rect::new(1, 0, 50, 1);
        // `[› chat]` from x = 1, then ` ≡ diagnostics ` from x = 21.
        assert_eq!(
            tab_areas(&labels(), area, Some(21)),
            [Rect::new(1, 0, 8, 1), Rect::new(21, 0, 15, 1)]
        );
    }

    #[test]
    fn overflow_keeps_focused_tab_visible_with_original_index() {
        let mut tabs = labels();
        for i in 0..12 {
            tabs.push(TabLabel {
                icon: "x",
                name: format!("tab{i}"),
                state: TabState::Idle,
                shown: false,
                focused: false,
            });
        }
        tabs[13].focused = true;
        tabs[13].shown = true;
        for split in [None, Some(15)] {
            let (areas, indicators, name, _) = layout(&tabs, Rect::new(0, 0, 40, 1), split);
            assert!(areas[13].width > 0);
            assert!(!indicators.is_empty());
            assert!(areas.iter().all(|r| r.width == 0 || r.right() <= name.x));
        }
    }

    #[test]
    fn tabs_do_not_overlap_the_version() {
        let area = Rect::new(0, 0, 12, 1);
        let (tabs, _, name, _) = layout(&labels(), area, None);
        assert!(
            tabs.iter()
                .all(|tab| tab.width == 0 || tab.right() <= name.x)
        );
    }

    #[test]
    fn title_is_grapheme_safe_and_leaves_the_next_tab_clickable() {
        let mut tabs = labels();
        tabs[0].name = "界e\u{301}".repeat(30);
        for (area, split) in [
            (Rect::new(0, 0, 44, 1), None),
            (Rect::new(0, 0, 80, 1), Some(25)),
        ] {
            let (areas, _, _, texts) = layout(&tabs, area, split);
            assert!(texts[0].ends_with("…]"), "{}", texts[0]);
            assert!(texts[0].contains("e\u{301}"));
            assert_eq!(usize::from(areas[0].width), texts[0].width());
            assert!(areas[1].width > 0);
            assert!(areas[0].right() <= areas[1].x);
            assert_eq!(tab_areas(&tabs, area, split), areas);
        }
    }

    #[test]
    fn every_width_draws_tabs_inside_the_header_and_never_over_the_version() {
        let mut tabs = labels();
        for i in 0..6 {
            tabs.push(TabLabel {
                icon: "x",
                name: format!("tab{i}"),
                state: TabState::Idle,
                shown: false,
                focused: false,
            });
        }
        for focus in [0, 3, tabs.len() - 1] {
            for tab in &mut tabs {
                tab.focused = false;
            }
            tabs[focus].focused = true;
            tabs[focus].shown = true;
            for width in 0..60 {
                let area = Rect::new(1, 0, width, 1);
                for split in [None, Some(1), Some(area.x + width / 2), Some(area.right())] {
                    let (areas, indicators, name, _) = layout(&tabs, area, split);
                    for draw in areas.iter().chain(&indicators).filter(|r| r.width > 0) {
                        assert!(
                            draw.x >= area.x && draw.right() <= area.right(),
                            "{draw:?} outside {area:?} at split {split:?}"
                        );
                        assert!(
                            draw.right() <= name.x,
                            "{draw:?} over the version at {name:?}, width {width}, split {split:?}"
                        );
                    }
                    let mut drawn: Vec<Rect> = areas
                        .iter()
                        .chain(&indicators)
                        .copied()
                        .filter(|r| r.width > 0)
                        .collect();
                    drawn.sort_by_key(|r| r.x);
                    for pair in drawn.windows(2) {
                        assert!(
                            pair[0].right() <= pair[1].x,
                            "{:?} overlaps {:?} at width {width}, split {split:?}",
                            pair[0],
                            pair[1]
                        );
                    }
                }
            }
        }
    }
}
