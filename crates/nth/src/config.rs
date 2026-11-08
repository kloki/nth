use std::{
    collections::BTreeMap,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use nth_context::Paths;
use nth_icons::icons;
use nth_protocol::{Effort, Mode};
use serde::{Deserialize, Serialize};

/// Everything in `config.toml`. Every key is optional: a missing key keeps
/// the default below, so a config file only lists what it changes.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The model a new chat starts on, as `provider/model`. Before the
    /// tables, since TOML has values ahead of tables.
    pub model: String,
    /// The endpoints by id; the id prefixes their models. None is built
    /// in: these are only the ones the file lists.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub provider: BTreeMap<String, ProviderConfig>,
    pub session: SessionConfig,
    /// How a session keeps itself inside the model's context window: whether
    /// it compacts on its own, and the budgets that uses. The keys are
    /// `auto` (compact when the last reply fills the window), `reserved`
    /// (the buffer kept below it; 0 sizes it to the model) and `preserve`
    /// (tokens kept verbatim; 0 sizes it to the window).
    #[serde(default)]
    pub context: nth_protocol::Compaction,
    pub task: TaskConfig,
    pub work: WorkConfig,
    pub memory: MemoryConfig,
    pub mode: ModeConfig,
    /// Named presets of a plan, an act and a subagent LLM, cycled with
    /// ctrl+p. The
    /// first is where a new chat starts; empty means no presets, and
    /// both modes run the model above.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileConfig>,
    pub chat: ChatConfig,
    pub layout: LayoutConfig,
    pub tools: nth_tools::ToolsConfig,
    pub skills: SkillsConfig,
    pub directories: DirectoriesConfig,
    pub format: nth_format::FormatConfig,
    pub lsp: nth_lsp::LspConfig,
    pub notify: nth_notify::NotifyConfig,
}

/// A provider endpoint; the models.dev catalogue selects each model's
/// protocol, falling back to OpenAI-compatible chat completions.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// What listings call it; the id when empty.
    pub name: String,
    pub base_url: String,
    /// The environment variable holding the API key, so the key itself
    /// never has to live in the config file.
    pub api_key_env: String,
    /// The models to offer instead of asking the endpoint's `/models`: for
    /// an endpoint without one, or to offer only these.
    pub models: Vec<String>,
}

/// What `nth init` writes: every key at its default, with a comment on
/// each. A test keeps it equal to `Config::default()`.
const TEMPLATE: &str = include_str!("config.toml");

impl ProviderConfig {
    /// Names it by `id` when its block has no name, and rejects a block
    /// without an endpoint or a key variable, or one taking a built-in id.
    fn complete(&mut self, id: &str) -> Result<()> {
        if id == nth_auth::ChatGpt::ID {
            bail!("provider {id} is built in: run `nth connect` instead of configuring it");
        }
        if self.name.is_empty() {
            self.name = id.to_string();
        }
        if self.base_url.is_empty() {
            bail!("provider {id} has no base_url");
        }
        if self.api_key_env.is_empty() {
            bail!("provider {id} has no api_key_env");
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub max_steps: usize,
    /// `0` lets a tool call run as long as it likes.
    pub tool_timeout_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_steps: nth_session::DEFAULT_MAX_STEPS,
            tool_timeout_secs: nth_session::DEFAULT_TOOL_TIMEOUT.as_secs(),
        }
    }
}

impl SessionConfig {
    pub fn tool_timeout(&self) -> Option<Duration> {
        (self.tool_timeout_secs > 0).then(|| Duration::from_secs(self.tool_timeout_secs))
    }
}

/// What a subagent the model starts with the task tool may spend. Lower
/// than the session's, since a child works on one delegated question and
/// its parent is waiting on the answer.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    pub max_steps: usize,
    /// `0` leaves a task to its steps alone.
    pub timeout_secs: u64,
    /// `0` never reports a stall.
    pub stall_secs: u64,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_steps: 50,
            timeout_secs: 600,
            stall_secs: 120,
        }
    }
}

impl TaskConfig {
    pub fn limits(&self) -> nth_session::subagent::Limits {
        nth_session::subagent::Limits {
            max_steps: self.max_steps,
            timeout: secs(self.timeout_secs),
            stall: secs(self.stall_secs),
        }
    }
}

/// What a subagent the model puts on a work item may spend, and how long
/// it may say nothing before the model hears it stalled. More steps than a
/// task's: an item is a whole piece of work, not one question.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkConfig {
    pub max_steps: usize,
    /// `0` leaves an item to its steps alone.
    pub timeout_secs: u64,
    /// `0` never reports a stall.
    pub stall_secs: u64,
}

impl Default for WorkConfig {
    fn default() -> Self {
        Self {
            max_steps: 100,
            timeout_secs: 600,
            stall_secs: 120,
        }
    }
}

impl WorkConfig {
    pub fn limits(&self) -> nth_session::subagent::Limits {
        nth_session::subagent::Limits {
            max_steps: self.max_steps,
            timeout: secs(self.timeout_secs),
            stall: secs(self.stall_secs),
        }
    }
}

/// A limit in seconds, where `0` means none.
fn secs(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Which mode a new chat starts in. What each mode runs with comes from
/// a profile.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModeConfig {
    /// Before the tables, since TOML has values ahead of tables.
    pub default: Mode,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            default: Mode::Plan,
        }
    }
}

/// A named preset: the model and effort plan, act and subagents run
/// with. The first
/// is where a new chat starts; ctrl+p in the TUI cycles through them.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    /// What the label and ctrl+p call it. Required.
    pub name: String,
    pub plan: ModeDefaults,
    pub act: ModeDefaults,
    /// What subagents run on. Unlike the modes', an empty model means the
    /// parent's model and effort, not the config's.
    pub subagent: ModeDefaults,
    /// What the built-in clerk runs on. An empty model means the subagent
    /// slot's.
    pub clerk: ModeDefaults,
}

