//! Agents the model delegates to with the task tool: opencode's built-in
//! `general` and `explore`, and one markdown file per agent of your own,
//! read where Claude Code and opencode keep them, so an agent written for
//! either works here too. The frontmatter names and describes the agent
//! and may pick its model and tools; the body is its system prompt.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde_norway::Value;

use crate::{Paths, frontmatter, project_root};

/// opencode's explore prompt, `agent/prompt/explore.txt`.
const EXPLORE: &str = include_str!("explore.md");
/// opencode's descriptions, `agent/agent.ts`.
const GENERAL_DESCRIPTION: &str = "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.";
const EXPLORE_DESCRIPTION: &str = "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.";
/// What opencode's explore may do, in nth's tool names.
const EXPLORE_TOOLS: [&str; 6] = ["read", "glob", "grep", "bash", "webfetch", "websearch"];

/// Deep enough for `agents/<group>/<agent>.md`. opencode goes any depth
/// and names a nested agent by its path, `group/agent`; nth names it by its
/// file alone, since a name is typed after `@`.
const MAX_DEPTH: usize = 2;

/// Agent folders inside a project directory, in rising precedence.
const PROJECT_DIRS: [&str; 4] = [
    ".claude/agents",
    ".opencode/agent",
    ".opencode/agents",
    ".nth/agents",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Built into nth.
    Builtin,
    /// The user's own folders, for every project.
    Global,
    /// Checked into the project, from its root down to the working
    /// directory.
    Project,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Builtin => "builtin",
            Source::Global => "global",
            Source::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    /// What the model reads to decide whether to delegate to the agent. An
    /// agent without one can be named by the user but is not offered to the
    /// model.
    pub description: Option<String>,
    /// Its system prompt, in place of the model's persona; `None` keeps the
    /// persona, as opencode's `general` does.
    pub prompt: Option<String>,
    /// The tools it may use, by name; `None` means every tool a subagent
    /// may have.
    pub tools: Option<Vec<String>>,
    /// The tools it may not use, whatever `tools` says: opencode's
    /// `{write: false}` and `permission: {bash: deny}`.
    pub denied: Vec<String>,
    /// The model it runs on; `None` means its parent's.
    pub model: Option<String>,
    /// opencode's `hidden`: the model may delegate to it, but `@` does not
    /// offer it.
    pub hidden: bool,
    /// The file it was read from; built-ins have none.
    pub path: Option<PathBuf>,
    pub source: Source,
}

impl Agent {
    /// Whether the agent may use the tool called `name`.
    pub fn allows(&self, name: &str) -> bool {
        !self.denied.iter().any(|t| t == name)
            && self
                .tools
                .as_ref()
                .is_none_or(|tools| tools.iter().any(|t| t == name))
    }
}

/// Every agent found, by name. When two share a name, the one found later
/// wins: a file over a built-in, project over global, nearer over further.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Agents(BTreeMap<String, Agent>);

impl Agents {
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.0.get(name)
    }

    /// In name order.
    pub fn iter(&self) -> impl Iterator<Item = &Agent> {
        self.0.values()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn insert(&mut self, agent: Agent) {
        self.0.insert(agent.name.clone(), agent);
    }

    fn remove(&mut self, name: &str) {
        self.0.remove(name);
    }
}

pub(crate) fn discover(cwd: &Path, paths: &Paths, warnings: &mut Vec<String>) -> Agents {
    let mut agents = Agents::default();
    for agent in builtin() {
        agents.insert(agent);
    }
    for (root, source) in roots(cwd, paths) {
        for file in find(&root) {
            match read(&file, source) {
                Ok(Read::Agent(agent)) => agents.insert(agent),
                // opencode's `disable` takes an agent away, a built-in too.
                Ok(Read::Disabled(name)) => agents.remove(&name),
                Ok(Read::Primary) => {}
                Err(e) => warnings.push(format!("skipped agent {}: {e}", file.display())),
            }
        }
    }
    agents
}

