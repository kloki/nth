//! Starting a session and what it runs with, the one way every front-end
//! does it: the chat when it opens and on `/resume`, `nth run` and `nth
//! context`. Each loads the config for where the session runs, builds its
//! providers and hands both to `prepare`.

mod llms;
mod providers;
mod tools;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use futures::FutureExt;
use nth_llm::Providers;
use nth_protocol::{Effort, Llm, Mode, Provider, SubagentLlm};
use nth_session::{Session, Subagents};
use nth_tui::PreparedRuntime;
pub(crate) use providers::{chatgpt, providers};
pub(crate) use tools::{post_write, tools};

use crate::config::Config;

/// What the command line decided once: the config file and the model that
/// overrides the config's.
pub struct Runtime {
    config_path: Option<PathBuf>,
    model: Option<String>,
}

/// A session to start: a fresh one in `mode` in `cwd`, or a saved one,
/// which comes back as it was.
pub enum Start {
    Fresh { cwd: PathBuf, mode: Mode },
    Saved(Box<Session>),
}

impl Start {
    /// Where the session runs, whose config it takes.
    fn cwd(&self) -> &Path {
        match self {
            Start::Fresh { cwd, .. } => cwd,
            Start::Saved(session) => &session.cwd,
        }
    }
}

impl Runtime {
    pub fn new(config_path: Option<PathBuf>, model: Option<String>) -> Self {
        Self { config_path, model }
    }

    /// The config for `cwd`, with the model override applied over it.
    pub fn load(&self, cwd: &Path) -> Result<Config> {
        let mut config = Config::load_for(self.config_path.as_deref(), cwd)?;
        if let Some(model) = &self.model {
            config.set_model(model.clone());
        }
        Ok(config)
    }

    /// The config `start` runs with: `load` for its directory, with a saved
    /// session's model filling an empty config model before the providers
    /// are built from it, so a bare id goes to the session's provider.
    pub fn config_for(&self, start: &Start) -> Result<Config> {
        let mut config = self.load(start.cwd())?;
        if let Start::Saved(session) = start {
            resumed_config(&mut config, session);
        }
        Ok(config)
    }

    /// What `/resume` prepares a saved session with: the config of the
    /// session's directory, providers built from it, and `prepare`, so the
    /// session comes back as `nth -c` would bring it.
    pub fn reload(self, subagents: Subagents) -> nth_tui::Reload {
        let runtime = Arc::new(self);
        Arc::new(move |session| {
            let (runtime, subagents) = (runtime.clone(), subagents.clone());
            async move {
                let start = Start::Saved(Box::new(session));
                let mut config = runtime.config_for(&start)?;
                let providers = Arc::new(providers(&config)?);
                prepare(&mut config, providers, start, subagents).await
            }
            .boxed()
        })
    }
}

/// The session `start` names, with everything it runs with, from `config`
/// and through `providers`: its instruction files, directories, limits,
/// llms, tools and the shell for `!` commands. A fresh start on a config
/// without a model runs on one picked from what the providers list,
/// written into `config` so the caller sees it too. What is worth telling
/// you, the pick and directories that cannot be added, comes back in
/// `warnings`; the instruction files' own warnings stay on the context.
pub async fn prepare(
    config: &mut Config,
    providers: Arc<Providers>,
    start: Start,
    subagents: Subagents,
) -> Result<PreparedRuntime> {
    let mut warnings = Vec::new();
    match &start {
        // `config_for` did this already; done again so a config loaded
        // any other way still gives the other mode the session's model.
        Start::Saved(session) => resumed_config(config, session),
        // The model above fills in for every profile slot that has none,
        // and both modes when the config lists no profiles at all.
        Start::Fresh { .. } if config.needs_model() => {
            config.model = providers::random_model(&providers).await?;
            warnings.push(format!(
                "no model set, picked {} (set model in the config or pass --model)",
                config.model
            ));
        }
        Start::Fresh { .. } => {}
    }
    let paths = config.paths();
    let context =
        Arc::new(nth_context::Context::load(start.cwd().to_path_buf(), paths.clone()).await);
    let mut session = match start {
        Start::Fresh { cwd, mode } => {
            let (model, effort) = config.llm_for(mode);
            let mut session = Session::new(providers.qualify(&model), cwd).with_context(context);
            session.mode = mode;
            session.effort = effort;
            let picked = |(model, effort): (String, Effort)| SubagentLlm {
                llm: Llm {
                    model: providers.qualify(&model),
                    effort,
                },
                ..SubagentLlm::default()
            };
            session.subagent_llm = config.subagent_for().map(picked);
            session.clerk_llm = config.clerk_for().map(picked);
            session
        }
        Start::Saved(mut session) => {
            session.set_context(context);
            // Saved before there were several providers, perhaps: shown and
            // compared as provider/model like the listed ones.
            let model = providers.qualify(&session.model);
            if model != session.model {
                session.set_model(model);
            }
            *session
        }
    };
    warnings.extend(apply_directories(&mut session, config));
    session.max_steps = config.session.max_steps;
    session.tool_timeout = config.session.tool_timeout();
    // The configured compaction policy; the limits come from a listing once
    // the model is final, so preparing waits on no endpoint. A saved
    // session keeps the limits it saved until then.
    session.compaction = config.context;
    let (mut mode_llms, profiles) = llms::model_settings(config, &providers);
    // A saved session keeps its model; the other mode starts on the
    // config's. One running as an agent keeps it for the agent, which the
    // chat takes from the session.
    let current = Llm {
        model: session.model.clone(),
        effort: session.effort,
    };
    match (session.primary(), session.mode) {
        (Some(_), _) => {}
        (None, Mode::Plan) => mode_llms.plan = current,
        (None, Mode::Act) => mode_llms.act = current,
    }
    let post_write = post_write(config);
    let checkers = tools::checkers(&post_write, config);
    let provider: Arc<dyn Provider> = providers;
    let model_tools = Arc::new(tools(config, post_write, provider.clone(), subagents));
    Ok(PreparedRuntime {
        session,
        paths,
        provider,
        model_tools,
        // You watch what you run and stop it yourself.
        shell: Arc::new(nth_tools::Bash::untimed(config.tools.bash.clone())),
        checkers,
        mode_llms,
        profiles,
        warnings,
    })
}