/// The model and effort one mode runs with in a profile.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModeDefaults {
    /// Empty means the config's model.
    pub model: String,
    pub effort: Effort,
}

impl ProfileConfig {
    /// Rejects a profile without a name: the label and the cycling hint
    /// need one.
    fn complete(&self, index: usize) -> Result<()> {
        if self.name.is_empty() {
            bail!("profile {} has no name", index + 1);
        }
        Ok(())
    }
}

/// The project's memory: what the model saves in `.nth/memory/` about the
/// project and you, and reads back in later sessions.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// The index goes into the system prompt and the model is told how to
    /// save; off, it is neither.
    pub enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// How the chat shows what the model did. `/settings` still flips these for
/// the run; this only sets where they start.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChatConfig {
    /// The model's reasoning under its `∴` line.
    pub thinking: bool,
    /// What each tool call returned, under its row.
    pub tool_output: bool,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            thinking: true,
            tool_output: true,
        }
    }
}

/// How the TUI lays out its content panel.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutConfig {
    /// From this many columns the chat shows on the left and the other
    /// tabs on the right; 0 never splits.
    pub split_width: u16,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self { split_width: 160 }
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Extra folders searched for `SKILL.md` files, after the standard ones.
    /// `~/` is the home directory; relative paths are from the working
    /// directory.
    pub paths: Vec<String>,
}

/// Additional directories available to a session.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DirectoriesConfig {
    pub paths: Vec<String>,
}

impl Config {
    /// `$XDG_CONFIG_HOME/nth/config.toml`, else `~/.config/nth/config.toml`.
    /// The design puts nth's files under `~/.config/nth` on every platform,
    /// so this does not use the macOS `Library` folder.
    pub fn default_path() -> Option<PathBuf> {
        Some(Paths::from_env().config_dir()?.join("config.toml"))
    }

    /// The model from `--model` or `NTH_MODEL`. It is for every mode and
    /// profile, so their own models go: those only fill in when nothing
    /// was given.
    pub fn set_model(&mut self, model: String) {
        self.model = model;
        for profile in &mut self.profiles {
            profile.plan.model.clear();
            profile.act.model.clear();
            profile.subagent.model.clear();
            profile.clerk.model.clear();
        }
    }

    /// The model and effort `mode` starts on: the first profile's, or
    /// the model above with the default effort when there are no
    /// profiles.
    pub fn llm_for(&self, mode: Mode) -> (String, Effort) {
        match self.profiles.first() {
            Some(profile) => self.profile_llm(profile, mode),
            None => (self.model.clone(), Effort::default()),
        }
    }

    /// The model and effort `mode` runs with in `profile`; an empty model
    /// falls back to the model above.
    pub fn profile_llm(&self, profile: &ProfileConfig, mode: Mode) -> (String, Effort) {
        let defaults = match mode {
            Mode::Plan => &profile.plan,
            Mode::Act => &profile.act,
        };
        let model = match defaults.model.as_str() {
            "" => self.model.clone(),
            model => model.to_string(),
        };
        (model, defaults.effort)
    }

    /// The model and effort subagents start on: the first profile's
    /// subagent slot. `None` when it names no model, or there are no
    /// profiles: subagents then run on their parent's.
    pub fn subagent_for(&self) -> Option<(String, Effort)> {
        self.profile_subagent(self.profiles.first()?)
    }

    /// The model and effort subagents run with in `profile`; `None` when
    /// its subagent slot names no model, so they run on their parent's.
    pub fn profile_subagent(&self, profile: &ProfileConfig) -> Option<(String, Effort)> {
        slot(&profile.subagent)
    }

    /// The model and effort the clerk starts on: the first profile's clerk
    /// slot. `None` when it names no model, or there are no profiles: the
    /// clerk then runs as the other subagents.
    pub fn clerk_for(&self) -> Option<(String, Effort)> {
        self.profile_clerk(self.profiles.first()?)
    }

    /// The model and effort the clerk runs with in `profile`; `None` when
    /// its clerk slot names no model, so it runs as the other subagents.
    pub fn profile_clerk(&self, profile: &ProfileConfig) -> Option<(String, Effort)> {
        slot(&profile.clerk)
    }

    /// Whether a random model has to fill the one above in: it is empty
    /// and something that may run has no model of its own — a profile
    /// slot, or, with no profiles, both modes' only model.
    pub fn needs_model(&self) -> bool {
        self.model.is_empty()
            && (self.profiles.is_empty()
                || self
                    .profiles
                    .iter()
                    .any(|p| p.plan.model.is_empty() || p.act.model.is_empty()))
    }

    /// Where nth looks for files about a project, with the configured skill
    /// folders added and the memory read when it is on.
    pub fn paths(&self) -> Paths {
        self.paths_over(Paths::from_env())
    }

    /// As `paths`, over the global directories in `paths` rather than the
    /// environment's.
    pub fn paths_over(&self, mut paths: Paths) -> Paths {
        paths.skill_paths = self
            .skills
            .paths
            .iter()
            .map(|path| skill_path(path, paths.home.as_deref()))
            .collect();
        paths.memory = self.memory.enabled;
        paths
    }

    /// Resolve the global config and project overlay for a session's cwd:
    /// `path`, else the config in `paths`' config directory, and the
    /// repository's, whose walk stops at `paths`' home.
    pub fn load_in(path: Option<&Path>, cwd: &Path, paths: &Paths) -> Result<Self> {
        Self::load_layers(path, cwd, paths).map(|(config, _)| config)
    }