fn builtin() -> [Agent; 2] {
    [
        Agent {
            name: "general".into(),
            description: Some(GENERAL_DESCRIPTION.into()),
            prompt: None,
            tools: None,
            denied: Vec::new(),
            model: None,
            hidden: false,
            path: None,
            source: Source::Builtin,
        },
        Agent {
            name: "explore".into(),
            description: Some(EXPLORE_DESCRIPTION.into()),
            prompt: Some(EXPLORE.trim().into()),
            tools: Some(EXPLORE_TOOLS.iter().map(|t| t.to_string()).collect()),
            denied: Vec::new(),
            model: None,
            hidden: false,
            path: None,
            source: Source::Builtin,
        },
    ]
}

/// The folders to search, lowest precedence first.
fn roots(cwd: &Path, paths: &Paths) -> Vec<(PathBuf, Source)> {
    let mut roots = Vec::new();
    if let Some(home) = &paths.home {
        roots.push(home.join(".claude/agents"));
    }
    if let Some(config_home) = &paths.config_home {
        roots.push(config_home.join("opencode/agent"));
        roots.push(config_home.join("opencode/agents"));
    }
    if let Some(config_dir) = paths.config_dir() {
        roots.push(config_dir.join("agents"));
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
    // Run from the home directory, its agent folders are global and project
    // ones at once; they count once, as global.
    let mut seen = std::collections::BTreeSet::new();
    roots.retain(|(root, _)| seen.insert(root.clone()));
    roots
}

/// Every `.md` file under `root`, in path order so discovery is the same on
/// every run. Ignore files are not applied: an agent folder that git
/// ignores still holds agents.
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
        .filter(|entry| {
            entry.path().extension().is_some_and(|e| e == "md") && entry.path().is_file()
        })
        .map(ignore::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

/// What an agent file says.
enum Read {
    Agent(Agent),
    /// opencode's `disable: true`, for the agent of this name.
    Disabled(String),
    /// opencode's primary agents replace build and plan, which nth calls
    /// modes; they are not subagents.
    Primary,
}

fn read(file: &Path, source: Source) -> Result<Read, String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let (front, body) = frontmatter::parse(&text)?;
    if frontmatter::field(&front, "mode").as_deref() == Some("primary") {
        return Ok(Read::Primary);
    }
    let name = match frontmatter::field(&front, "name") {
        Some(name) => name,
        None => file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .ok_or("no name in its frontmatter")?,
    };
    // A name is typed after `@`, so it has to be one word.
    if name.contains(|c: char| c.is_whitespace() || c == '/') {
        return Err(format!("name {name:?} has spaces or slashes"));
    }
    if front.get("disable").and_then(Value::as_bool) == Some(true) {
        return Ok(Read::Disabled(name));
    }
    let prompt = Some(body.trim())
        .filter(|b| !b.is_empty())
        .map(String::from);
    let (tools, mut denied) = tools(front.get("tools"));
    denied.extend(permission_denied(front.get("permission")));
    denied.sort();
    denied.dedup();
    Ok(Read::Agent(Agent {
        name,
        description: frontmatter::field(&front, "description"),
        prompt,
        tools,
        denied,
        // Claude Code's way of saying its parent's model.
        model: frontmatter::field(&front, "model").filter(|m| m != "inherit"),
        hidden: front.get("hidden").and_then(Value::as_bool) == Some(true),
        path: Some(file.to_path_buf()),
        source,
    }))
}

/// The `tools` field in either shape, as the tools allowed and those
/// denied. Claude Code's `Read, Grep` list is all an agent may use;
/// opencode's `{read: true, write: false}` map changes what every tool
/// allows, so only its `false` entries count. Names are lowercased, since
/// nth's tools are.
fn tools(value: Option<&Value>) -> (Option<Vec<String>>, Vec<String>) {
    let names: Vec<String> = match value {
        Some(Value::String(list)) => list
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
        Some(Value::Sequence(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(|t| t.trim().to_lowercase())
            .collect(),
        Some(Value::Mapping(map)) => {
            let denied = map
                .iter()
                .filter(|(_, allowed)| allowed.as_bool() == Some(false))
                .filter_map(|(name, _)| name.as_str())
                .flat_map(opencode_tool)
                .collect();
            return (None, denied);
        }
        _ => return (None, Vec::new()),
    };
    (Some(names), Vec::new())
}

/// The tools opencode's `permission` map denies outright. A pattern map,
/// such as `bash: {"git *": allow, "*": deny}`, is finer than nth's tools
/// can be told, so only a plain `deny` counts.
fn permission_denied(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Mapping(map)) = value else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, action)| action.as_str() == Some("deny"))
        .filter_map(|(name, _)| name.as_str())
        .flat_map(opencode_tool)
        .collect()
}

