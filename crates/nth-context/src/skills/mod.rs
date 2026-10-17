//! Skills: folders with a `SKILL.md`, whose frontmatter names and describes
//! the skill and whose body tells the model how to do the task. nth reads
//! them where Claude Code, opencode and the open standard keep them, so a
//! skill written for any of those works here too.

mod internal;
mod template;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub use template::parse;

use crate::{Paths, frontmatter, project_root};

const FILE: &str = "SKILL.md";
/// What the model gets when a skill is loaded, as in opencode.
const CONTENT: &str = include_str!("content.md");
/// The same for an internal skill, which has no folder or files beside it.
const INTERNAL_CONTENT: &str = include_str!("internal/content.md");
/// Files listed next to a skill's body, so the model knows what it can
/// read or run without listing the folder first.
const MAX_FILES: usize = 10;
/// Deep enough for `skills/<group>/<skill>/SKILL.md`, shallow enough that a
/// skill's own references are not walked for long.
const MAX_DEPTH: usize = 4;

/// Skill folders inside a project directory, in rising precedence.
const PROJECT_DIRS: [&str; 5] = [
    ".claude/skills",
    ".opencode/skill",
    ".opencode/skills",
    ".agents/skills",
    ".nth/skills",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Shipped inside nth, about using nth itself.
    Internal,
    /// The user's own folders, for every project.
    Global,
    /// Checked into the project, from its root down to the working
    /// directory.
    Project,
    /// Listed under `[skills] paths` in the config.
    Config,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Internal => "internal",
            Source::Global => "global",
            Source::Project => "project",
            Source::Config => "config",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    /// What the model reads to decide whether to load the skill. A skill
    /// without one can be run by name but is not offered to the model.
    pub description: Option<String>,
    pub location: Location,
    pub source: Source,
    /// The folder searched that it was found under, e.g. `~/.claude/skills`;
    /// `None` for an internal skill.
    pub root: Option<PathBuf>,
}

/// Where a skill's `SKILL.md` is.
#[derive(Debug, Clone, PartialEq)]
pub enum Location {
    File(PathBuf),
    /// Its whole text, shipped inside nth.
    Internal(&'static str),
}

impl Skill {
    /// The `SKILL.md` it was read from; internal skills have none.
    pub fn path(&self) -> Option<&Path> {
        match &self.location {
            Location::File(path) => Some(path),
            Location::Internal(_) => None,
        }
    }

    /// The skill's folder, which relative paths in its body start from;
    /// internal skills have none.
    pub fn dir(&self) -> Option<&Path> {
        self.path()
            .map(|path| path.parent().unwrap_or(Path::new(".")))
    }

    /// The skill's body with where it lives and the files beside it, read
    /// afresh so edits since startup count.
    pub async fn render(&self) -> Result<String, String> {
        let skill = self.clone();
        crate::blocking(move || skill.render_blocking()).await
    }

    fn render_blocking(&self) -> Result<String, String> {
        Ok(self.render_body(&self.body()?))
    }

    /// The `SKILL.md` without its frontmatter.
    fn body(&self) -> Result<String, String> {
        let text = match &self.location {
            Location::File(path) => std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
            Location::Internal(text) => text.to_string(),
        };
        let (_, body) = frontmatter::split(&text)?;
        Ok(body.to_string())
    }

