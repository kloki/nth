//! Skills: folders with a `SKILL.md`, whose frontmatter names and describes
//! the skill and whose body tells the model how to do the task. nth reads
//! them where Claude Code, opencode and the open standard keep them, so a
//! skill written for any of those works here too.

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
    /// The `SKILL.md` itself.
    pub path: PathBuf,
    pub source: Source,
}

impl Skill {
    /// The skill's folder, which relative paths in its body start from.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
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
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("cannot read {}: {e}", self.path.display()))?;
        let (_, body) = frontmatter::split(&text)?;
        Ok(body.to_string())
    }

    /// `body` wrapped with where the skill lives and the files beside it.
    fn render_body(&self, body: &str) -> String {
        let files: Vec<String> = files(self.dir())
            .iter()
            .map(|file| format!("<file>{}</file>", file.display()))
            .collect();
        let dir = self.dir().display().to_string();
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
/// wins: project over global, config over both, nearer over further.
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
    for (root, source) in roots(cwd, paths) {
        for file in find(&root) {
            match read(&file, source) {
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
/// is still a skill.
fn find(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .standard_filters(false)
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

fn read(file: &Path, source: Source) -> Result<Skill, String> {
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
        path: file.to_path_buf(),
        source,
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
                .map(|s| {
                    let dir = s.dir().strip_prefix(self.0.path()).expect("inside");
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
}
