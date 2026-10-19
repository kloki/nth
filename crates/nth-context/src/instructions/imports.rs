//! `@path` imports, as Claude Code reads them: a word starting with `@`
//! outside code names another file whose text goes in after the one that
//! names it. Words that name no file are left alone, since `@explore` is an
//! agent mention and `koen@example.com` an address.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::{Instruction, normalize};

/// How many files deep imports are followed, counting from the one read.
pub(super) const MAX_DEPTH: usize = 4;

/// Punctuation a sentence puts after a path, which is not part of it.
const TRAILING: [char; 5] = ['.', ',', ';', ':', ')'];

/// The paths named by `@` words in `text`, in order: each word that starts
/// with `@` at the start of a line or after whitespace, outside fenced code
/// blocks and inline code spans.
pub(super) fn find(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Between backticks is code: every odd piece of the split.
        for prose in line.split('`').step_by(2) {
            for word in prose.split_whitespace() {
                let Some(path) = word.strip_prefix('@') else {
                    continue;
                };
                let path = path.trim_end_matches(TRAILING);
                if !path.is_empty() {
                    found.push(path);
                }
            }
        }
    }
    found
}

/// The files `instruction` imports and theirs in turn, each followed by its
/// own imports, down to `MAX_DEPTH`. `claim` decides whether a path
/// is read at all, so a file comes once however often it is named; it is
/// asked before reading, so callers sharing a set never both take a file.
pub(super) fn expand(
    instruction: &Instruction,
    home: Option<&Path>,
    claim: &mut dyn FnMut(&Path) -> bool,
    warnings: &mut Vec<String>,
) -> Vec<Instruction> {
    let mut out = Vec::new();
    walk(instruction, 1, home, claim, warnings, &mut out);
    out
}

fn walk(
    instruction: &Instruction,
    depth: usize,
    home: Option<&Path>,
    claim: &mut dyn FnMut(&Path) -> bool,
    warnings: &mut Vec<String>,
    out: &mut Vec<Instruction>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let dir = instruction.path.parent().unwrap_or(Path::new("/"));
    for path in find(&instruction.content) {
        let Some(path) = resolve(path, dir, home) else {
            continue;
        };
        if !claim(&path) {
            continue;
        }
        if let Some(imported) = super::read(path, warnings) {
            out.push(imported.clone());
            walk(&imported, depth + 1, home, claim, warnings, out);
        }
    }
}

/// `raw` as a file: a leading `~` is the home directory, anything else is
/// relative to `dir`, the importing file's folder. `None` when that is no
/// file.
fn resolve(raw: &str, dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let path = crate::expand_home(raw, home).unwrap_or_else(|| dir.join(raw));
    let path = normalize(&path);
    path.is_file().then_some(path)
}

/// A claim that takes each path once, for a walk that shares no set.
pub(super) fn once(seen: &mut BTreeSet<PathBuf>) -> impl FnMut(&Path) -> bool + '_ {
    move |path| seen.insert(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_starting_with_at_are_imports() {
        let text = "@first.md\nSee @docs/a.md, then @~/b.md.\n- @c.md";
        assert_eq!(find(text), ["first.md", "docs/a.md", "~/b.md", "c.md"]);
    }

    #[test]
    fn code_and_addresses_are_not_imports() {
        let text = "\
Mail koen@example.com or run `cat @x.md` here.
```sh
echo @y.md
```
~~~
@z.md
~~~
@after.md";
        assert_eq!(find(text), ["after.md"]);
    }

    #[test]
    fn a_bare_at_is_nothing() {
        assert_eq!(find("@ and @. alone"), Vec::<&str>::new());
    }
}