    /// `body` wrapped with where the skill lives and the files beside it.
    fn render_body(&self, body: &str) -> String {
        let Some(dir) = self.dir() else {
            return INTERNAL_CONTENT
                .replace("{name}", &self.name)
                .replace("{body}", body.trim());
        };
        let files: Vec<String> = files(dir)
            .iter()
            .map(|file| format!("<file>{}</file>", file.display()))
            .collect();
        let dir = dir.display().to_string();
        // Body last, so placeholders inside it are left alone.
        CONTENT
            .replace("{name}", &self.name)
            .replace("{dir}", &dir)
            .replace("{files}", &files.join("\n"))
            .replace("{body}", body.trim())
    }
}

/// Up to `MAX_FILES` files in `dir` other than the `SKILL.md`, walked
/// depth-first in name order so the same ones are listed on every run.
fn files(dir: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(dir)
        .hidden(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter(|entry| entry.path() != dir.join(FILE))
        .take(MAX_FILES)
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// Every skill found, by name. When two share a name, the one found later
/// wins: a file over an internal skill, project over global, config over
/// both, nearer over further.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Skills(BTreeMap<String, Skill>);

impl Skills {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.0.get(name)
    }

    /// In name order.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.0.values()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn insert(&mut self, skill: Skill) {
        self.0.insert(skill.name.clone(), skill);
    }
}

pub(crate) fn discover(cwd: &Path, paths: &Paths, warnings: &mut Vec<String>) -> Skills {
    let mut skills = Skills::default();
    for skill in internal::skills() {
        skills.insert(skill);
    }
    for (root, source) in roots(cwd, paths) {
        for file in find(&root) {
            match read(&file, &root, source) {
                Ok(skill) => skills.insert(skill),
                Err(e) => warnings.push(format!("skipped skill {}: {e}", file.display())),
            }
        }
    }
    skills
}

/// The folders to search, lowest precedence first.
fn roots(cwd: &Path, paths: &Paths) -> Vec<(PathBuf, Source)> {
    let mut roots = Vec::new();
    if let Some(home) = &paths.home {
        roots.push(home.join(".claude/skills"));
    }
    if let Some(config_home) = &paths.config_home {
        roots.push(config_home.join("opencode/skill"));
        roots.push(config_home.join("opencode/skills"));
    }
    if let Some(home) = &paths.home {
        roots.push(home.join(".agents/skills"));
    }
    if let Some(config_dir) = paths.config_dir() {
        roots.push(config_dir.join("skills"));
    }
    let mut roots: Vec<_> = roots.into_iter().map(|r| (r, Source::Global)).collect();

    let mut dirs: Vec<&Path> = match project_root(cwd) {
        Some(root) => cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(&root))
            .collect(),
        None => vec![cwd],
    };
    dirs.reverse();
    for dir in dirs {
        roots.extend(PROJECT_DIRS.iter().map(|d| (dir.join(d), Source::Project)));
    }

    roots.extend(
        paths
            .skill_paths
            .iter()
            .map(|path| (cwd.join(path), Source::Config)),
    );
    // Run from the home directory, its skill folders are global and
    // project ones at once; they count once, as global.
    let mut seen = std::collections::BTreeSet::new();
    roots.retain(|(root, _)| seen.insert(root.clone()));
    roots
}