/// nth's tools that opencode's tool or permission `name` stands for:
/// opencode's `edit` permission covers every tool that changes a file, and
/// its `write`, `edit` and `patch` switches all set it.
fn opencode_tool(name: &str) -> Vec<String> {
    match name.to_lowercase().as_str() {
        "write" | "edit" | "patch" | "apply_patch" => {
            ["write", "edit", "apply_patch"].map(String::from).to_vec()
        }
        other => vec![other.to_string()],
    }
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

        fn agent(&self, file: &str, front: &str, body: &str) -> &Self {
            let path = self.path(file);
            fs::create_dir_all(path.parent().expect("has a parent")).expect("dirs");
            fs::write(path, format!("---\n{front}\n---\n{body}\n")).expect("writes");
            self
        }

        fn paths(&self) -> Paths {
            Paths {
                home: Some(self.path("home")),
                config_home: Some(self.path("home/.config")),
                skill_paths: Vec::new(),
            }
        }

        /// Name, description and file relative to the tree, built-ins
        /// left out.
        fn discover(&self, cwd: &str) -> (Vec<(String, String, String)>, Vec<String>) {
            let mut warnings = Vec::new();
            let agents = discover(&self.path(cwd), &self.paths(), &mut warnings);
            let found = agents
                .iter()
                .filter(|a| a.source != Source::Builtin)
                .map(|a| {
                    let path = a.path.as_ref().expect("read from a file");
                    let path = path.strip_prefix(self.0.path()).expect("inside");
                    (
                        a.name.clone(),
                        a.description.clone().unwrap_or_default(),
                        path.display().to_string(),
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
    fn the_builtins_are_always_there() {
        let tree = Tree::new();
        let agents = discover(&tree.path("repo"), &tree.paths(), &mut Vec::new());

        let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["explore", "general"]);
        let explore = agents.get("explore").expect("explore");
        assert!(
            explore
                .prompt
                .as_deref()
                .is_some_and(|p| p.starts_with("You are a file search specialist"))
        );
        assert!(explore.allows("grep"));
        assert!(!explore.allows("write"));
        let general = agents.get("general").expect("general");
        assert_eq!(general.prompt, None);
        assert!(general.allows("write"));
        assert_eq!(general.source, Source::Builtin);
    }

    #[test]
    fn finds_agents_in_every_standard_place() {
        let tree = Tree::new();
        tree.agent("home/.claude/agents/a.md", "description: claude", "")
            .agent(
                "home/.config/opencode/agent/b.md",
                "description: opencode",
                "",
            )
            .agent("home/.config/nth/agents/c.md", "description: nth", "")
            .agent(
                "repo/.claude/agents/d.md",
                "description: project claude",
                "",
            )
            .agent(
                "repo/.opencode/agents/group/e.md",
                "description: nested",
                "",
            )
            .agent("repo/.nth/agents/f.md", "description: project nth", "");
        fs::create_dir_all(tree.path("repo/.git")).expect("git dir");

        let (agents, warnings) = tree.discover("repo");

        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            agents,
            found(&[
                ("a", "claude", "home/.claude/agents/a.md"),
                ("b", "opencode", "home/.config/opencode/agent/b.md"),
                ("c", "nth", "home/.config/nth/agents/c.md"),
                ("d", "project claude", "repo/.claude/agents/d.md"),
                ("e", "nested", "repo/.opencode/agents/group/e.md"),
                ("f", "project nth", "repo/.nth/agents/f.md"),
            ])
        );
    }

    #[test]
    fn a_file_beats_a_builtin_and_nearer_beats_further() {
        let tree = Tree::new();
        tree.agent(
            "home/.claude/agents/explore.md",
            "description: mine",
            "Look harder.",
        )
        .agent("repo/.claude/agents/review.md", "description: root", "")
        .agent("repo/sub/.nth/agents/review.md", "description: nearer", "");
        fs::create_dir_all(tree.path("repo/.git")).expect("git dir");

        let agents = discover(&tree.path("repo/sub"), &tree.paths(), &mut Vec::new());

        let explore = agents.get("explore").expect("explore");
        assert_eq!(explore.description.as_deref(), Some("mine"));
        assert_eq!(explore.prompt.as_deref(), Some("Look harder."));
        assert_eq!(explore.tools, None, "the file says nothing about tools");
        assert_eq!(explore.source, Source::Global);
        assert_eq!(
            agents.get("review").expect("review").description.as_deref(),
            Some("nearer")
        );
    }

    #[test]
    fn reads_both_shapes_of_tools_and_the_model() {
        let tree = Tree::new();
        tree.agent(
            "repo/.claude/agents/reviewer.md",
            "name: reviewer\ndescription: Reviews\ntools: Read, Grep, Glob\nmodel: sonnet",
            "You review.",
        )
        .agent(
            "repo/.opencode/agents/triage.md",
            "description: Triages\nmode: subagent\ntools:\n  read: true\n  write: false\n  grep: true",
            "You triage.",
        );

        let agents = discover(&tree.path("repo"), &tree.paths(), &mut Vec::new());

        let reviewer = agents.get("reviewer").expect("reviewer");
        assert_eq!(
            reviewer.tools.as_deref(),
            Some(&["read".to_string(), "grep".into(), "glob".into()][..])
        );
        assert_eq!(reviewer.model.as_deref(), Some("sonnet"));
        assert_eq!(reviewer.prompt.as_deref(), Some("You review."));
        let triage = agents.get("triage").expect("triage");
        assert_eq!(triage.tools, None, "a map only changes the defaults");
        assert!(triage.allows("read") && triage.allows("bash"));
        for writer in ["write", "edit", "apply_patch"] {
            assert!(!triage.allows(writer), "{writer}");
        }
    }

    #[test]
    fn opencode_permissions_deny_and_inherit_is_the_parents_model() {
        let tree = Tree::new();
        tree.agent(
            "repo/.opencode/agents/careful.md",
            "description: c\npermission:\n  edit: deny\n  bash:\n    \"*\": deny\n  webfetch: deny",
            "",
        )
        .agent(
            "repo/.claude/agents/plain.md",
            "description: p\nmodel: inherit",
            "",
        );

        let agents = discover(&tree.path("repo"), &tree.paths(), &mut Vec::new());

        let careful = agents.get("careful").expect("careful");
        assert_eq!(careful.denied, ["apply_patch", "edit", "webfetch", "write"]);
        assert!(careful.allows("bash"), "a pattern map is not a plain deny");
        assert_eq!(agents.get("plain").expect("plain").model, None);
    }

    #[test]
    fn hidden_agents_are_kept_and_disabled_ones_go() {
        let tree = Tree::new();
        tree.agent(
            "repo/.opencode/agent/secret.md",
            "description: s\nhidden: true",
            "",
        )
        .agent("repo/.opencode/agent/explore.md", "disable: true", "")
        .agent("repo/.claude/agents/gone.md", "description: g", "")
        .agent("repo/.nth/agents/gone.md", "disable: true", "");
        fs::create_dir_all(tree.path("repo/.git")).expect("git dir");

        let agents = discover(&tree.path("repo"), &tree.paths(), &mut Vec::new());

        let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["general", "secret"]);
        assert!(agents.get("secret").expect("secret").hidden);
    }

    #[test]
    fn primary_agents_are_not_subagents_and_bad_names_warn() {
        let tree = Tree::new();
        tree.agent(
            "repo/.opencode/agent/build.md",
            "description: b\nmode: primary",
            "",
        )
        .agent(
            "repo/.claude/agents/bad.md",
            "name: two words\ndescription: x",
            "",
        );

        let (agents, warnings) = tree.discover("repo");

        assert_eq!(agents, Vec::new());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("has spaces or slashes"),
            "{warnings:?}"
        );
    }
}