/// A resumed session runs on its own model, so a config that names none
/// takes it as the default the other mode, subagents and clerk start on.
fn resumed_config(config: &mut Config, session: &Session) {
    if config.needs_model() {
        config.model = session.model.clone();
    }
}

/// Add configured directories to a session without losing manually added ones.
fn apply_directories(session: &mut Session, config: &Config) -> Vec<String> {
    let configured: Vec<_> = config.directories.paths.iter().map(PathBuf::from).collect();
    let resolved =
        nth_session::directories::resolve(&session.cwd, &session.extra_dirs, &configured);
    let warnings = resolved
        .warnings
        .into_iter()
        .filter_map(|warning| {
            use nth_session::directories::Refusal;
            let message = match warning {
                Refusal::Inaccessible { path, error } => format!("{}: {error}", path.display()),
                Refusal::NotDirectory(path) => format!("{} is not a directory", path.display()),
                Refusal::Overlap { .. } => return None,
            };
            Some(format!("directories.paths: {message}"))
        })
        .collect();
    session.set_extra_dirs(resolved.directories);
    warnings
}

#[cfg(test)]
pub(crate) mod tests {
    use nth_llm::{Credentials, EndpointConfig};
    use nth_tui::ModeLlms;

    use super::*;

    /// Providers with one endpoint, `zen`, serving `models` without a
    /// request, and no catalogue, so nothing is fetched.
    pub(crate) fn zen(models: &[&str]) -> Arc<Providers> {
        let providers = Providers::offline(
            vec![EndpointConfig {
                id: "zen".into(),
                name: "Zen".into(),
                base_url: "http://127.0.0.1".into(),
                credentials: Credentials::Key("unused".into()),
                models: models.iter().map(|m| m.to_string()).collect(),
            }],
            Vec::new(),
            "",
        )
        .expect("providers");
        Arc::new(providers)
    }

    fn config(text: &str) -> Config {
        toml::from_str(text).expect("config")
    }

    fn subagents() -> Subagents {
        let (tx, _) = tokio::sync::mpsc::channel(1);
        Subagents::new(tx)
    }

    fn llm(model: &str, effort: Effort) -> Llm {
        Llm {
            model: model.into(),
            effort,
        }
    }

    #[tokio::test]
    async fn prepare_qualifies_a_saved_sessions_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let saved = Session::new("a", dir.path().into());
        let mut config = config("model = \"zen/b\"");

        let prepared = prepare(
            &mut config,
            zen(&[]),
            Start::Saved(Box::new(saved)),
            subagents(),
        )
        .await
        .expect("prepares");