/// Every `SKILL.md` under `root`, in path order so discovery is the same on
/// every run. Ignore files are not applied: a skill folder that git ignores
/// is still a skill. Hidden folders are skipped, so a `.trash` beside real
/// skills does not list what was deleted.
fn find(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(true)
        .follow_links(true)
        .max_depth(Some(MAX_DEPTH))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() == FILE && entry.path().is_file())
        .map(ignore::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

fn read(file: &Path, root: &Path, source: Source) -> Result<Skill, String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let (front, _body) = frontmatter::split(&text)?;
    let name = match front.name {
        Some(name) => name,
        None => file
            .parent()
            .and_then(Path::file_name)
            .map(|dir| dir.to_string_lossy().into_owned())
            .ok_or("no name in its frontmatter")?,
    };
    // A name is typed after `/`, so it has to be one word.
    if name.contains(|c: char| c.is_whitespace() || c == '/') {
        return Err(format!("name {name:?} has spaces or slashes"));
    }
    Ok(Skill {
        name,
        description: front.description,
        location: Location::File(file.to_path_buf()),
        source,
        root: Some(root.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("tempdir"))
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.path().join(rel)
        }

        fn skill(&self, dir: &str, front: &str) -> &Self {
            let path = self.path(dir).join(FILE);
            fs::create_dir_all(path.parent().expect("has a parent")).expect("dirs");
            fs::write(path, format!("---\n{front}\n---\nbody\n")).expect("writes");
            self
        }

        fn paths(&self) -> Paths {
            Paths {
                home: Some(self.path("home")),
                config_home: Some(self.path("home/.config")),
                skill_paths: Vec::new(),
            }
        }

        /// Name, description and the skill's folder relative to the tree.
        fn discover(
            &self,
            cwd: &str,
            paths: &Paths,
        ) -> (Vec<(String, String, String)>, Vec<String>) {
            let mut warnings = Vec::new();
            let skills = discover(&self.path(cwd), paths, &mut warnings);
            let found = skills
                .iter()
                .filter(|s| s.source != Source::Internal)
                .map(|s| {
                    let dir = s.dir().expect("read from a file");
                    let dir = dir.strip_prefix(self.0.path()).expect("inside");
                    (
                        s.name.clone(),
                        s.description.clone().unwrap_or_default(),
                        dir.display().to_string(),
                    )
                })
                .collect();
            (found, warnings)
        }
    }

    fn found(list: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        list.iter()
            .map(|(n, d, p)| (n.to_string(), d.to_string(), p.to_string()))
            .collect()
    }

    #[test]
    fn finds_skills_in_every_standard_place() {
        let tree = Tree::new();
        tree.skill("home/.claude/skills/a", "name: a\ndescription: claude")
            .skill(
                "home/.config/opencode/skills/b",
                "name: b\ndescription: opencode",
            )
            .skill("home/.agents/skills/c", "name: c\ndescription: agents")
            .skill("home/.config/nth/skills/d", "name: d\ndescription: nth")
            .skill(
                "repo/.claude/skills/e",
                "name: e\ndescription: project claude",
            )
            .skill(
                "repo/.agents/skills/group/f",
                "name: f\ndescription: nested",
            )
            .skill("repo/.nth/skills/g", "name: g\ndescription: project nth");
        fs::create_dir_all(tree.path("repo/.git")).expect("git dir");

        let (skills, warnings) = tree.discover("repo", &tree.paths());

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            skills,
            found(&[
                ("a", "claude", "home/.claude/skills/a"),
                ("b", "opencode", "home/.config/opencode/skills/b"),
                ("c", "agents", "home/.agents/skills/c"),
                ("d", "nth", "home/.config/nth/skills/d"),
                ("e", "project claude", "repo/.claude/skills/e"),
                ("f", "nested", "repo/.agents/skills/group/f"),
                ("g", "project nth", "repo/.nth/skills/g"),
            ])
        );
    }

    #[test]
    fn hidden_folders_are_not_searched() {
        let tree = Tree::new();
        tree.skill(
            "home/.claude/skills/.trash/dead",
            "name: dead\ndescription: gone",
        )
        .skill("home/.claude/skills/live", "name: live\ndescription: here");

        assert_eq!(
            tree.discover("home", &tree.paths()).0,
            found(&[("live", "here", "home/.claude/skills/live")])
        );
    }

    #[test]
    fn a_skill_knows_the_root_it_was_found_under() {
        let tree = Tree::new();
        tree.skill("home/.claude/skills/a", "name: a")
            .skill("repo/.agents/skills/group/b", "name: b");

        let skills = discover(&tree.path("repo"), &tree.paths(), &mut Vec::new());

        assert_eq!(
            skills.get("a").expect("a").root.as_deref(),
            Some(tree.path("home/.claude/skills").as_path())
        );
        assert_eq!(
            skills.get("b").expect("b").root.as_deref(),
            Some(tree.path("repo/.agents/skills").as_path())
        );
        assert_eq!(skills.get("work-plan").expect("internal").root, None);
    }

    #[test]
    fn project_beats_global_and_config_beats_both() {
        let tree = Tree::new();
        tree.skill(
            "home/.claude/skills/review",
            "name: review\ndescription: global",
        )
        .skill(
            "repo/.claude/skills/review",
            "name: review\ndescription: root",
        )
        .skill(
            "repo/sub/.agents/skills/review",
            "name: review\ndescription: nearer",
        )
        .skill("repo/.claude/skills/lint", "name: lint\ndescription: root")
        .skill("shared/lint", "name: lint\ndescription: config");
        fs::create_dir_all(tree.path("repo/.git")).expect("git dir");
        let mut paths = tree.paths();

        assert_eq!(
            tree.discover("repo/sub", &paths).0,
            found(&[
                ("lint", "root", "repo/.claude/skills/lint"),
                ("review", "nearer", "repo/sub/.agents/skills/review"),
            ])
        );

        paths.skill_paths = vec!["../../shared".into()];
        assert_eq!(
            tree.discover("repo/sub", &paths).0[0],
            (
                "lint".into(),
                "config".into(),
                "repo/sub/../../shared/lint".into()
            )
        );
    }

    #[test]
    fn the_folder_names_a_skill_without_a_name() {
        let tree = Tree::new();
        tree.skill("home/.agents/skills/deploy", "description: ship it");

        assert_eq!(
            tree.discover("home", &tree.paths()).0,
            found(&[("deploy", "ship it", "home/.agents/skills/deploy")])
        );
    }

    #[tokio::test]
    async fn renders_the_body_with_its_folder_and_files() {
        let tree = Tree::new();
        tree.skill(
            "home/.agents/skills/deploy",
            "name: deploy\ndescription: ship it",
        );
        let dir = tree.path("home/.agents/skills/deploy");
        fs::create_dir_all(dir.join("scripts")).expect("dirs");
        fs::write(dir.join("scripts/ship.sh"), "").expect("writes");
        let skills = discover(&tree.path("home"), &tree.paths(), &mut Vec::new());
        let skill = skills.get("deploy").expect("found");

        let content = skill.render().await.expect("renders");

        assert_eq!(
            content,
            format!(
                "<skill_content name=\"deploy\">\n# Skill: deploy\n\nbody\n\n\
                 Base directory for this skill: {dir}\n\
                 Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\n\n\
                 <skill_files>\n<file>{dir}/scripts/ship.sh</file>\n</skill_files>\n</skill_content>\n",
                dir = dir.display()
            )
        );
    }

    #[test]
    fn broken_skills_are_skipped_with_a_warning() {
        let tree = Tree::new();
        tree.skill("home/.agents/skills/ok", "name: ok")
            .skill("home/.agents/skills/spaced", "name: two words")
            .skill("home/.agents/skills/bad", "name: [unclosed");

        let (skills, warnings) = tree.discover("home", &tree.paths());

        assert_eq!(skills, found(&[("ok", "", "home/.agents/skills/ok")]));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.starts_with("skipped skill ")));
    }

    #[test]
    fn internal_skills_are_always_there() {
        let tree = Tree::new();
        let skills = discover(&tree.path("repo"), &tree.paths(), &mut Vec::new());

        for name in [
            "config",
            "init",
            "screenshot",
            "setup-worktrees",
            "work-plan",
        ] {
            let skill = skills.get(name).expect(name);
            assert_eq!(skill.source, Source::Internal);
            assert!(skill.description.is_some());
            assert_eq!(skill.path(), None);
        }
    }

    #[test]
    fn a_file_beats_an_internal_skill() {
        let tree = Tree::new();
        tree.skill(
            "home/.agents/skills/work-plan",
            "name: work-plan\ndescription: mine",
        );

        assert_eq!(
            tree.discover("home", &tree.paths()).0,
            found(&[("work-plan", "mine", "home/.agents/skills/work-plan")])
        );
    }

    #[tokio::test]
    async fn renders_an_internal_skill_without_a_folder() {
        let skills = discover(Path::new("/nowhere"), &Paths::default(), &mut Vec::new());
        let skill = skills.get("work-plan").expect("work-plan");

        let content = skill.render().await.expect("renders");

        assert!(
            content.starts_with("<skill_content name=\"work-plan\">\n# Skill: work-plan\n\n"),
            "{content}"
        );
        assert!(content.ends_with("\n</skill_content>\n"), "{content}");
        assert!(!content.contains("Base directory"), "{content}");
        assert!(
            !content.contains("name: work-plan"),
            "the frontmatter is left out"
        );
    }
}
