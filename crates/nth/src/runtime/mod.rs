//! Starting a session and what it runs with, the one way every front-end
//! does it: the chat when it opens and on `/resume`, `nth run` and `nth
//! context`. Each loads the config for where the session runs, builds its
//! providers and hands both to `prepare`.

mod llms;
mod providers;
mod tools;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::Result;
use futures::FutureExt;
use nth_config::Config;
use nth_context::Paths;
use nth_llm::Providers;
use nth_protocol::{Llm, Mode, Provider, SubagentLlm};
use nth_session::{Session, Subagents};
use nth_tui::PreparedRuntime;
pub(crate) use tools::{post_write, tools};

/// What the command line decided once: the config file, the model that
/// overrides the config's, the global directories, and the one ChatGPT
/// login of this process, which every provider, `/connect` and `nth
/// connect` share.
pub struct Runtime {
    config_path: Option<PathBuf>,
    model: Option<String>,
    /// The home, config and data directories, read from the environment
    /// once: where the global config, a session's global files and the
    /// login are. Tests point them at a temporary one.
    env: Paths,
    /// Built on first use, so a command that never signs in (`nth init`,
    /// `nth curio`) runs without a data directory.
    chatgpt: OnceLock<nth_auth::ChatGpt>,
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
        Self {
            config_path,
            model,
            env: Paths::from_env(),
            chatgpt: OnceLock::new(),
        }
    }

    /// The runtime with its global directories at `env`, so nothing reads
    /// the real home directory.
    #[cfg(test)]
    pub(crate) fn with_paths(mut self, env: Paths) -> Self {
        self.env = env;
        self
    }

    /// Where a session on `config` finds its global files and skills.
    pub(crate) fn paths(&self, config: &Config) -> Paths {
        config.paths_over(self.env.clone())
    }

    /// The global config file: `--config` or `NTH_CONFIG`, else the one in
    /// the config directory. `None` without a home directory.
    pub fn config_file(&self) -> Option<PathBuf> {
        self.config_path
            .clone()
            .or_else(|| Config::default_path(&self.env))
    }

    /// The home, config and data directories the runtime reads.
    pub fn env(&self) -> &Paths {
        &self.env
    }

    /// The process's ChatGPT login: every call a clone of one handle, so
    /// its clones refresh the token once between them.
    pub fn chatgpt(&self) -> Result<nth_auth::ChatGpt> {
        if let Some(chatgpt) = self.chatgpt.get() {
            return Ok(chatgpt.clone());
        }
        let built = providers::chatgpt(&self.env)?;
        // Should two threads build one at once, both get the one kept.
        Ok(self.chatgpt.get_or_init(|| built).clone())
    }

    /// The providers `config` names, ChatGPT's on the process's login,
    /// keeping the models.dev catalogue in the data directory.
    pub fn providers(&self, config: &Config) -> Result<Providers> {
        let cache = self.env.nth_data().map(|dir| dir.join("models.json"));
        providers::providers(config, &self.chatgpt()?, cache)
    }

    /// The config for `cwd`, with the model override applied over it.
    pub fn load(&self, cwd: &Path) -> Result<Config> {
        let mut config = Config::load_in(self.config_path.as_deref(), cwd, &self.env)?;
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

    /// What `/resume` prepares a saved session with: `resume`, then
    /// `prepare`, so the session comes back as `nth -c` would bring it.
    pub fn reload(self, subagents: Subagents) -> nth_tui::Reload {
        let runtime = Arc::new(self);
        Arc::new(move |session| {
            let (runtime, subagents) = (runtime.clone(), subagents.clone());
            async move {
                // Loading the config asks git about the repository and
                // building the providers reads the ChatGPT login: off the
                // runtime's workers, which the chat's loop shares.
                let blocking = runtime.clone();
                let (start, config, providers) =
                    tokio::task::spawn_blocking(move || blocking.resume(session)).await??;
                runtime
                    .prepare(config, Arc::new(providers), start, subagents)
                    .await
            }
            .boxed()
        })
    }

    /// The session `start` names, with everything it runs with, from `config`
    /// and through `providers`: its instruction files, directories, limits,
    /// llms, tools and the shell for `!` commands. A fresh start on a config
    /// without a model runs on one picked from what the providers list,
    /// written into the config the runtime carries on. What is worth telling
    /// you, the pick and directories that cannot be added, comes back in
    /// `warnings`; the instruction files' own warnings stay on the context.
    pub async fn prepare(
        &self,
        mut config: Config,
        providers: Arc<Providers>,
        start: Start,
        subagents: Subagents,
    ) -> Result<PreparedRuntime> {
        let mut warnings = Vec::new();
        match &start {
            // `config_for` did this already; done again so a config loaded
            // any other way still gives the other mode the session's model.
            Start::Saved(session) => resumed_config(&mut config, session),
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
        let paths = self.paths(&config);
        let context =
            Arc::new(nth_context::Context::load(start.cwd().to_path_buf(), paths.clone()).await);
        let mut selection = llms::selection(&config, &providers);
        let mut session = match start {
            Start::Fresh { cwd, mode } => {
                selection.mode = mode;
                let Llm { model, effort } = selection.current();
                let mut session = Session::new(model, cwd).with_context(context);
                session.mode = mode;
                session.effort = effort;
                let picked = |llm: &Llm| SubagentLlm {
                    llm: llm.clone(),
                    ..SubagentLlm::default()
                };
                session.subagent_llm = selection.slots().subagent.as_ref().map(picked);
                session.clerk_llm = selection.slots().clerk.as_ref().map(picked);
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
                // A saved session keeps its model, for its mode or for the
                // agent it runs as; the other slots start on the config's.
                selection.resume(
                    session.mode,
                    session.primary().map(|agent| agent.name.clone()),
                    Llm {
                        model: session.model.clone(),
                        effort: session.effort,
                    },
                );
                *session
            }
        };
        warnings.extend(apply_directories(&mut session, &config));
        session.max_steps = config.session.max_steps;
        session.tool_timeout = config.session.tool_timeout();
        // The configured compaction policy; the limits come from a listing once
        // the model is final, so preparing waits on no endpoint. A saved
        // session keeps the limits it saved until then.
        session.compaction = config.context;
        let post_write = post_write(&config);
        let checkers = tools::checkers(&post_write, &config);
        let provider: Arc<dyn Provider> = providers;
        let model_tools = Arc::new(tools(&config, post_write, provider.clone(), subagents));
        Ok(PreparedRuntime {
            session,
            paths,
            provider,
            model_tools,
            // You watch what you run and stop it yourself.
            shell: Arc::new(nth_tools::Bash::untimed(config.tools.bash.clone())),
            checkers,
            selection,
            warnings,
            config,
        })
    }

    /// The saved `session` to start, the config of its directory and the
    /// providers built from it: what `/resume` needs before `prepare`.
    /// Blocks, on git and the login file.
    fn resume(&self, session: Session) -> Result<(Start, Config, Providers)> {
        let start = Start::Saved(Box::new(session));
        let config = self.config_for(&start)?;
        let providers = self.providers(&config)?;
        Ok((start, config, providers))
    }
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
    use nth_config::Slots;
    use nth_llm::{Credentials, EndpointConfig};
    use nth_protocol::Effort;

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

    /// Home, config and data directories inside `dir`, empty, so a test
    /// never reads your instruction files, skills or login.
    pub(crate) fn paths(dir: &Path) -> Paths {
        Paths {
            home: Some(dir.join("home")),
            config_home: Some(dir.join("config")),
            data_home: Some(dir.join("data")),
            ..Paths::default()
        }
    }

    /// A runtime on `paths(dir)`, reading the config at `config_path`.
    pub(crate) fn runtime(dir: &Path, config_path: Option<PathBuf>) -> Runtime {
        Runtime::new(config_path, None).with_paths(paths(dir))
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
        let config = config("model = \"zen/b\"");

        let prepared = runtime(dir.path(), None)
            .prepare(config, zen(&[]), Start::Saved(Box::new(saved)), subagents())
            .await
            .expect("prepares");

        assert_eq!(prepared.session.model, "zen/a");
        assert_eq!(prepared.selection.slots().act.model, "zen/a");
    }

    #[tokio::test]
    async fn prepare_fills_a_fresh_session_from_the_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(
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

        let prepared = runtime(dir.path(), None)
            .prepare(config, zen(&[]), start, subagents())
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
            prepared.selection.slots(),
            &Slots {
                plan: llm("zen/a", Effort::High),
                act: llm("zen/b", Effort::Default),
                subagent: Some(llm("zen/c", Effort::Low)),
                clerk: Some(llm("zen/d", Effort::Minimal)),
            }
        );
        assert!(prepared.warnings.is_empty(), "{:?}", prepared.warnings);
        // Your own instruction files stay out of a test's session.
        assert_eq!(session.context().instructions, []);
    }

    #[tokio::test]
    async fn a_saved_session_runs_on_its_own_llm_in_its_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut saved = Session::new("zen/a", dir.path().into());
        saved.mode = Mode::Plan;
        saved.effort = Effort::High;
        let config = config("model = \"zen/b\"");

        let prepared = runtime(dir.path(), None)
            .prepare(config, zen(&[]), Start::Saved(Box::new(saved)), subagents())
            .await
            .expect("prepares");

        assert_eq!(prepared.selection.slots().plan, llm("zen/a", Effort::High));
        assert_eq!(
            prepared.selection.slots().act,
            llm("zen/b", Effort::Default)
        );
    }

    #[tokio::test]
    async fn a_missing_model_is_picked_and_warned_about() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config("");
        let start = Start::Fresh {
            cwd: dir.path().into(),
            mode: Mode::Act,
        };

        let prepared = runtime(dir.path(), None)
            .prepare(config, zen(&["a"]), start, subagents())
            .await
            .expect("prepares");

        assert_eq!(prepared.config.model, "zen/a");
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
        let config = config("model = \"zen/a\"\n[context]\nauto = false\nreserved = 30000");

        let prepared = runtime(dir.path(), None)
            .prepare(config, zen(&[]), Start::Saved(Box::new(saved)), subagents())
            .await
            .expect("prepares");

        let session = &prepared.session;
        assert!(!session.compaction.auto);
        assert_eq!(session.compaction.reserved, 30_000);
        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.max_output, Some(64_000));
    }

    #[test]
    fn a_saved_sessions_model_fills_the_config_it_resumes_with() {
        // Before the providers are built from it, so their default is the
        // session's provider; `--model` still wins.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").expect("writes");
        let start = Start::Saved(Box::new(Session::new("zen/a", dir.path().into())));

        let config = runtime(dir.path(), Some(path.clone()))
            .config_for(&start)
            .expect("loads");
        assert_eq!(config.model, "zen/a");

        let config = Runtime::new(Some(path), Some("go/b".into()))
            .with_paths(paths(dir.path()))
            .config_for(&start)
            .expect("loads");
        assert_eq!(config.model, "go/b");
    }

    #[test]
    fn one_login_is_shared_by_providers_connect_and_reload() {
        // Off a Tokio runtime, so building providers fetches no catalogue.
        // The login in the test's data directory is not connected, so the
        // providers need a key. `reload` is `resume` then `prepare` on a
        // blocking thread; its closure hands back only a `dyn Provider`, so
        // `resume` is where the reload's login is seen.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            model = "zen/a"

            [provider.zen]
            base_url = "http://127.0.0.1"
            # Cargo sets it when it runs the tests; the test binary run
            # by hand fails here on a key that is not set.
            api_key_env = "CARGO_PKG_NAME"

            [notify]
            enabled = false
            "#,
        )
        .expect("writes");
        let runtime = runtime(dir.path(), Some(path));
        let login = runtime.chatgpt().expect("a login");
        let config = runtime.load(dir.path()).expect("loads");

        let providers = runtime.providers(&config).expect("providers");
        let endpoint = providers.chatgpt().expect("the built-in endpoint");
        assert!(endpoint.same_store(&login));

        let saved = Session::new("zen/a", dir.path().into());
        let (_, _, resumed) = runtime.resume(saved).expect("resumes");
        let endpoint = resumed.chatgpt().expect("the built-in endpoint");
        assert!(endpoint.same_store(&login));

        let settings = crate::chat::settings(runtime, subagents()).expect("settings");
        assert!(settings.chatgpt.same_store(&login));
    }

    #[test]
    fn a_configured_model_is_kept_on_resume() {
        let session = Session::new("zen/a", PathBuf::from("/repo"));
        let mut config = config("model = \"zen/b\"");

        resumed_config(&mut config, &session);

        assert_eq!(config.model, "zen/b");
    }
}