    /// As `load_in`, also returning the files actually read, global first.
    fn load_layers(path: Option<&Path>, cwd: &Path, paths: &Paths) -> Result<(Self, Vec<PathBuf>)> {
        let home = paths.home.as_deref();
        let cwd = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            std::env::current_dir()?.join(cwd)
        };
        let global = path
            .map(Path::to_path_buf)
            .or_else(|| paths.config_dir().map(|dir| dir.join("config.toml")));
        let mut sources = Vec::new();
        let mut value = toml::Value::Table(Default::default());
        if let Some(global) = global.as_deref().filter(|p| path.is_some() || p.exists()) {
            let mut layer = read_layer(global, false)?;
            normalize_paths(&mut layer, &cwd, home, false)?;
            merge(&mut value, layer, "");
            sources.push(global.to_path_buf());
        }
        if let Some((root, file)) = repo_config(&cwd, home) {
            let invalid = || format!("invalid config {}", file.display());
            // Where a refused path belongs, named even when that file does
            // not exist yet.
            let global = global.as_ref().map_or_else(
                || "the global config".to_string(),
                |p| p.display().to_string(),
            );
            let mut layer = read_layer(&file, true)?;
            normalize_paths(&mut layer, &root, home, true).with_context(invalid)?;
            paths_inside(&layer, &root, &global).with_context(invalid)?;
            merge(&mut value, layer, "");
            sources.push(file);
        }
        let text = toml::to_string(&value)?;
        let config = Self::parse(&text).with_context(|| {
            format!(
                "invalid merged config from {}",
                sources
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        Ok((config, sources))
    }

    fn parse(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        for (id, provider) in &mut config.provider {
            provider.complete(id)?;
        }
        for (index, profile) in config.profiles.iter().enumerate() {
            profile.complete(index)?;
        }
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string(self)?)
    }
}

/// The one project config for `cwd` and the directory its paths are
/// relative to: the main checkout's in a Git repository, else the nearest
/// among the directories instructions are read from (up to home when under
/// it, else `cwd` alone).
fn repo_config(cwd: &Path, home: Option<&Path>) -> Option<(PathBuf, PathBuf)> {
    if let Some(root) = nth_context::main_project_root(cwd) {
        let file = root.join(".nth/config");
        return file.exists().then_some((root, file));
    }
    nth_context::search_dirs(cwd, home)
        .into_iter()
        .find_map(|root| {
            let file = root.join(".nth/config");
            file.exists().then(|| (root.to_path_buf(), file))
        })
}

fn read_layer(path: &Path, repo: bool) -> Result<toml::Value> {
    let result = || -> Result<toml::Value> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow::anyhow!("config file {} not found", path.display()),
            _ => anyhow::Error::new(e).context(format!("cannot read {}", path.display())),
        })?;
        let value: toml::Value = toml::from_str(&text)?;
        let table = value.as_table().expect("TOML document is a table");
        if repo {
            for key in table.keys() {
                if ![
                    "model",
                    "profiles",
                    "mode",
                    "session",
                    "task",
                    "work",
                    "context",
                    "memory",
                    "skills",
                    "directories",
                ]
                .contains(&key.as_str())
                {
                    bail!("project config key `{key}` is not allowed");
                }
            }
        }
        let mut parsed: Config = value.clone().try_into()?;
        for (id, provider) in &mut parsed.provider {
            provider.complete(id)?;
        }
        for (index, profile) in parsed.profiles.iter().enumerate() {
            profile.complete(index)?;
        }
        Ok(value)
    };
    result().with_context(|| format!("invalid config {}", path.display()))
}

/// Resolves every `skills.paths` and `directories.paths` entry against
/// `base`, dropping duplicates. A path that cannot be resolved (a symlink
/// loop) refuses a project layer, whose paths must be checked, and stays as
/// written in the global one, where a broken entry only warns later.
fn normalize_paths(
    value: &mut toml::Value,
    base: &Path,
    home: Option<&Path>,
    project: bool,
) -> Result<()> {
    for section in ["skills", "directories"] {
        let Some(paths) = value.get_mut(section).and_then(|v| v.get_mut("paths")) else {
            continue;
        };
        let paths = paths.as_array_mut().expect("validated paths");
        let mut seen = std::collections::HashSet::new();
        let mut kept = Vec::with_capacity(paths.len());
        for value in paths.iter() {
            let written = value.as_str().expect("validated path");
            let path = skill_path(written, home);
            let path = if path.is_absolute() {
                path
            } else {
                base.join(path)
            };
            let path = match resolve_path(&path) {
                Ok(path) => path,
                Err(error) if project => {
                    return Err(error.context(format!("{section}.paths entry {written}")));
                }
                Err(_) => path,
            };
            if seen.insert(path.clone()) {
                kept.push(toml::Value::String(path.to_string_lossy().into_owned()));
            }
        }
        *paths = kept;
    }
    Ok(())
}

/// Refuses a project layer's normalized `skills.paths` or `directories.paths`
/// entry outside `root`: a cloned repository must not hand the session a
/// directory such as `~/.ssh` or load prompt text from elsewhere on disk.
/// `normalize_paths` resolved every existing component through `resolve_path`,
/// so a prefix check sees where a symlink really points.
///
/// The check holds only at load: a path missing then can become an outward
/// symlink before the session uses it, say after a branch switch. Outside Git a `~/.nth/config` is a project config whose root
/// is home, so it may name anything under home; that file is the user's own.
fn paths_inside(value: &toml::Value, root: &Path, global: &str) -> Result<()> {
    let root = resolve_path(root)?;
    for section in ["skills", "directories"] {
        let Some(paths) = value
            .get(section)
            .and_then(|v| v.get("paths"))
            .and_then(toml::Value::as_array)
        else {
            continue;
        };
        for path in paths.iter().filter_map(toml::Value::as_str) {
            if !Path::new(path).starts_with(&root) {
                bail!("{section}.paths entry {path} is outside the repository; put it in {global}");
            }
        }
    }
    Ok(())
}

/// Symlinks `resolve_path` follows by hand before calling it a loop, as
/// Linux's own limit.
const SYMLINK_HOPS: usize = 40;

