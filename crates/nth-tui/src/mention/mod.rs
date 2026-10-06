//! Mentions: an `@` starting any word of the prompt, completed to an agent
//! the model can delegate to, or by fuzzy matching to a file under the
//! working directory. Agents come first: there are few, and one is what
//! `@` at the start of a prompt usually means.

mod index;

pub use index::walk;
use nth_context::Agents;
use nucleo_matcher::{
    Config, Matcher,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};

/// Rows in the popup; typing narrows the list rather than scrolling it.
pub const LIMIT: usize = 8;
/// The most characters of an agent's description a row shows.
const ABOUT_CHARS: usize = 48;

/// One row of the `@` popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Agent { name: String, about: String },
    File(String),
}

impl Item {
    /// What `@` is completed to.
    pub fn name(&self) -> &str {
        match self {
            Item::Agent { name, .. } => name,
            Item::File(path) => path,
        }
    }
}

/// The best `LIMIT` rows for `query`: the agents whose name starts with
/// it, then the files that match it. A query with a `/` is a path.
pub fn items(agents: &Agents, files: &[String], query: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    if !query.contains('/') {
        let query = query.to_lowercase();
        items.extend(
            agents
                .iter()
                .filter(|agent| agent.name.to_lowercase().starts_with(&query))
                .map(|agent| Item::Agent {
                    name: agent.name.clone(),
                    about: about(agent.description.as_deref().unwrap_or("agent")),
                }),
        );
    }
    let room = LIMIT.saturating_sub(items.len());
    items.extend(matches(files, query, room).into_iter().map(Item::File));
    items
}

/// How `items` show in the popup: `@name` with the description lined up
/// for an agent, the path alone for a file.
pub fn rows(items: &[Item]) -> Vec<(String, &str)> {
    let width = items
        .iter()
        .filter_map(|item| match item {
            Item::Agent { name, .. } => Some(name.len()),
            Item::File(_) => None,
        })
        .max()
        .unwrap_or(0)
        + 1;
    items
        .iter()
        .map(|item| match item {
            Item::Agent { name, about } => (format!("@{name:<width$} "), about.as_str()),
            Item::File(path) => (path.clone(), ""),
        })
        .collect()
}

/// The first line of a description, short enough for the popup.
fn about(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default();
    match line.char_indices().nth(ABOUT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// The mention being typed: `start` is the byte offset of its `@`.
#[derive(Debug, PartialEq)]
pub struct Mention<'a> {
    pub start: usize,
    pub query: &'a str,
}

/// The mention ending at `cursor`, if the word it sits in starts with `@`.
/// Requiring `@` at the start of a word keeps `me@example.com` out.
pub fn find(text: &str, cursor: usize) -> Option<Mention<'_>> {
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let query = before[start..].strip_prefix('@')?;
    Some(Mention { start, query })
}

/// The best `limit` files for `query`, shorter paths first among equals.
pub fn matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let pattern = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut scored = pattern.match_list(files, &mut matcher);
    scored.sort_by(|(a, x), (b, y)| {
        y.cmp(x)
            .then_with(|| a.len().cmp(&b.len()))
            .then_with(|| a.cmp(b))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(file, _)| file.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Option<Mention<'_>> {
        find(text, text.len())
    }

    #[test]
    fn finds_a_mention_at_the_start_of_any_word() {
        assert_eq!(
            at("@ke"),
            Some(Mention {
                start: 0,
                query: "ke"
            })
        );
        assert_eq!(
            at("see @src/ke"),
            Some(Mention {
                start: 4,
                query: "src/ke"
            })
        );
        assert_eq!(
            at("one\n@"),
            Some(Mention {
                start: 4,
                query: ""
            })
        );
        assert_eq!(at("mail me@example.com"), None);
        assert_eq!(at("@ke done"), None);
    }

    #[test]
    fn only_looks_behind_the_cursor() {
        assert_eq!(
            find("see @keys and more", 7),
            Some(Mention {
                start: 4,
                query: "ke"
            })
        );
        assert_eq!(find("see @keys and more", 12), None);
    }

    #[test]
    fn agents_come_before_files_and_a_path_skips_them() {
        let agents = nth_context::Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        )
        .agents;
        let files: Vec<String> = ["explorer.rs", "src/general.rs"].map(String::from).into();

        let all = items(&agents, &files, "");
        assert_eq!(all.len(), 4);
        assert!(matches!(&all[0], Item::Agent { name, .. } if name == "explore"));
        assert!(matches!(&all[1], Item::Agent { name, .. } if name == "general"));
        assert_eq!(all[2], Item::File("explorer.rs".into()));
        let rows = rows(&all);
        assert_eq!(rows[0].0, "@explore  ", "names lined up, then a space");
        assert!(
            rows[0]
                .1
                .starts_with("Fast agent specialized for exploring codebases.")
        );
        assert_eq!(rows[2], ("explorer.rs".to_string(), ""));

        let ex = items(&agents, &files, "ex");
        assert!(matches!(&ex[0], Item::Agent { name, .. } if name == "explore"));
        assert_eq!(ex[1], Item::File("explorer.rs".into()));
        // An agent's name matches whatever the case; a file's follows the
        // fuzzy matcher's smart case.
        assert!(matches!(
            &items(&agents, &files, "Ex")[0],
            Item::Agent { .. }
        ));
        assert_eq!(
            items(&agents, &files, "src/gen"),
            [Item::File("src/general.rs".into())]
        );
    }

    #[test]
    fn ranks_fuzzy_matches() {
        let files: Vec<String> = [
            "crates/nth-tui/src/app/keys.rs",
            "crates/nth-tui/src/app/mod.rs",
            "README.md",
            "Cargo.toml",
        ]
        .map(String::from)
        .into();

        assert_eq!(
            matches(&files, "keys", LIMIT),
            ["crates/nth-tui/src/app/keys.rs"]
        );
        assert_eq!(matches(&files, "", 2), ["README.md", "Cargo.toml"]);
        assert!(matches(&files, "zzz", LIMIT).is_empty());
    }
}
