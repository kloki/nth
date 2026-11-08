//! Plan and act: Tab switches between them, and each keeps its own model
//! and effort, so planning can run on a different model than acting.
//! Subagents have a third, which by default is the parent's. A primary
//! agent picked with `/agents` runs in place of the mode, on a model of its
//! own for this run, until Tab goes back to the mode. The rules are
//! `Selection`'s; this is what the keys and pickers call.

use nth_config::Selection;
use nth_protocol::{Llm, Mode};
use nth_session::Session;

use super::App;

/// What a turn runs as and on: the mode, the agent running in its place,
/// and the model and effort.
pub(super) type Steerable = (Mode, Option<String>, Llm);

/// `selection` running as and on what `session` does.
pub(super) fn of_session(mut selection: Selection, session: &Session) -> Selection {
    follow(&mut selection, session);
    selection
}

/// Puts `selection` on what `session` runs as and on: its mode, the agent
/// it runs as (in its mode when that agent is gone) and its model. The
/// other slots keep theirs.
pub(super) fn follow(selection: &mut Selection, session: &Session) {
    selection.resume(
        session.mode,
        session.primary().map(|agent| agent.name.clone()),
        Llm {
            model: session.model.clone(),
            effort: session.effort,
        },
    );
}

impl App {
    /// What the next turn runs as and on.
    pub(super) fn runs_as(&self) -> Steerable {
        let selection = &self.selection;
        (selection.mode, selection.agent.clone(), selection.current())
    }

    /// What each kind of turn runs with.
    #[cfg(test)]
    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = selection;
        self
    }

    /// Switches to `mode`, leaving the agent that ran in its place. The
    /// next turn runs in it; one running now keeps its mode.
    pub(super) fn set_mode(&mut self, mode: Mode) {
        self.selection.switch_mode(mode);
    }

    /// Tab: back to the mode an agent ran in place of, else to the other
    /// mode.
    pub(super) fn toggle_mode(&mut self) {
        self.selection.toggle_mode();
    }

    /// Runs later turns as the primary agent `name`. Its own model is
    /// qualified, as its frontmatter may name it bare and the prompt and
    /// picker show listed ids.
    pub(super) fn set_agent(&mut self, name: String) {
        let own = self
            .context
            .agents
            .primary(&name)
            .and_then(|agent| agent.model.as_deref())
            .map(|model| self.provider.qualify(model));
        self.selection.set_agent(name, own);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::{future::BoxFuture, stream::BoxStream};
    use nth_config::{Slots, Target};
    use nth_protocol::{BoxError, Effort, Listing, Provider, Request, StreamEvent};

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, prompt_row, rows, status_row},
    };

    fn llm(model: &str, effort: Effort) -> Llm {
        Llm {
            model: model.into(),
            effort,
        }
    }

    /// Plan on kimi at high effort, act on glm.
    fn kimi_and_glm() -> App {
        app().with_selection(Selection::new(Slots {
            plan: llm("kimi", Effort::High),
            act: llm("glm", Effort::Default),
            subagent: None,
            clerk: None,
        }))
    }

    #[test]
    fn tab_switches_the_mode_and_its_model() {
        let mut app = kimi_and_glm();
        assert_eq!(app.selection.mode, Mode::Act, "a bare session acts");
        assert_eq!(prompt_row(&rows(&mut app), 0).trim_end(), " ▎ act  · glm");

        app.apply(Action::NextTab);
        assert_eq!(app.selection.mode, Mode::Plan);
        assert_eq!(app.selection.current(), llm("kimi", Effort::High));
        let shown = rows(&mut app);
        assert_eq!(prompt_row(&shown, 0).trim_end(), " ▎ plan · kimi · high");
        assert_eq!(status_row(&shown, 0).trim_end(), " /repo 0m");
        assert_eq!(
            prompt_row(&shown, 0).find("· kimi"),
            " ▎ act  · glm".find("· glm"),
            "the model keeps its column across modes"
        );

        app.selection.set_current(llm("kimi", Effort::Low));
        app.apply(Action::PrevTab);
        assert_eq!(app.selection.current(), llm("glm", Effort::Default));
        app.apply(Action::NextTab);
        assert_eq!(
            app.selection.current().effort,
            Effort::Low,
            "the pick stays with its mode"
        );
    }

    #[test]
    fn the_status_bar_reads_the_selection() {
        let mut app = kimi_and_glm();
        assert_eq!(prompt_row(&rows(&mut app), 0).trim_end(), " ▎ act  · glm");

        app.selection.set_current(llm("qwen", Effort::Low));

        assert_eq!(
            prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ act  · qwen · low"
        );
    }

    #[test]
    fn an_agent_keeps_its_own_llm_apart_from_the_modes() {
        let mut app = kimi_and_glm();
        app.set_mode(Mode::Plan);

        app.set_agent("review".into());
        assert_eq!(
            app.selection.current(),
            llm("glm", Effort::Default),
            "act's to start"
        );
        app.selection.set(Target::Agent, llm("qwen", Effort::Low));
        assert_eq!(app.selection.current().model, "qwen");
        assert_eq!(
            app.selection.llm(Target::Act),
            llm("glm", Effort::Default),
            "act keeps its own"
        );

        app.apply(Action::NextTab);
        assert_eq!(app.runs_as(), (Mode::Plan, None, llm("kimi", Effort::High)));
        app.set_agent("review".into());
        assert_eq!(
            app.selection.current().model,
            "qwen",
            "the agent's pick holds for the run"
        );
        assert_eq!(app.runs_as().1.as_deref(), Some("review"));
    }

    /// A provider that names every model as Zen's, as `Providers` does a
    /// bare id when Zen is the default.
    struct Zen;

    impl Provider for Zen {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            Box::pin(async { Ok(Listing::default()) })
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            Box::pin(async { Err("unused".into()) })
        }

        fn qualify(&self, model: &str) -> String {
            format!("zen/{model}")
        }
    }

    #[test]
    fn an_agents_model_is_qualified_by_the_provider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents = dir.path().join(".nth/agents");
        std::fs::create_dir_all(&agents).expect("dir");
        std::fs::write(
            agents.join("review.md"),
            "---\ndescription: Reviews\nmode: primary\nmodel: glm\n---\nYou review.\n",
        )
        .expect("writes");
        let session = nth_session::Session::new("zen/kimi", "/repo".into());
        let mut app = App::new(crate::app::tests::runtime(session, Arc::new(Zen)));
        app.context = Arc::new(nth_context::Context::discover(
            dir.path(),
            &nth_context::Paths::default(),
        ));

        app.set_agent("review".into());

        assert_eq!(
            app.selection.current().model,
            "zen/glm",
            "named as the picker and the listing name it"
        );
    }
}
