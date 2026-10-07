use std::{
    collections::BTreeMap,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use nth_context::Paths;
use nth_protocol::{Effort, Mode};
use serde::{Deserialize, Serialize};

/// Everything in `config.toml`. Every key is optional: a missing key keeps
/// the default below, so a config file only lists what it changes.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The model a new chat starts on, as `provider/model`. Before the
    /// tables, since TOML has values ahead of tables.
    pub model: String,
    /// The endpoints by id; the id prefixes their models. OpenCode Go is
    /// built in as `opencode` and listed only to change it.
    pub provider: BTreeMap<String, ProviderConfig>,
    pub session: SessionConfig,
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

pub const BUILT_IN_PROVIDER: &str = "opencode";

impl ProviderConfig {
    fn opencode() -> Self {
        Self {
            name: "OpenCode Go".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            api_key_env: "OPENCODE_API_KEY".into(),
            models: Vec::new(),
        }
    }

    /// Fills what `id`'s block left out: the built-in values for the
    /// built-in provider, and the id as the name for any.
    fn complete(&mut self, id: &str) -> Result<()> {
        if id == BUILT_IN_PROVIDER {
            let built_in = Self::opencode();
            for (field, default) in [
                (&mut self.name, built_in.name),
                (&mut self.base_url, built_in.base_url),
                (&mut self.api_key_env, built_in.api_key_env),
            ] {
                if field.is_empty() {
                    *field = default;
                }
            }
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

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "opencode/deepseek-v4.1-flash".into(),
            provider: BTreeMap::from([(BUILT_IN_PROVIDER.into(), ProviderConfig::opencode())]),
            session: SessionConfig::default(),
            mode: ModeConfig::default(),
            tools: nth_tools::ToolsConfig::default(),
            skills: SkillsConfig::default(),
            format: nth_format::FormatConfig::default(),
            lsp: nth_lsp::LspConfig::default(),
            notify: nth_notify::NotifyConfig::default(),
        }
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
    /// be missing and then gives the built-in defaults.
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

    /// The built-in provider is always there, so a file that only adds
    /// another keeps it.
    fn parse(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        config.provider.entry(BUILT_IN_PROVIDER.into()).or_default();
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
    fn example_file_matches_the_defaults() {
        let example = include_str!("../../../docs/config.example.toml");
        assert_eq!(Config::parse(example).expect("parses"), Config::default());
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
    fn another_provider_keeps_the_built_in_one() {
        let config = Config::parse(
            r#"
            [provider.lyceum]
            base_url = "https://api.lyceum.technology/api/v2/external/serverless"
            api_key_env = "LYCEUM_API_KEY"
            "#,
        )
        .expect("parses");
        assert_eq!(
            config.provider.keys().collect::<Vec<_>>(),
            ["lyceum", "opencode"]
        );
        assert_eq!(config.provider["opencode"], ProviderConfig::opencode());
        assert_eq!(config.provider["lyceum"].name, "lyceum", "named by id");
        assert_eq!(config.provider["lyceum"].api_key_env, "LYCEUM_API_KEY");
    }

    #[test]
    fn the_built_in_provider_block_changes_only_its_keys() {
        let config =
            Config::parse("[provider.opencode]\napi_key_env = \"GO_KEY\"").expect("parses");
        let expected = ProviderConfig {
            api_key_env: "GO_KEY".into(),
            ..ProviderConfig::opencode()
        };
        assert_eq!(config.provider["opencode"], expected);
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
