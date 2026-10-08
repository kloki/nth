use std::{
    collections::BTreeMap,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use nth_context::Paths;
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
    pub task: TaskConfig,
    pub mode: ModeConfig,
    pub tools: nth_tools::ToolsConfig,
    pub skills: SkillsConfig,
    pub format: nth_format::FormatConfig,
    pub lsp: nth_lsp::LspConfig,
    pub notify: nth_notify::NotifyConfig,
}

/// An OpenAI-compatible chat completions endpoint, the one protocol nth
/// speaks, so there is no kind to pick.
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
    /// without an endpoint or a key variable.
    fn complete(&mut self, id: &str) -> Result<()> {
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_steps: nth_session::DEFAULT_MAX_STEPS,
        }
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
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_steps: 50,
            timeout_secs: 600,
        }
    }
}

impl TaskConfig {
    pub fn limits(&self) -> nth_session::subagent::Limits {
        nth_session::subagent::Limits {
            max_steps: self.max_steps,
            timeout: (self.timeout_secs > 0).then(|| Duration::from_secs(self.timeout_secs)),
        }
    }
}

/// Which mode a new chat starts in, and the model and effort each mode
/// runs with.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModeConfig {
    /// Before the tables, since TOML has values ahead of tables.
    pub default: Mode,
    pub plan: ModeDefaults,
    pub act: ModeDefaults,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            default: Mode::Plan,
            plan: ModeDefaults::default(),
            act: ModeDefaults::default(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModeDefaults {
    /// Empty means the config's model, so `--model` reaches every mode
    /// that has none of its own.
    pub model: String,
    pub effort: Effort,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Extra folders searched for `SKILL.md` files, after the standard ones.
    /// `~/` is the home directory; relative paths are from the working
    /// directory.
    pub paths: Vec<String>,
}

impl Config {
    /// `$XDG_CONFIG_HOME/nth/config.toml`, else `~/.config/nth/config.toml`.
    /// The design puts nth's files under `~/.config/nth` on every platform,
    /// so this does not use the macOS `Library` folder.
    pub fn default_path() -> Option<PathBuf> {
        Some(Paths::from_env().config_dir()?.join("config.toml"))
    }

    /// The model from `--model` or `NTH_MODEL`. It is for every mode, so
    /// the per-mode models go: those only fill in when nothing was given.
    pub fn set_model(&mut self, model: String) {
        self.model = model;
        self.mode.plan.model.clear();
        self.mode.act.model.clear();
    }

    /// The model and effort `mode` runs with.
    pub fn llm_for(&self, mode: Mode) -> (String, Effort) {
        let defaults = match mode {
            Mode::Plan => &self.mode.plan,
            Mode::Act => &self.mode.act,
        };
        let model = match defaults.model.as_str() {
            "" => self.model.clone(),
            model => model.to_string(),
        };
        (model, defaults.effort)
    }

    /// Where nth looks for files about a project, with the configured skill
    /// folders added.
    pub fn paths(&self) -> Paths {
        let mut paths = Paths::from_env();
        paths.skill_paths = self
            .skills
            .paths
            .iter()
            .map(|path| expand_home(path, paths.home.as_deref()))
            .collect();
        paths
    }

    /// Loads `path`, which must exist, or else the default path, which may
    /// be missing and then gives the defaults.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        if let Some(path) = path {
            return Self::load_file(path);
        }
        match Self::default_path() {
            Some(path) if path.exists() => Self::load_file(&path),
            _ => Ok(Self::default()),
        }
    }

    fn load_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow::anyhow!("config file {} not found", path.display()),
            _ => anyhow::Error::new(e).context(format!("cannot read {}", path.display())),
        })?;
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        for (id, provider) in &mut config.provider {
            provider.complete(id)?;
        }
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string(self)?)
    }
}

/// `nth config`: the config in use with every default filled in, and on
/// stderr where it came from.
pub fn show(explicit: Option<PathBuf>, config: &Config) -> Result<()> {
    use owo_colors::OwoColorize;

    match explicit.or_else(Config::default_path) {
        Some(path) if path.exists() => {
            eprintln!("{} {}", "✓".green().bold(), path.display().dimmed())
        }
        Some(path) => eprintln!(
            "{} {}",
            "→".cyan().bold(),
            format!("no {}, using defaults", path.display()).dimmed()
        ),
        None => eprintln!(
            "{} {}",
            "→".cyan().bold(),
            "no home dir, using defaults".dimmed()
        ),
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
    eprintln!("{} wrote {}", "✓".green().bold(), path.display().dimmed());
    eprintln!(
        "{} {}",
        "→".cyan().bold(),
        "uncomment a [provider.<id>] in it, export its key and set model".dimmed()
    );
    Ok(())
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

/// `~/x` is `x` in the home directory. Everything else, relative paths
/// included, is left for the caller to resolve.
fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => home.map_or_else(|| PathBuf::from(path), Path::to_path_buf),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(Config::load(Some(&path)).expect("loads"), Config::default());
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
    fn each_mode_falls_back_to_the_provider_model() {
        let mut config = Config::parse(
            r#"
            [mode]
            default = "act"

            [mode.plan]
            model = "kimi-k3"
            effort = "high"
            "#,
        )
        .expect("parses");
        assert_eq!(config.mode.default, Mode::Act);
        assert_eq!(config.llm_for(Mode::Plan), ("kimi-k3".into(), Effort::High));

        config.model = "from-file".into();
        assert_eq!(
            config.llm_for(Mode::Act),
            ("from-file".into(), Effort::Default)
        );
        assert!(Config::parse("[mode]\ndefault = \"build\"").is_err());
    }

    #[test]
    fn a_model_from_the_flag_beats_the_per_mode_ones() {
        let mut config = Config::parse(
            r#"
            [mode.plan]
            model = "kimi-k3"
            effort = "high"
            "#,
        )
        .expect("parses");

        config.set_model("from-flag".into());

        assert_eq!(
            config.llm_for(Mode::Plan),
            ("from-flag".into(), Effort::High)
        );
        assert_eq!(config.llm_for(Mode::Act).0, "from-flag");
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
            .map(|p| expand_home(p, Some(home)))
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
        let err = Config::load(Some(&missing)).expect_err("missing file");
        assert!(format!("{err:#}").contains("not found"), "{err:#}");

        std::fs::write(&missing, "[session]\nmax_steps = 3\n").expect("write");
        let config = Config::load(Some(&missing)).expect("loads");
        assert_eq!(config.session.max_steps, 3);
    }
}