/// `path` made absolute and free of `.` and `..`, each existing prefix
/// canonicalized as it is reached, so a symlink is followed before a later
/// `..` and wherever it sits, even after a missing folder (`missing/../link`).
/// A symlink that does not canonicalize (dangling, or in a loop) is followed
/// through its target as text, so the result names where it points even
/// before that exists; past `SYMLINK_HOPS` it is a loop and an error. Other
/// components that do not exist are kept as written.
fn resolve_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    // Components still to walk, the next one last, so a symlink's target
    // can be pushed in front of what followed it.
    let mut pending: Vec<PathBuf> = path
        .components()
        .rev()
        .map(|part| PathBuf::from(part.as_os_str()))
        .collect();
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    while let Some(part) = pending.pop() {
        match part.components().next() {
            None | Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(_) => {
                resolved.push(&part);
                if let Ok(canonical) = std::fs::canonicalize(&resolved) {
                    resolved = canonical;
                    continue;
                }
                let dangling = std::fs::symlink_metadata(&resolved)
                    .is_ok_and(|meta| meta.file_type().is_symlink());
                if !dangling {
                    continue;
                }
                hops += 1;
                if hops > SYMLINK_HOPS {
                    bail!("{} is a symlink loop", path.display());
                }
                let target = std::fs::read_link(&resolved)
                    .with_context(|| format!("cannot read symlink {}", resolved.display()))?;
                // A relative target is from the link's folder; an absolute
                // one starts over at its root when pushed.
                resolved.pop();
                pending.extend(
                    target
                        .components()
                        .rev()
                        .map(|part| PathBuf::from(part.as_os_str())),
                );
            }
        }
    }
    Ok(resolved)
}

