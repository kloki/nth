//! Text the terminal listings share.

use std::{collections::BTreeMap, path::Path};

use nth_icons::icons;

/// Longest description shown in a terminal listing.
const DESCRIPTION_CHARS: usize = 72;

/// The first line of `text`, cut to fit one terminal row.
pub fn shorten(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

/// `path` with `home` written as `~`, as the status bar shows it.
pub fn place(path: &Path, home: Option<&Path>) -> String {
    match home {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display())
                .trim_end_matches('/')
                .to_string(),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// The guide before child `i` of `len` in a tree: an elbow for the last,
/// a tee for the others. Unstyled, so each listing styles it its own way.
pub fn branch(i: usize, len: usize) -> String {
    let tree = &icons().tree;
    let joint = match i + 1 == len {
        true => tree.elbow,
        false => tree.tee,
    };
    format!("{joint}{}", tree.dash)
}

/// `items` grouped by `key`, the groups in key order and each keeping the
/// order `items` came in. `nth skills` and `nth agents` key on source and
/// root, which makes `global` granular: `~/.claude/skills` and
/// `~/.config/nth/skills` are separate groups.
pub fn groups<'a, T, K: Ord>(
    items: impl IntoIterator<Item = &'a T>,
    key: impl Fn(&'a T) -> K,
) -> BTreeMap<K, Vec<&'a T>> {
    let mut groups: BTreeMap<K, Vec<&'a T>> = BTreeMap::new();
    for item in items {
        groups.entry(key(item)).or_default().push(item);
    }
    groups
}

/// `dir` below `root` when it sits more than one level down (e.g.
/// `synced/<id>/diagram`), so a group's heading alone does not hide where
/// an entry came from. One directly under its root needs no folder: its
/// name is the folder.
pub fn nested_folder(dir: Option<&Path>, root: Option<&Path>) -> Option<String> {
    let dir = dir?;
    let root = root?;
    let folder = dir.strip_prefix(root).unwrap_or(dir);
    (folder.components().count() > 1).then(|| folder.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_uses_only_the_first_line_and_cuts_after_72_characters() {
        assert_eq!(shorten(""), "");
        assert_eq!(shorten("\nsecond"), "");
        assert_eq!(shorten("first\r\nsecond"), "first");
        assert_eq!(shorten(&"a".repeat(71)), "a".repeat(71));
        assert_eq!(shorten(&"a".repeat(72)), "a".repeat(72));
        assert_eq!(shorten(&"a".repeat(73)), format!("{}…", "a".repeat(72)));
    }

    #[test]
    fn shorten_counts_unicode_characters_without_splitting_utf8() {
        let line = format!("{}界🙂extra", "é".repeat(71));
        assert_eq!(shorten(&line), format!("{}界…", "é".repeat(71)));
        assert_eq!(shorten(&"🙂".repeat(72)), "🙂".repeat(72));
    }

    #[test]
    fn place_writes_home_as_a_tilde() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(
            place(Path::new("/home/u/.claude/skills"), home),
            "~/.claude/skills"
        );
        assert_eq!(place(Path::new("/home/u"), home), "~");
        assert_eq!(
            place(Path::new("/repo/.nth/skills"), home),
            "/repo/.nth/skills"
        );
        assert_eq!(place(Path::new("/home/u/x"), None), "/home/u/x");
        assert_eq!(
            place(Path::new("/home/u/x"), Some(Path::new(""))),
            "/home/u/x"
        );
    }

    #[test]
    fn branch_is_a_tee_for_a_middle_child_and_an_elbow_for_the_last() {
        assert_eq!(branch(0, 3), "├─");
        assert_eq!(branch(1, 3), "├─");
        assert_eq!(branch(2, 3), "└─");
        assert_eq!(branch(0, 1), "└─");
    }

    #[test]
    fn groups_are_in_key_order_and_keep_the_items_order() {
        let items = ["b1", "a1", "b2", "a2"];
        let groups = groups(&items, |item| &item[..1]);

        let groups: Vec<_> = groups.into_iter().collect();
        assert_eq!(
            groups,
            [("a", vec![&"a1", &"a2"]), ("b", vec![&"b1", &"b2"])]
        );
    }

    #[test]
    fn nested_folder_is_named_only_more_than_one_level_down() {
        let root = Some(Path::new("/home/u/.claude/skills"));
        let at = |dir: &str| nested_folder(Some(Path::new(dir)), root);
        assert_eq!(at("/home/u/.claude/skills/diagram"), None);
        assert_eq!(
            at("/home/u/.claude/skills/synced/1/diagram").as_deref(),
            Some("synced/1/diagram")
        );
        assert_eq!(nested_folder(None, root), None);
        assert_eq!(nested_folder(Some(Path::new("/a/b/c")), None), None);
    }
}