        assert_eq!(prepared.session.model, "zen/a");
        assert_eq!(prepared.mode_llms.act.model, "zen/a");
    }

    #[tokio::test]
    async fn prepare_fills_a_fresh_session_from_the_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = config(
            r#"
            [session]
            max_steps = 7

            [context]
            auto = false
            reserved = 30000
            preserve = 1000

            [[profiles]]
            name = "p"

            [profiles.plan]
            model = "a"
            effort = "high"

            [profiles.act]
            model = "b"

            [profiles.subagent]
            model = "c"
            effort = "low"

            [profiles.clerk]
            model = "d"
            effort = "minimal"
            "#,
        );
        let start = Start::Fresh {
            cwd: dir.path().into(),
            mode: Mode::Plan,
        };

        let prepared = prepare(&mut config, zen(&[]), start, subagents())
            .await
            .expect("prepares");

        let session = &prepared.session;
        assert_eq!(session.cwd, dir.path());
        assert_eq!(session.mode, Mode::Plan);
        assert_eq!(session.model, "zen/a");
        assert_eq!(session.effort, Effort::High);
        assert_eq!(session.max_steps, 7);
        assert!(!session.compaction.auto);
        assert_eq!(session.compaction.reserved, 30_000);
        assert_eq!(session.compaction.preserve, 1_000);
        let subagent = session.subagent_llm.as_ref().expect("subagent llm");
        assert_eq!(subagent.llm, llm("zen/c", Effort::Low));
        let clerk = session.clerk_llm.as_ref().expect("clerk llm");
        assert_eq!(clerk.llm, llm("zen/d", Effort::Minimal));
        assert_eq!(
            prepared.mode_llms,
            ModeLlms {
                plan: llm("zen/a", Effort::High),
                act: llm("zen/b", Effort::Default),
                subagent: Some(llm("zen/c", Effort::Low)),
                clerk: Some(llm("zen/d", Effort::Minimal)),
            }
        );
        assert!(prepared.warnings.is_empty(), "{:?}", prepared.warnings);
    }

    #[tokio::test]
    async fn a_saved_session_runs_on_its_own_llm_in_its_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut saved = Session::new("zen/a", dir.path().into());
        saved.mode = Mode::Plan;
        saved.effort = Effort::High;
        let mut config = config("model = \"zen/b\"");

        let prepared = prepare(
            &mut config,
            zen(&[]),
            Start::Saved(Box::new(saved)),
            subagents(),
        )
        .await
        .expect("prepares");

        assert_eq!(prepared.mode_llms.plan, llm("zen/a", Effort::High));
        assert_eq!(prepared.mode_llms.act, llm("zen/b", Effort::Default));
    }

    #[tokio::test]
    async fn a_missing_model_is_picked_and_warned_about() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = config("");
        let start = Start::Fresh {
            cwd: dir.path().into(),
            mode: Mode::Act,
        };

        let prepared = prepare(&mut config, zen(&["a"]), start, subagents())
            .await
            .expect("prepares");

        assert_eq!(config.model, "zen/a");
        assert_eq!(prepared.session.model, "zen/a");
        assert_eq!(
            prepared.warnings,
            ["no model set, picked zen/a (set model in the config or pass --model)"]
        );
    }

    #[tokio::test]
    async fn a_context_config_reaches_a_resumed_session() {
        // The saved limits stay for the first turn's compaction until a
        // listing sizes the session again.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut saved = Session::new("zen/a", dir.path().into());
        saved.set_limits(Some(200_000), Some(64_000));
        let mut config = config("model = \"zen/a\"\n[context]\nauto = false\nreserved = 30000");

        let prepared = prepare(
            &mut config,
            zen(&[]),
            Start::Saved(Box::new(saved)),
            subagents(),
        )
        .await
        .expect("prepares");

        let session = &prepared.session;
        assert!(!session.compaction.auto);
        assert_eq!(session.compaction.reserved, 30_000);
        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.max_output, Some(64_000));
    }

    #[tokio::test]
    async fn a_resumed_session_fills_the_missing_model_for_every_slot() {
        let a = || llm("zen/a", Effort::default());
        for text in ["", "[[profiles]]\nname = \"p\"\n"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let saved = Session::new("zen/a", dir.path().into());
            let mut config = config(text);
            assert!(config.needs_model());

            let prepared = prepare(
                &mut config,
                zen(&[]),
                Start::Saved(Box::new(saved)),
                subagents(),
            )
            .await
            .expect("prepares");

            // Subagents and the clerk name no model of their own, so they
            // run on their parent's: `zen/a` too.
            let expected = ModeLlms {
                plan: a(),
                act: a(),
                subagent: None,
                clerk: None,
            };
            assert_eq!(prepared.mode_llms, expected, "{text:?}");
            for profile in prepared.profiles {
                assert_eq!(profile.llms, expected, "{text:?}");
            }
        }
    }

    #[test]
    fn a_configured_model_is_kept_on_resume() {
        let session = Session::new("zen/a", PathBuf::from("/repo"));
        let mut config = config("model = \"zen/b\"");

        resumed_config(&mut config, &session);

        assert_eq!(config.model, "zen/b");
    }
}