fn merge(base: &mut toml::Value, overlay: toml::Value, key: &str) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (name, value) in overlay {
                let path = if key.is_empty() {
                    name.clone()
                } else {
                    format!("{key}.{name}")
                };
                match base.get_mut(&name) {
                    Some(existing) => merge(existing, value, &path),
                    None => {
                        base.insert(name, value);
                    }
                }
            }
        }
        (toml::Value::Array(base), toml::Value::Array(overlay))
            if key == "skills.paths" || key == "directories.paths" =>
        {
            for item in overlay {
                if !base.contains(&item) {
                    base.push(item);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// `nth config`: the config in use with every default filled in, and on
/// stderr where it came from.
pub fn show(explicit: Option<PathBuf>, config: &Config) -> Result<()> {
    use owo_colors::OwoColorize;

    match explicit.or_else(Config::default_path) {
        Some(path) if path.exists() => {
            eprintln!("{} {}", icons().ok.green().bold(), path.display().dimmed())
        }
        Some(path) => eprintln!(
            "{} {}",
            icons().to.cyan().bold(),
            format!("no {}, using defaults", path.display()).dimmed()
        ),
        None => eprintln!(
            "{} {}",
            icons().to.cyan().bold(),
            "no home dir, using defaults".dimmed()
        ),
    }
    let cwd = std::env::current_dir()?;
    if let Some((_, path)) = repo_config(&cwd, Paths::from_env().home.as_deref()) {
        eprintln!("{} {}", icons().ok.green().bold(), path.display().dimmed());
    }
    print!("{}", config.to_toml()?);
    Ok(())
}

/// `nth init`: writes the template to the config path, unless a file is
/// there already and `force` is not set.
pub fn init(explicit: Option<PathBuf>, force: bool) -> Result<()> {
    use owo_colors::OwoColorize;

    let Some(path) = explicit.or_else(Config::default_path) else {
        bail!("no home dir to put the config in; pass --config");
    };
    write_template(&path, force)?;
    eprintln!(
        "{} wrote {}",
        icons().ok.green().bold(),
        path.display().dimmed()
    );
    eprintln!(
        "{} {}",
        icons().to.cyan().bold(),
        "uncomment a [provider.<id>] in it, export its key and set model".dimmed()
    );
    Ok(())
}

/// A profile's subagent or clerk slot; `None` when it names no model.
fn slot(defaults: &ModeDefaults) -> Option<(String, Effort)> {
    (!defaults.model.is_empty()).then(|| (defaults.model.clone(), defaults.effort))
}

fn write_template(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!("{} exists; --force overwrites it", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(path, TEMPLATE).with_context(|| format!("cannot write {}", path.display()))
}

/// A configured skill folder: `~/x` is `x` in the home directory.
/// Everything else, relative paths included, is left for the caller to
/// resolve.
fn skill_path(path: &str, home: Option<&Path>) -> PathBuf {
    nth_context::expand_home(path, home).unwrap_or_else(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_layers_merge_and_resolve_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let global = root.join("global.toml");
        std::fs::write(&global, "model = 'global'\n[skills]\npaths = ['../shared']\n[directories]\npaths = ['../docs']\n[format.mine]\ncommand = ['fmt']\nextensions = ['txt']\nenv = { A = 'a' }\n[lsp.mine]\ncommand = ['server']\nextensions = ['x']\n").unwrap();
        std::fs::write(root.join(".nth/config"), "model = ''\n[context]\nauto = false\nreserved = 0\n[skills]\npaths = ['shared', './shared']\n[directories]\npaths = ['docs', 'other']\n").unwrap();
        let (mut config, sources) =
            Config::load_layers(Some(&global), &root.join("sub"), &Paths::default()).unwrap();
        assert_eq!(sources, [global, root.join(".nth/config")]);
        assert_eq!(config.model, "");
        assert!(!config.context.auto);
        assert_eq!(config.context.reserved, 0);
        assert_eq!(config.skills.paths, [root.join("shared").to_string_lossy()]);
        assert_eq!(config.directories.paths.len(), 2);
        assert_eq!(config.format.formatters["mine"].env.len(), 1);
        assert_eq!(
            config.format.formatters["mine"]
                .extensions
                .as_ref()
                .unwrap(),
            &[".txt"]
        );
        assert_eq!(config.lsp.servers["mine"].command, ["server"]);
        config.set_model("cli".into());
        assert_eq!(config.model, "cli");
    }

    #[test]
    fn layers_reject_hidden_invalid_and_forbidden_values() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        let global = root.join("global");
        let repo = root.join(".nth/config");
        for text in [
            "[tools.websearch]\nurl = 'x'",
            "[provider.x]\nbase_url = 'x'",
            "[session]\nmax_step = 2",
            "[format.mine]\ncommand = 3",
            "[format.x]\ncommand = ['fmt']\nextensions = ['.x']",
            "[lsp.x]\ncommand = ['server']\nextensions = ['.x']",
            "[tools.bash]\nrtk = false",
            "[tools.read]\nmax_lines = 10",
        ] {
            std::fs::write(&global, "").unwrap();
            std::fs::write(&repo, text).unwrap();
            let err = Config::load_in(Some(&global), root, &Paths::default()).unwrap_err();
            assert!(
                format!("{err:#}").contains(&repo.display().to_string()),
                "{text}: {err:#}"
            );
        }
        std::fs::write(&global, "[session]\nmax_steps = 'bad'").unwrap();
        std::fs::write(&repo, "[session]\nmax_steps = 5").unwrap();
        let err = Config::load_in(Some(&global), root, &Paths::default()).unwrap_err();
        assert!(format!("{err:#}").contains(&global.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_config_cannot_add_directories_outside_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let sibling = tmp.path().join("sibling");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".nth/worktrees/w")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::os::unix::fs::symlink(&sibling, root.join("link")).unwrap();
        // Points outside at something that does not exist yet.
        std::os::unix::fs::symlink(sibling.join("later"), root.join("dangling")).unwrap();
        std::os::unix::fs::symlink("loop-b", root.join("loop-a")).unwrap();
        std::os::unix::fs::symlink("loop-a", root.join("loop-b")).unwrap();
        let global = tmp.path().join("global.toml");
        let repo = root.join(".nth/config");
        std::fs::write(&global, "").unwrap();
        let sibling = sibling.canonicalize().unwrap();
        for section in ["skills", "directories"] {
            for entry in [
                sibling.to_str().unwrap(),
                "../sibling",
                "link",
                "link/x",
                "missing/../link",
                "missing/../link/x",
                "dangling",
                "dangling/x",
                "missing/../dangling",
            ] {
                std::fs::write(&repo, format!("[{section}]\npaths = ['{entry}']")).unwrap();
                let err = Config::load_in(Some(&global), &root, &Paths::default()).unwrap_err();
                let message = format!("{err:#}");
                assert!(
                    message.starts_with(&format!("invalid config {}: ", repo.display())),
                    "{entry}: {message}"
                );
                assert!(
                    message.contains(&format!("{section}.paths entry {}", sibling.display())),
                    "{entry}: {message}"
                );
                assert!(
                    message.ends_with(&format!(
                        "is outside the repository; put it in {}",
                        global.display()
                    )),
                    "{entry}: {message}"
                );
            }
            // Paths under the root stay allowed, worktrees included.
            std::fs::write(
                &repo,
                format!("[{section}]\npaths = ['docs', '.nth/worktrees/w', 'sub/../x']"),
            )
            .unwrap();
            Config::load_in(Some(&global), &root, &Paths::default()).unwrap();
            // The global config may name any path.
            std::fs::write(&repo, "").unwrap();
            std::fs::write(
                &global,
                format!("[{section}]\npaths = ['{}']", sibling.display()),
            )
            .unwrap();
            Config::load_in(Some(&global), &root, &Paths::default()).unwrap();
            std::fs::write(&global, "").unwrap();
        }
        // A symlink loop is refused rather than kept as written, in a
        // project config only.
        for entry in ["loop-a", "loop-a/x", "missing/../loop-b"] {
            std::fs::write(&repo, format!("[directories]\npaths = ['{entry}']")).unwrap();
            let err = Config::load_in(Some(&global), &root, &Paths::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.starts_with(&format!(
                    "invalid config {}: directories.paths entry {entry}: ",
                    repo.display()
                )),
                "{entry}: {message}"
            );
            assert!(message.ends_with("is a symlink loop"), "{entry}: {message}");
        }
        std::fs::write(&repo, "").unwrap();
        std::fs::write(
            &global,
            format!(
                "[directories]\npaths = ['{}']",
                root.join("loop-a").display()
            ),
        )
        .unwrap();
        Config::load_in(Some(&global), &root, &Paths::default()).unwrap();
        std::fs::write(&global, "").unwrap();
        // Outside Git the root is the folder holding `.nth`.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join(".nth")).unwrap();
        std::fs::create_dir_all(plain.join("docs")).unwrap();
        std::fs::write(plain.join(".nth/config"), "[directories]\npaths = ['docs']").unwrap();
        Config::load_layers(Some(&global), &plain, &Paths::default()).unwrap();
        std::fs::write(
            plain.join(".nth/config"),
            "[directories]\npaths = ['../sibling']",
        )
        .unwrap();
        let err = Config::load_layers(Some(&global), &plain, &Paths::default()).unwrap_err();
        assert!(
            format!("{err:#}").contains("is outside the repository"),
            "{err:#}"
        );
    }

    #[test]
    fn nearest_non_git_config_and_git_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        std::fs::write(root.join(".nth/config"), "model = 'outer'").unwrap();
        std::fs::create_dir_all(root.join("nested/.nth")).unwrap();
        std::fs::write(root.join("nested/.nth/config"), "model = 'inner'").unwrap();
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        let global = root.join("global");
        std::fs::write(&global, "").unwrap();
        let model = |cwd: &Path, home: Option<&Path>| {
            let paths = Paths {
                home: home.map(Path::to_path_buf),
                ..Paths::default()
            };
            Config::load_layers(Some(&global), cwd, &paths)
                .unwrap()
                .0
                .model
        };
        assert_eq!(model(&root.join("nested/deeper"), Some(root)), "inner");
        std::fs::remove_file(root.join("nested/.nth/config")).unwrap();
        // Under home the walk goes up to and including home.
        assert_eq!(model(&root.join("nested/deeper"), Some(root)), "outer");
        // A config above home is ignored.
        assert_eq!(
            model(&root.join("nested/deeper"), Some(&root.join("nested"))),
            ""
        );
        // Outside home only cwd's own config counts.
        let home = root.join("home");
        assert_eq!(model(&root.join("nested/deeper"), Some(&home)), "");
        assert_eq!(model(root, Some(&home)), "outer");
        // A repository stops the walk at its root.
        std::fs::create_dir_all(root.join("nested/.git")).unwrap();
        assert_eq!(model(&root.join("nested"), Some(root)), "");
    }

    #[test]
    fn linked_worktree_uses_main_checkout_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("main");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(root.join(".git/worktrees/w")).unwrap();
        std::fs::create_dir_all(root.join(".nth")).unwrap();
        std::fs::create_dir_all(worktree.join(".nth")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", root.join(".git/worktrees/w").display()),
        )
        .unwrap();
        std::fs::write(
            root.join(".nth/config"),
            "model = 'main'\n[skills]\npaths = ['skills']",
        )
        .unwrap();
        std::fs::write(worktree.join(".nth/config"), "model = 'worktree'").unwrap();
        let global = tmp.path().join("global");
        std::fs::write(&global, "").unwrap();
        let config = Config::load_in(Some(&global), &worktree, &Paths::default()).unwrap();
        assert_eq!(config.model, "main");
        assert_eq!(config.skills.paths, [root.join("skills").to_string_lossy()]);
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").expect("parses"), Config::default());
    }

    #[test]
    fn partial_file_changes_only_its_keys() {
        let config = Config::parse(
            r#"
            model = "kimi-k3"

            [tools.bash]
            max_output_chars = 10
            "#,
        )
        .expect("parses");
        let mut expected = Config {
            model: "kimi-k3".into(),
            ..Default::default()
        };
        expected.tools.bash.max_output_chars = 10;
        assert_eq!(config, expected);
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err =
            Config::parse("[provider.opencode]\nmodle = \"x\"").expect_err("typo is rejected");
        assert!(format!("{err:#}").contains("modle"), "{err:#}");
        assert!(Config::parse("[nope]").is_err());
    }

    #[test]
    fn template_matches_the_defaults() {
        assert_eq!(Config::parse(TEMPLATE).expect("parses"), Config::default());
    }

    #[test]
    fn init_writes_the_template_and_creates_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a/b/config.toml");

        write_template(&path, false).expect("writes");

        assert_eq!(
            Config::load_in(Some(&path), dir.path(), &Paths::default()).expect("loads"),
            Config::default()
        );
    }

    #[test]
    fn init_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "mine").expect("write");

        let err = write_template(&path, false).expect_err("file exists");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "mine");

        write_template(&path, true).expect("overwrites");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), TEMPLATE);
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = Config::default().to_toml().expect("serializes");
        assert_eq!(Config::parse(&text).expect("parses"), Config::default());
    }

    #[test]
    fn format_section_takes_custom_formatters() {
        let config = Config::parse(
            r#"
            [format.rustfmt]
            disabled = true

            [format.sed]
            command = ["sed", "-i", "s/a/b/", "$FILE"]
            extensions = [".txt"]
            "#,
        )
        .expect("parses");
        assert!(config.format.enabled);
        assert!(config.format.formatters["rustfmt"].disabled);
        assert!(Config::parse("[format.mine]\nextensions = [\".x\"]").is_err());
    }

    #[test]
    fn lsp_section_overrides_a_server_and_round_trips() {
        let config = Config::parse(
            r#"
            [lsp.rust]
            command = ["ra-multiplex"]

            [lsp.pyright]
            disabled = true
            "#,
        )
        .expect("parses");
        assert!(config.lsp.enabled);
        assert_eq!(config.lsp.servers["rust"].command, ["ra-multiplex"]);
        assert!(config.lsp.servers["pyright"].disabled);

        let text = config.to_toml().expect("serializes");
        assert!(text.contains("[lsp.rust]"), "{text}");
        assert_eq!(Config::parse(&text).expect("parses"), config);
        assert!(Config::parse("[lsp.mine]\ncommand = [\"mine\"]").is_err());
    }

    #[test]
    fn the_first_profile_is_where_a_chat_starts() {
        let config = Config::parse(
            r#"
            [mode]
            default = "act"

            [[profiles]]
            name = "go"

            [profiles.plan]
            model = "kimi-k3"
            effort = "high"

            [profiles.act]
            model = "glm"
            effort = "low"

            [[profiles]]
            name = "zen"

            [profiles.plan]
            model = "zen/gpt"
            "#,
        )
        .expect("parses");

        assert_eq!(config.mode.default, Mode::Act);
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.llm_for(Mode::Plan), ("kimi-k3".into(), Effort::High));
        assert_eq!(config.llm_for(Mode::Act), ("glm".into(), Effort::Low));
        assert_eq!(
            config.profile_llm(&config.profiles[1], Mode::Act),
            ("".into(), Effort::Default),
            "not the first profile"
        );
        assert!(Config::parse("[mode]\ndefault = \"build\"").is_err());
    }

    #[test]
    fn a_profile_names_what_subagents_run_on() {
        let config = Config::parse(
            r#"
            model = "glm"

            [[profiles]]
            name = "go"

            [profiles.subagent]
            model = "minimax"
            effort = "low"

            [[profiles]]
            name = "zen"

            [profiles.subagent]
            effort = "high"
            "#,
        )
        .expect("parses");

        assert_eq!(config.subagent_for(), Some(("minimax".into(), Effort::Low)));
        assert_eq!(
            config.profile_subagent(&config.profiles[1]),
            None,
            "an empty subagent model means the parent's, not the model above"
        );
        assert_eq!(
            Config::parse("model = \"glm\"")
                .expect("parses")
                .subagent_for(),
            None
        );
    }

    #[test]
    fn a_profile_names_what_the_clerk_runs_on() {
        let config = Config::parse(
            r#"
            [[profiles]]
            name = "go"

            [profiles.subagent]
            model = "minimax"

            [profiles.clerk]
            model = "flash"
            effort = "minimal"

            [[profiles]]
            name = "zen"

            [profiles.clerk]
            effort = "low"
            "#,
        )
        .expect("parses");

        assert_eq!(config.clerk_for(), Some(("flash".into(), Effort::Minimal)));
        assert_eq!(
            config.profile_clerk(&config.profiles[1]),
            None,
            "an empty clerk model runs it as the other subagents"
        );
    }

    #[test]
    fn a_profile_model_falls_back_to_the_one_above() {
        let mut config = Config::parse(
            r#"
            [[profiles]]
            name = "go"

            [profiles.plan]
            model = "kimi-k3"
            effort = "high"

            [profiles.act]
            effort = "low"
            "#,
        )
        .expect("parses");

        config.model = "from-file".into();

        assert_eq!(config.llm_for(Mode::Plan), ("kimi-k3".into(), Effort::High));
        assert_eq!(
            config.llm_for(Mode::Act),
            ("from-file".into(), Effort::Low),
            "an empty profile model means the config's"
        );
        assert!(Config::parse("[mode.plan]\nmodel = \"kimi-k3\"").is_err());
    }

    #[test]
    fn without_profiles_both_modes_run_the_model_above() {
        let config = Config::parse("model = \"glm\"\n").expect("parses");
        assert_eq!(config.llm_for(Mode::Plan), ("glm".into(), Effort::Default));
        assert_eq!(config.llm_for(Mode::Act), ("glm".into(), Effort::Default));
        assert!(!config.needs_model());

        let empty = Config::parse("").expect("parses");
        assert!(empty.needs_model(), "no model anywhere to run on");
    }

    #[test]
    fn needs_model_reads_every_profile() {
        let config = Config::parse(
            r#"
            [[profiles]]
            name = "go"

            [profiles.plan]
            model = "kimi-k3"

            [profiles.act]
            model = "glm"

            [[profiles]]
            name = "zen"
            "#,
        )
        .expect("parses");
        assert!(config.needs_model(), "zen's modes would run an empty model");

        let config = Config::parse(
            r#"
            model = "glm"

            [[profiles]]
            name = "go"
            "#,
        )
        .expect("parses");
        assert!(!config.needs_model(), "the model above fills every slot");
    }

    #[test]
    fn a_profile_needs_a_name() {
        let err = Config::parse("[[profiles]]\nname = \"\"").expect_err("no name");
        assert!(err.to_string().contains("profile 1 has no name"), "{err}");

        let err =
            Config::parse("[[profiles]]\n[profiles.plan]\nmodel = \"x\"").expect_err("no name");
        assert!(err.to_string().contains("profile 1 has no name"), "{err}");
    }

    #[test]
    fn profiles_round_trip_through_toml() {
        let config = Config::parse(
            r#"
            [[profiles]]
            name = "go"

            [profiles.plan]
            model = "kimi-k3"
            effort = "high"

            [profiles.act]
            model = "glm"

            [profiles.subagent]
            model = "minimax"

            [[profiles]]
            name = "zen"

            [profiles.act]
            effort = "low"
            "#,
        )
        .expect("parses");

        let text = config.to_toml().expect("serializes");
        assert!(text.contains("[[profiles]]"), "{text}");
        assert_eq!(Config::parse(&text).expect("parses"), config);
    }

    #[test]
    fn a_model_from_the_flag_beats_the_profiles() {
        let mut config = Config::parse(
            r#"
            [[profiles]]
            name = "go"

            [profiles.plan]
            model = "kimi-k3"
            effort = "high"

            [profiles.act]
            model = "glm"
            effort = "low"

            [profiles.subagent]
            model = "minimax"

            [profiles.clerk]
            model = "flash"
            "#,
        )
        .expect("parses");

        config.set_model("from-flag".into());
        assert_eq!(
            config.subagent_for(),
            None,
            "subagents run on their parent's, the flag's model"
        );
        assert_eq!(config.clerk_for(), None, "so does the clerk");

        assert_eq!(
            config.llm_for(Mode::Plan),
            ("from-flag".into(), Effort::High)
        );
        assert_eq!(
            config.llm_for(Mode::Act),
            ("from-flag".into(), Effort::Low),
            "the profile keeps its effort, not its model"
        );
    }

    #[test]
    fn chat_settings_set_where_the_toggles_start() {
        let config = Config::parse(
            r#"
            [chat]
            thinking = false
            tool_output = false
            "#,
        )
        .expect("parses");
        assert!(!config.chat.thinking);
        assert!(!config.chat.tool_output);

        let round = Config::parse(&config.to_toml().expect("serializes")).expect("parses");
        assert_eq!(round, config);
        assert!(Config::parse("[chat]\nthinking = \"x\"").is_err());
        assert!(Config::parse("[chat]\ntoggl = true").is_err());
    }

    #[test]
    fn layout_sets_the_split_width() {
        assert_eq!(Config::default().layout.split_width, 160);
        let config = Config::parse("[layout]\nsplit_width = 0").expect("parses");
        assert_eq!(config.layout.split_width, 0);
        assert!(Config::parse("[layout]\nsplit_width = -1").is_err());
    }

    #[test]
    fn context_takes_the_compaction_settings() {
        let config = Config::parse(
            r#"
            [context]
            auto = false
            reserved = 30000
            preserve = 1000
            "#,
        )
        .expect("parses");
        assert!(!config.context.auto);
        assert_eq!(config.context.reserved, 30000);
        assert_eq!(config.context.preserve, 1000);

        let round = Config::parse(&config.to_toml().expect("serializes")).expect("parses");
        assert_eq!(round, config);
        assert!(Config::parse("[context]\nauto = \"x\"").is_err());
        assert!(
            Config::parse("[context]\naut = true").is_err(),
            "a typo is caught, as in every other table"
        );
    }

    #[test]
    fn a_provider_block_is_the_only_provider() {
        let config = Config::parse(
            r#"
            [provider.lyceum]
            base_url = "https://api.lyceum.technology/api/v2/external/serverless"
            api_key_env = "LYCEUM_API_KEY"
            "#,
        )
        .expect("parses");
        assert_eq!(config.provider.keys().collect::<Vec<_>>(), ["lyceum"]);
        assert_eq!(config.provider["lyceum"].name, "lyceum", "named by id");
        assert_eq!(config.provider["lyceum"].api_key_env, "LYCEUM_API_KEY");
    }

    #[test]
    fn template_providers_parse_when_uncommented() {
        // Uncomments the OpenCode blocks the way a user would: from their
        // header to the first blank comment line.
        let mut text = String::new();
        let mut inside = false;
        for line in TEMPLATE.lines() {
            if line == "# [provider.opencode]" || line == "# [provider.zen]" {
                inside = true;
            } else if line == "#" || !line.starts_with('#') {
                inside = false;
            }
            match line.strip_prefix("# ") {
                Some(rest) if inside => text.push_str(rest),
                _ => text.push_str(line),
            }
            text.push('\n');
        }

        let config = Config::parse(&text).expect("parses");

        assert_eq!(
            config.provider.keys().collect::<Vec<_>>(),
            ["opencode", "zen"]
        );
        assert_eq!(config.provider["opencode"].name, "OpenCode Go");
        assert_eq!(
            config.provider["opencode"].base_url,
            "https://opencode.ai/zen/go/v1"
        );
        assert_eq!(
            config.provider["zen"].base_url,
            "https://opencode.ai/zen/v1"
        );
        assert_eq!(config.provider["zen"].api_key_env, "OPENCODE_API_KEY");
    }

    #[test]
    fn template_profiles_parse_when_uncommented() {
        // Uncomments the profile example the way a user would: from the
        // first `# [[profiles]]` to the end of the commented block.
        let mut text = String::new();
        let mut inside = false;
        for line in TEMPLATE.lines() {
            if line == "# [[profiles]]" {
                inside = true;
            } else if !line.starts_with('#') {
                inside = false;
            }
            match line.strip_prefix("# ") {
                Some(rest) if inside => text.push_str(rest),
                _ => text.push_str(line),
            }
            text.push('\n');
        }

        let config = Config::parse(&text).expect("parses");

        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[0].name, "opencode go");
        assert_eq!(
            config.profile_llm(&config.profiles[0], Mode::Plan),
            ("opencode/deepseek-v4.1-flash".into(), Effort::High)
        );
        assert_eq!(
            config.profile_llm(&config.profiles[0], Mode::Act),
            ("opencode/kimi-k3".into(), Effort::Default)
        );
        assert_eq!(
            config.profile_subagent(&config.profiles[0]),
            Some(("opencode/deepseek-v4.1-flash".into(), Effort::Low))
        );
        assert_eq!(config.profiles[1].name, "zen");
        assert_eq!(
            config.profile_llm(&config.profiles[1], Mode::Act),
            ("zen/glm-5.3".into(), Effort::Default)
        );
    }

    #[test]
    fn a_provider_needs_an_endpoint_and_a_key_variable() {
        let err = Config::parse("[provider.lyceum]\napi_key_env = \"K\"").expect_err("no url");
        assert!(err.to_string().contains("lyceum has no base_url"), "{err}");
        let err = Config::parse("[provider.lyceum]\nbase_url = \"https://x\"").expect_err("no key");
        assert!(
            err.to_string().contains("lyceum has no api_key_env"),
            "{err}"
        );
    }

    #[test]
    fn chatgpt_is_reserved() {
        let err =
            Config::parse("[provider.chatgpt]\nbase_url = \"https://x\"\napi_key_env = \"K\"")
                .expect_err("built in");
        assert!(err.to_string().contains("chatgpt is built in"), "{err}");
    }

    #[test]
    fn skill_paths_expand_home() {
        let config = Config::parse(
            r#"
            [skills]
            paths = ["~/shared/skills", "tools/skills", "/opt/skills"]
            "#,
        )
        .expect("parses");
        let home = Path::new("/home/k");

        let paths: Vec<_> = config
            .skills
            .paths
            .iter()
            .map(|p| skill_path(p, Some(home)))
            .collect();

        assert_eq!(
            paths,
            [
                PathBuf::from("/home/k/shared/skills"),
                PathBuf::from("tools/skills"),
                PathBuf::from("/opt/skills"),
            ]
        );
    }

    #[test]
    fn explicit_path_must_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("config.toml");
        let err = Config::load_in(Some(&missing), dir.path(), &Paths::default())
            .expect_err("missing file");
        assert!(format!("{err:#}").contains("not found"), "{err:#}");

        std::fs::write(&missing, "[session]\nmax_steps = 3\n").expect("write");
        let config = Config::load_in(Some(&missing), dir.path(), &Paths::default()).expect("loads");
        assert_eq!(config.session.max_steps, 3);
    }

    #[cfg(unix)]
    #[test]
    fn path_resolution_follows_symlinks_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(shared.join("team")).unwrap();
        std::fs::create_dir_all(shared.join("docs")).unwrap();
        std::os::unix::fs::symlink(shared.join("team"), repo.join("link")).unwrap();
        assert_eq!(
            resolve_path(&repo.join("link/../docs")).unwrap(),
            shared.join("docs").canonicalize().unwrap()
        );
    }

    #[test]
    fn an_empty_global_formatter_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        std::fs::write(
            &global,
            "[format.custom]\ncommand = []\nextensions = ['.x']\n",
        )
        .unwrap();
        let error = Config::load_in(Some(&global), dir.path(), &Paths::default()).unwrap_err();
        assert!(
            format!("{error:#}").contains("command is empty"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains(&global.display().to_string()));
    }
}
