//! nth's configuration: `Config` and its sections, what `config.toml` and a
//! repository's `.nth/config` hold, the template `nth init` writes, the
//! layering of the two files (`layers`) and what every kind of turn runs on
//! (`selection`). Every front-end reads its settings from here rather than
//! from copies of its own.

mod layers;
mod selection;

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use anyhow::{Result, bail};
pub use layers::repo_config;
use layers::skill_path;
use nth_context::Paths;
use nth_protocol::{Effort, Mode};
pub use selection::{Cycled, Profile, Selection, Slots, Target};
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
pub const TEMPLATE: &str = include_str!("config.toml");

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
    /// `config.toml` in `paths`' config directory: `$XDG_CONFIG_HOME/nth`,
    /// else `~/.config/nth`. The design puts nth's files under
    /// `~/.config/nth` on every platform, so this does not use the macOS
    /// `Library` folder.
    pub fn default_path(paths: &Paths) -> Option<PathBuf> {
        Some(paths.config_dir()?.join("config.toml"))
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

    /// Where nth looks for files about a project, over the global
    /// directories in `paths`: the configured skill folders added and the
    /// memory read when it is on.
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

/// A profile's subagent or clerk slot; `None` when it names no model.
fn slot(defaults: &ModeDefaults) -> Option<(String, Effort)> {
    (!defaults.model.is_empty()).then(|| (defaults.model.clone(), defaults.effort))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
    fn template_parses_to_the_defaults() {
        assert_eq!(Config::parse(TEMPLATE).expect("parses"), Config::default());
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

    /// Every key a config can hold, as (table, key), with the ids of
    /// `[provider.<id>]`, `[format.<name>]` and `[lsp.<id>]` written `*`.
    /// The default config leaves out the keyed tables and `[[profiles]]`,
    /// so a sample of each is added.
    fn every_key() -> Vec<(String, String)> {
        let sample = Config::parse(
            r#"
            [provider.x]
            base_url = "https://x"
            api_key_env = "X"
            models = ["m"]

            [[profiles]]
            name = "p"
            plan = { model = "m" }
            act = { model = "m" }
            subagent = { model = "m" }
            clerk = { model = "m" }

            [format.x]
            disabled = true
            command = ["x"]
            extensions = [".x"]
            env = { A = "a" }

            [lsp.x]
            disabled = true
            command = ["x"]
            extensions = [".x"]
            env = { A = "a" }
            initialization = { a = true }
            "#,
        )
        .expect("parses");
        let value = toml::Value::try_from(&sample).expect("serializes");
        let mut keys = Vec::new();
        collect(&value, &mut Vec::new(), &mut keys);
        keys
    }

    fn collect(value: &toml::Value, path: &mut Vec<String>, keys: &mut Vec<(String, String)>) {
        let Some(table) = value.as_table() else {
            return;
        };
        for (key, value) in table {
            // A map of names to values (`env`, `initialization`) is one key.
            let leaf = !value.is_table() && !is_table_array(value)
                || ["env", "initialization"].contains(&key.as_str());
            if leaf {
                keys.push((keyed(&path.join(".")), key.clone()));
                continue;
            }
            path.push(key.clone());
            match value {
                toml::Value::Array(tables) => {
                    for table in tables {
                        collect(table, path, keys);
                    }
                }
                table => collect(table, path, keys),
            }
            path.pop();
        }
    }

    fn is_table_array(value: &toml::Value) -> bool {
        value
            .as_array()
            .is_some_and(|items| !items.is_empty() && items.iter().all(toml::Value::is_table))
    }

    /// `table` with the id of a keyed table written `*`.
    fn keyed(table: &str) -> String {
        match table.split_once('.') {
            Some((section @ ("provider" | "format" | "lsp"), _)) => format!("{section}.*"),
            _ => table.to_string(),
        }
    }

    /// The template's lines grouped under the table header above them,
    /// commented or not: what the template says about each table.
    fn template_tables() -> Vec<(String, String)> {
        let mut tables = vec![(String::new(), String::new())];
        for line in TEMPLATE.lines() {
            let bare = line.strip_prefix("# ").unwrap_or(line).trim();
            let header = bare
                .strip_prefix("[[")
                .and_then(|rest| rest.strip_suffix("]]"))
                .or_else(|| {
                    bare.strip_prefix('[')
                        .and_then(|rest| rest.strip_suffix(']'))
                });
            match header {
                Some(header) if !header.contains(' ') => {
                    tables.push((keyed(header), String::new()))
                }
                _ => {
                    let (_, text) = tables.last_mut().expect("one table at least");
                    text.push_str(bare);
                    text.push('\n');
                }
            }
        }
        tables
    }

    /// The config skill's reference, split at its bold table headers: the
    /// root keys first, under an empty name.
    fn skill_tables(skill: &str) -> Vec<(String, String)> {
        let (_, reference) = skill.split_once("## Reference").expect("a reference");
        let mut tables = vec![(String::new(), String::new())];
        for line in reference.lines() {
            let header = line
                .strip_prefix("**`[")
                .and_then(|rest| rest.split_once("]`**"))
                .map(|(header, _)| header.trim_matches(['[', ']']));
            match header {
                Some(header) => tables.push((
                    keyed(&header.replace("<id>", "x").replace("<name>", "x")),
                    String::new(),
                )),
                None => {
                    let (_, text) = tables.last_mut().expect("one table at least");
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
        tables
    }

    #[test]
    fn every_key_is_in_the_template_and_the_skill() {
        let skill = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../nth-context/src/skills/internal/config/SKILL.md"),
        )
        .expect("the config skill");
        let template = template_tables();
        let skill = skill_tables(&skill);
        let keys = every_key();
        assert!(keys.len() > 40, "{keys:?}");
        for (table, key) in keys {
            let assigned = |text: &str| {
                text.lines()
                    .any(|line| line.starts_with(&format!("{key} =")))
            };
            assert!(
                template
                    .iter()
                    .any(|(name, text)| *name == table && assigned(text)),
                "`{key}` of [{table}] is not in the template"
            );
            // The skill lists a table's subtables in its own rows, as
            // `plan` / `act` `model` under `[[profiles]]`: the row's first
            // cell names the subtable and ends with the key.
            let mut parts: Vec<&str> = table.split('.').filter(|p| !p.is_empty()).collect();
            let mut subtables = Vec::new();
            let found = loop {
                let name = parts.join(".");
                if let Some((_, text)) = skill.iter().find(|(table, _)| *table == name) {
                    break text;
                }
                subtables.push(parts.pop().expect("the root keys are a table"));
            };
            let listed = |cell: &str| match subtables.as_slice() {
                [] => cell == format!("`{key}`"),
                subtables => {
                    cell.ends_with(&format!(" `{key}`"))
                        && subtables
                            .iter()
                            .all(|sub| cell.contains(&format!("`{sub}`")))
                }
            };
            assert!(
                found.lines().any(|row| {
                    row.strip_prefix('|')
                        .and_then(|row| row.split('|').next())
                        .is_some_and(|cell| listed(cell.trim()))
                }),
                "`{key}` of [{table}] is not a row of the config skill's reference"
            );
        }
    }
}
