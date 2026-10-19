//! The project's memory: what the model learned in earlier sessions about
//! the project and the person it works for, as Claude Code's auto memory
//! keeps it. One markdown file per fact under `.nth/memory/` in the main
//! checkout, so the session's worktrees share it, and `MEMORY.md`, the
//! index, one line per file. The index goes into the system prompt; the
//! model reads a file when the index says it matters, and writes them with
//! its ordinary tools.

use std::path::{Path, PathBuf};

use crate::{frontmatter, main_checkout, project_root};

/// The index, in the memory folder.
pub const INDEX: &str = "MEMORY.md";
/// At most this many lines of the index go into the system prompt, or
/// `INDEX_BYTES`, whichever ends first: Claude Code's limits.
pub const INDEX_LINES: usize = 200;
pub const INDEX_BYTES: usize = 25_000;

/// The memory of one project, as the system prompt shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub dir: PathBuf,
    /// The index as far as it fits; empty when there is none yet.
    pub index: String,
    /// The index went past `INDEX_LINES` or `INDEX_BYTES`.
    pub truncated: bool,
}

/// What one memory file is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Who the person is: their role, what they know, how they like to work.
    User,
    /// How they want the work done: corrections and approaches they
    /// confirmed.
    Feedback,
    /// Goals, constraints and work in flight the code does not show.
    Project,
    /// Where to find things outside the project.
    Reference,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::User, Kind::Feedback, Kind::Project, Kind::Reference];

    /// The `type` its frontmatter gives.
    pub fn name(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Feedback => "feedback",
            Kind::Project => "project",
            Kind::Reference => "reference",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == text)
    }
}

/// One memory file.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub kind: Kind,
}

/// The memory folder of the project at `cwd`: `.nth/memory` in the main
/// checkout when `cwd` is in a worktree of it, else in the project root,
/// else in `cwd`.
pub fn dir(cwd: &Path) -> PathBuf {
    let root = project_root(cwd);
    let main = root.as_deref().and_then(main_checkout);
    main.or(root)
        .unwrap_or_else(|| cwd.to_path_buf())
        .join(".nth/memory")
}

/// The memory of the project at `cwd`, its index cut to fit.
pub(crate) fn discover(cwd: &Path, warnings: &mut Vec<String>) -> Memory {
    let dir = dir(cwd);
    let index = dir.join(INDEX);
    let text = match std::fs::read_to_string(&index) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            warnings.push(format!("skipped {}: {e}", index.display()));
            String::new()
        }
    };
    let (index, truncated) = fit(&text);
    Memory {
        dir,
        index: index.to_string(),
        truncated,
    }
}

/// `text` up to `INDEX_LINES` lines or `INDEX_BYTES` bytes, ending at a
/// line, and whether anything was left out.
fn fit(text: &str) -> (&str, bool) {
    let mut end = 0;
    for (n, line) in text.split_inclusive('\n').enumerate() {
        if n == INDEX_LINES || end + line.len() > INDEX_BYTES {
            return (&text[..end], true);
        }
        end += line.len();
    }
    (text, false)
}

/// The memory files in `dir`, by kind and then name. A file whose
/// frontmatter cannot be read is a warning; the index is not a memory.
pub fn list(dir: &Path, warnings: &mut Vec<String>) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "md")
                && path.file_name().is_some_and(|name| name != INDEX)
        })
        .filter_map(|path| match entry(&path) {
            Ok(entry) => Some(entry),
            Err(e) => {
                warnings.push(format!("skipped {}: {e}", path.display()));
                None
            }
        })
        .collect();
    entries.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    entries
}

fn entry(path: &Path) -> Result<Entry, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (front, _) = frontmatter::parse(&text)?;
    let kind = frontmatter::field(&front, "type").ok_or("no type")?;
    Ok(Entry {
        path: path.to_path_buf(),
        name: frontmatter::field(&front, "name").ok_or("no name")?,
        description: frontmatter::field(&front, "description").unwrap_or_default(),
        kind: Kind::parse(&kind).ok_or_else(|| format!("unknown type {kind}"))?,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("has a parent")).expect("dirs");
        fs::write(path, text).expect("writes");
    }

    #[test]
    fn the_folder_is_in_the_project_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        write(&repo.join(".git/HEAD"), "");
        fs::create_dir_all(repo.join("crates/a")).expect("dirs");

        assert_eq!(dir(&repo.join("crates/a")), repo.join(".nth/memory"));
    }

    #[test]
    fn a_worktree_shares_its_main_checkouts_folder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let worktree = repo.join(".nth/worktrees/x");
        write(&repo.join(".git/HEAD"), "");
        let gitdir = repo.join(".git/worktrees/x");
        write(
            &worktree.join(".git"),
            &format!("gitdir: {}\n", gitdir.display()),
        );

        assert_eq!(dir(&worktree), repo.join(".nth/memory"));
    }

    #[test]
    fn outside_a_repository_the_folder_is_in_the_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");

        assert_eq!(dir(tmp.path()), tmp.path().join(".nth/memory"));
    }

    #[test]
    fn the_index_is_cut_by_lines() {
        let text: String = (0..250).map(|n| format!("- line {n}\n")).collect();
        let (fitted, truncated) = fit(&text);

        assert!(truncated);
        assert_eq!(fitted.lines().count(), INDEX_LINES);
        assert!(fitted.ends_with("- line 199\n"));
    }

    #[test]
    fn the_index_is_cut_by_bytes_at_a_line() {
        let line = format!("- {}\n", "x".repeat(997));
        let text = line.repeat(30);
        let (fitted, truncated) = fit(&text);

        assert!(truncated);
        assert_eq!(fitted.len(), 25 * line.len());
    }

    #[test]
    fn a_short_index_is_whole() {
        assert_eq!(fit("- a\n- b"), ("- a\n- b", false));
    }

    #[test]
    fn discover_reads_the_index_or_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut warnings = Vec::new();
        assert_eq!(discover(tmp.path(), &mut warnings).index, "");

        write(
            &tmp.path().join(".nth/memory/MEMORY.md"),
            "- [A](a.md) — a\n",
        );
        let memory = discover(tmp.path(), &mut warnings);

        assert_eq!(memory.index, "- [A](a.md) — a\n");
        assert!(!memory.truncated);
        assert_eq!(warnings, Vec::<String>::new());
    }

    #[test]
    fn list_reads_each_memorys_frontmatter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        write(&dir.join("MEMORY.md"), "- index\n");
        write(
            &dir.join("terse.md"),
            "---\nname: terse-commits\ndescription: short commit messages\ntype: feedback\n---\n\nKeep them short.\n",
        );
        write(
            &dir.join("role.md"),
            "---\nname: role\ndescription: maintains nth\ntype: user\n---\n",
        );
        write(&dir.join("broken.md"), "---\nname: x\n---\n");
        write(&dir.join("notes.txt"), "not a memory");
        let mut warnings = Vec::new();

        let entries = list(dir, &mut warnings);

        let names: Vec<(&str, Kind)> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            [("role", Kind::User), ("terse-commits", Kind::Feedback)]
        );
        assert_eq!(entries[1].description, "short commit messages");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("broken.md: no type"), "{warnings:?}");
    }
}
