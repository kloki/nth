//! The LLMs the endpoint serves: listed for the context window on the
//! status bar, and picked from in the model picker.

use std::path::Path;

use nth_protocol::{BoxError, Cost, Effort, Listing};
use nth_session::Price;

use super::{App, Llm, input::Input, keys::Action};
use crate::llm_picker::{LlmPicker, Target};

impl App {
    /// A key while the model picker is open.
    pub(super) fn key_llm_picker(&mut self, action: Action) {
        let Input::LlmPicker(picker) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext => picker.next(),
            Action::SelectPrev => picker.prev(),
            Action::Right => picker.more(),
            Action::Left => picker.less(),
            Action::NextTab => self.cycle_llm_target(false),
            Action::PrevTab => self.cycle_llm_target(true),
            Action::Insert(c) => picker.insert(c),
            Action::Backspace => picker.backspace(),
            Action::Submit => self.choose_llm(),
            // As on the prompt, ctrl+c clears the query before closing.
            Action::ClearOrQuit if picker.clear_query() => {}
            // Closing the picker must not also interrupt a running turn.
            Action::Interrupt | Action::ClearOrQuit | Action::LlmPicker => {
                self.input = Input::Prompt
            }
            _ => {}
        }
    }

    /// Opens the picker on what the next turn runs with, listing LLMs the
    /// first time, and again while a provider is missing from the list.
    pub(super) fn open_llm_picker(&mut self) {
        self.completion = None;
        let target = match self.agent {
            Some(_) => Target::Agent,
            None => Target::of(self.mode),
        };
        let mut picker = LlmPicker::new(target, &self.model, self.effort);
        // The answer fills this picker when it comes, over what it shows.
        if let Some(llms) = &self.llms {
            picker.load(Ok(llms.clone()));
        }
        self.list_llms();
        self.input = Input::LlmPicker(picker);
    }

    /// Asks for the LLMs unless they are listed in full or already asked
    /// for. Done at start-up too, since the status bar needs the context
    /// window.
    pub(super) fn list_llms(&mut self) {
        let listed = self.llms.as_ref().is_some_and(|l| l.failed.is_empty());
        if !listed && !self.llm_listing.is_running() {
            let provider = self.provider.clone();
            self.llm_listing
                .start(|_| tokio::spawn(async move { provider.models().await }));
        }
    }

    /// The context window of the model in use, when the provider says.
    pub fn context_window(&self) -> Option<u64> {
        self.context_window_of(&self.model)
    }

    /// The context window and output limit of `model`; `None` until the
    /// listing names it, so a caller keeps what it knew before.
    pub fn limits_of(&self, model: &str) -> Option<(Option<u64>, Option<u64>)> {
        let llm = self
            .llms
            .as_ref()?
            .models
            .iter()
            .find(|llm| llm.id == model)?;
        Some((llm.context, llm.output))
    }

    /// Whether `model` takes images, when the listing names it.
    pub fn takes_images_of(&self, model: &str) -> Option<bool> {
        let llms = self.llms.as_ref()?;
        llms.models.iter().find(|llm| llm.id == model)?.takes_images
    }

    /// The context window of `model`, when the provider says.
    pub fn context_window_of(&self, model: &str) -> Option<u64> {
        let llms = self.llms.as_ref()?;
        llms.models.iter().find(|llm| llm.id == model)?.context
    }

    /// What `model` costs, when the catalogue says.
    pub fn cost_of(&self, model: &str) -> Option<Cost> {
        self.llms.as_ref()?.cost_of(model)
    }

    /// What the session spent so far, at the catalogue's prices.
    pub fn price(&self) -> Option<Price> {
        self.spent.ledger.price(|model| self.cost_of(model))
    }

    /// Whether a provider could not list, so its models are missing.
    pub(super) fn llms_partial(&self) -> bool {
        self.llms.as_ref().is_some_and(|l| !l.failed.is_empty())
    }

    /// Only a list is kept, even one a provider is missing from, since the
    /// others' models are good; after any failure the next open asks again.
    pub(super) fn llms_listed(&mut self, llms: Result<Listing, BoxError>) {
        let llms = llms.map_err(|e| e.to_string());
        // Asked again over a partial list, a failure leaves the picker on
        // what it shows rather than on nothing.
        let shown = self.llms.is_some();
        if let Ok(llms) = &llms {
            self.llms = Some(llms.clone());
        }
        if let Input::LlmPicker(picker) = &mut self.input
            && (llms.is_ok() || !shown)
        {
            picker.load(llms);
        }
    }

    /// Writes the counts in the background, as the prompt history is.
    pub(super) fn save_llm_usage(&mut self) {
        let Some(path) = self.llm_usage.saved_at().map(Path::to_path_buf) else {
            return;
        };
        let json = self.llm_usage.to_json();
        self.llm_usage_saving.start_or_queue(|_| {
            tokio::spawn(async move {
                if let Some(dir) = path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::write(&path, json).await
            })
        });
    }

    /// A failed save is told once; the counts stay in memory for this run.
    pub(super) fn llm_usage_saved(&mut self, saved: std::io::Result<()>) {
        if self.llm_usage_saving.take_again() {
            self.save_llm_usage();
        }
        if let Err(e) = saved {
            let path = self.llm_usage.saved_at().map(|p| p.display().to_string());
            self.chat.transcript.push_error(format!(
                "model usage not saved to {}: {e}",
                path.unwrap_or_default()
            ));
            self.llm_usage.forget_path();
        }
    }

    /// Tab in the picker: the pick goes to the next target (`back`: the
    /// previous), marking what that runs on.
    pub(super) fn cycle_llm_target(&mut self, back: bool) {
        let Input::LlmPicker(picker) = &self.input else {
            return;
        };
        let agent = picker.offers_agent();
        let target = match back {
            false => picker.target().next(agent),
            true => picker.target().prev(agent),
        };
        let llm = self.llm_of(target);
        if let Input::LlmPicker(picker) = &mut self.input {
            picker.retarget(target, &llm.model, llm.effort);
        }
    }

    /// Switches the picker's target to the highlighted model for later
    /// turns; the session picks it up when the next turn starts, since
    /// mid-turn it is in the turn task. The picker stays open, marking the
    /// pick, so Tab can move on to another target; Esc closes it.
    pub(super) fn choose_llm(&mut self) {
        let Input::LlmPicker(picker) = &mut self.input else {
            return;
        };
        if let Some((model, effort)) = picker.chosen() {
            let target = picker.target();
            picker.mark(&model, effort);
            self.hint = Some(set_hint(target, &model, effort).into());
            self.set_llm(target, Llm { model, effort });
        }
    }
}

/// Says what the pick went to, since a target other than the mode in use
/// changes nothing the status bar shows.
fn set_hint(target: Target, model: &str, effort: Effort) -> String {
    let what = match target {
        Target::Plan => "Plan",
        Target::Act => "Act",
        Target::Agent => "The agent",
        Target::Subagent => "Subagents",
        Target::Clerk => "Clerk",
    };
    match effort {
        Effort::Default => format!("{what} set to {model}"),
        effort => format!("{what} set to {model} · {}", effort.name()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{Effort, Failed, Mode, Origin, Provider, Request, StreamEvent};
    use nth_session::Session;

    use super::*;
    use crate::{
        app::{
            keys::Action,
            tests::{app, rows},
        },
        llm_picker::{tests::model, usage::LlmUsage},
    };

    /// An idle app whose model list is already in, so opening the picker
    /// asks the provider for nothing.
    fn llm_listed_app() -> App {
        let mut app = app();
        app.llms = Some(vec![model("glm", true), model("plain", false)].into());
        app
    }

    fn picker(app: &App) -> &LlmPicker {
        match &app.input {
            Input::LlmPicker(picker) => picker,
            _ => panic!("picker not open"),
        }
    }

    #[test]
    fn the_picker_draws_in_place_of_the_prompt() {
        let mut app = llm_listed_app();
        app.prompt.insert_str("/models");
        app.submit();
        let rows = rows(&mut app);

        assert!(rows[5].starts_with(" ▎ switch model"), "{:?}", rows[5]);
        assert!(rows[6].starts_with(" ▎ > "), "{:?}", rows[6]);
        assert!(rows[6].trim_end().ends_with("2/2"), "{:?}", rows[6]);
        assert!(rows[7].starts_with(" ▎ → glm   ✓"), "{:?}", rows[7]);
        assert!(rows[7].trim_end().ends_with("◂ default ▸"));
        assert!(rows[8].starts_with(" ▎   plain"));
        assert!(
            rows[9..13].iter().all(|r| r.trim().is_empty()),
            "six model rows"
        );
        assert!(
            rows.iter().all(|r| !r.contains("Ask anything")),
            "no prompt"
        );
        assert_eq!(
            crate::app::tests::status_row(&rows, 0).trim_end(),
            " /repo 0m",
            "status stays"
        );
    }

    #[test]
    fn the_picker_switches_model_and_effort() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        app.apply(Action::Right);
        app.apply(Action::Right);
        app.apply(Action::Submit);
        assert!(
            matches!(app.input, Input::LlmPicker(_)),
            "enter keeps it open"
        );
        app.apply(Action::Interrupt);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!((app.model.as_str(), app.effort), ("glm", Effort::Medium));
        let shown = rows(&mut app);
        assert_eq!(
            crate::app::tests::prompt_row(&shown, 0).trim_end(),
            " ▎ act  · glm · medium"
        );
        assert_eq!(
            crate::app::tests::status_row(&shown, 0).trim_end(),
            " /repo 0m"
        );

        app.apply(Action::LlmPicker);
        app.apply(Action::SelectNext);
        app.apply(Action::Submit);
        assert_eq!((app.model.as_str(), app.effort), ("plain", Effort::Default));
    }

    #[test]
    fn typing_filters_the_picker_and_ctrl_c_clears_before_closing() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        for c in "pla".chars() {
            app.apply(Action::Insert(c));
        }
        let rows = rows(&mut app);
        assert!(rows[6].starts_with(" ▎ > pla"), "{:?}", rows[6]);
        assert!(rows[6].trim_end().ends_with("1/2"), "{:?}", rows[6]);
        assert!(rows[7].starts_with(" ▎ → plain"), "{:?}", rows[7]);
        assert!(rows[8].trim().is_empty(), "glm filtered out");

        app.apply(Action::ClearOrQuit);
        assert!(matches!(app.input, Input::LlmPicker(_)), "query cleared");
        assert_eq!(picker(&app).chosen().map(|c| c.0), Some("glm".into()));

        app.apply(Action::Insert('p'));
        app.apply(Action::Submit);
        assert_eq!(app.model, "plain");
        assert!(matches!(app.input, Input::LlmPicker(_)));
        let shown = crate::app::tests::rows(&mut app);
        assert!(
            shown[6].starts_with(" ▎ > p"),
            "the query stays: {:?}",
            shown[6]
        );
        assert!(
            shown[7].starts_with(" ▎ → plain ✓"),
            "marked: {:?}",
            shown[7]
        );
    }

    #[tokio::test]
    async fn a_turn_counts_for_its_model_and_is_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nth/llm-usage.json");
        let mut app = llm_listed_app().with_llm_usage(LlmUsage::load(path.clone()).await);

        app.prompt.insert_str("go");
        app.submit();
        let saved = app.llm_usage_saving.join().await.expect("save finished");
        app.llm_usage_saved(saved);
        app.turn.join().await.expect("turn task finished");

        assert_eq!(LlmUsage::load(path).await.ranked(), [("glm", 1)]);
    }

    #[test]
    fn esc_leaves_the_picker_unchanged() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        app.apply(Action::SelectNext);
        app.apply(Action::Interrupt);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.model, "glm");
        assert!(!app.quit);
    }

    /// Before the listing arrives — a resumed session's first prompt — the
    /// session keeps the limits it saved, so its compaction still fires.
    #[tokio::test]
    async fn a_turn_before_the_listing_keeps_the_sessions_limits() {
        let mut app = app();
        app.session
            .as_mut()
            .expect("idle")
            .set_limits(Some(10_000), Some(2_000));

        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("session came back");
        assert_eq!(
            (session.context_window, session.max_output),
            (Some(10_000), Some(2_000))
        );
    }

    /// Another model's window would size the trigger wrongly: a switch to a
    /// model the listing does not name leaves the limits unknown.
    #[tokio::test]
    async fn switching_to_an_unlisted_model_drops_the_old_limits() {
        let mut app = app();
        app.session
            .as_mut()
            .expect("idle")
            .set_limits(Some(10_000), Some(2_000));
        app.model = "unlisted".into();

        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("session came back");
        assert_eq!((session.context_window, session.max_output), (None, None));
    }

    #[tokio::test]
    async fn a_turn_puts_the_listed_limits_on_the_session() {
        let mut app = app();
        let mut glm = model("glm", true);
        glm.context = Some(200_000);
        glm.output = Some(64_000);
        app.llms = Some(vec![glm].into());

        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.max_output, Some(64_000));
        assert_eq!(app.limits_of("glm"), Some((Some(200_000), Some(64_000))));
        assert_eq!(app.limits_of("unlisted"), None);
    }

    #[tokio::test]
    async fn a_turn_without_a_listing_leaves_the_limits_unknown() {
        let mut app = app();

        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.context_window, None);
        assert_eq!(session.max_output, None, "nothing auto-compacts");
    }

    #[tokio::test]
    async fn the_next_turn_runs_the_chosen_model() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        app.apply(Action::Right);
        app.apply(Action::Submit);

        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("idle");
        assert_eq!(
            (session.model.as_str(), session.effort),
            ("glm", Effort::Low)
        );
    }

    #[test]
    fn tab_in_the_picker_picks_what_the_choice_is_for() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        assert_eq!(picker(&app).target(), Target::Act, "the mode in use");
        assert!(
            rows(&mut app)[5].starts_with(" ▎ switch model  plan · act · subagent"),
            "{:?}",
            rows(&mut app)[5]
        );

        app.apply(Action::NextTab);
        assert_eq!(picker(&app).target(), Target::Subagent);
        assert_eq!(
            picker(&app).chosen().map(|c| c.0),
            Some("glm".into()),
            "subagents run on the mode's until picked"
        );
        app.apply(Action::SelectNext);
        app.apply(Action::Submit);
        assert_eq!(app.model, "glm", "the mode keeps its model");
        assert_eq!(app.subagent_llm().model, "plain");
        assert_eq!(app.hint.as_deref(), Some("Subagents set to plain"));
        assert_eq!(app.mode, Mode::Act, "Tab in the picker leaves the mode be");

        app.apply(Action::NextTab);
        assert_eq!(
            picker(&app).target(),
            Target::Clerk,
            "open for the next pick"
        );
        assert_eq!(
            picker(&app).chosen().map(|c| c.0),
            Some("plain".into()),
            "the clerk runs on the subagents' just picked"
        );
        app.apply(Action::Interrupt);

        app.apply(Action::LlmPicker);
        app.apply(Action::PrevTab);
        assert_eq!(picker(&app).target(), Target::Plan);
        app.apply(Action::SelectNext);
        app.apply(Action::Submit);
        assert_eq!(app.llm(Mode::Plan).model, "plain");
        assert_eq!(app.model, "glm", "act is untouched");
        assert_eq!(app.hint.as_deref(), Some("Plan set to plain"));

        app.apply(Action::Interrupt);
        app.apply(Action::LlmPicker);
        app.apply(Action::NextTab);
        app.apply(Action::NextTab);
        app.apply(Action::NextTab);
        assert_eq!(picker(&app).target(), Target::Plan, "wraps past clerk");
        app.apply(Action::PrevTab);
        assert_eq!(picker(&app).target(), Target::Clerk);
        assert_eq!(
            picker(&app).chosen().map(|c| c.0),
            Some("plain".into()),
            "the subagents' until picked"
        );
        app.apply(Action::SelectPrev);
        app.apply(Action::Right);
        app.apply(Action::Submit);
        let low = Llm {
            model: "glm".into(),
            effort: Effort::Low,
        };
        assert_eq!(app.llm_of(Target::Clerk), low);
        assert_eq!(app.llm_of(Target::Subagent).model, "plain", "untouched");
        assert_eq!(app.hint.as_deref(), Some("Clerk set to glm · low"));
    }

    #[tokio::test]
    async fn a_turn_hands_the_subagent_llm_to_the_session() {
        let mut app = llm_listed_app();
        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);
        let session = app.session.as_ref().expect("idle");
        assert_eq!(session.subagent_llm, None, "the parent's unless picked");

        app.set_llm(
            Target::Subagent,
            Llm {
                model: "plain".into(),
                effort: Effort::Low,
            },
        );
        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);
        let session = app.session.as_ref().expect("idle");
        let subagent = session.subagent_llm.as_ref().expect("picked");
        assert_eq!(
            (subagent.llm.model.as_str(), subagent.llm.effort),
            ("plain", Effort::Low)
        );
    }

    #[tokio::test]
    async fn the_list_loads_into_the_open_picker() {
        let mut app = app();
        app.apply(Action::LlmPicker);
        assert!(picker(&app).chosen().is_none(), "still loading");

        let listing = app.llm_listing.take().expect("listing");
        app.llms_listed(Ok(vec![model("glm", true)].into()));
        listing.abort();

        assert_eq!(picker(&app).chosen(), Some(("glm".into(), Effort::Default)));
        assert!(app.llms.is_some());
    }

    #[tokio::test]
    async fn a_provider_that_could_not_list_shows_why_after_the_models() {
        let mut app = app();
        app.apply(Action::LlmPicker);
        let listing = app.llm_listing.take().expect("listing");
        listing.abort();
        app.llms_listed(Ok(Listing {
            models: vec![model("glm", true)],
            failed: vec![Failed {
                origin: Origin {
                    id: "lyceum".into(),
                    name: "Lyceum".into(),
                },
                error: "401: bad key".into(),
            }],
        }));
        let rows = rows(&mut app);

        assert!(rows[7].starts_with(" ▎ → glm"), "{:?}", rows[7]);
        assert!(rows[8].contains("✗ Lyceum: 401: bad key"), "{:?}", rows[8]);
    }

    #[tokio::test]
    async fn a_list_a_provider_is_missing_from_is_asked_for_again() {
        let mut app = app();
        app.llms = Some(Listing {
            models: vec![model("glm", true)],
            failed: vec![Failed {
                origin: Origin {
                    id: "chatgpt".into(),
                    name: "ChatGPT".into(),
                },
                error: "401: expired".into(),
            }],
        });

        app.apply(Action::LlmPicker);

        assert!(app.llm_listing.is_running(), "asked again");
        assert_eq!(
            picker(&app).chosen(),
            Some(("glm".into(), Effort::Default)),
            "what it has shows meanwhile"
        );
        app.llm_listing.take().expect("listing").abort();

        app.llms_listed(Err("offline".into()));
        assert!(picker(&app).chosen().is_some(), "still shows glm");
    }

    #[tokio::test]
    async fn a_full_list_is_not_asked_for_again() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        assert!(!app.llm_listing.is_running());
    }

    #[tokio::test]
    async fn a_failed_list_is_asked_for_again() {
        let mut app = app();
        app.apply(Action::LlmPicker);
        let listing = app.llm_listing.take().expect("listing");
        listing.abort();
        app.llms_listed(Err("offline".into()));
        assert!(rows(&mut app)[6].contains("✗ offline"));

        app.apply(Action::Interrupt);
        app.apply(Action::LlmPicker);
        assert!(app.llm_listing.is_running(), "asked again");
    }

    /// A provider whose model list never arrives, and records when the
    /// request for it is dropped.
    struct SlowList(Arc<AtomicBool>);

    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl Provider for SlowList {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            let guard = SetOnDrop(self.0.clone());
            async move {
                let _guard = guard;
                std::future::pending().await
            }
            .boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_listing() {
        let dropped = Arc::new(AtomicBool::new(false));
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(
            session,
            Arc::new(SlowList(dropped.clone())),
            Arc::new(Vec::new()),
        );
        app.apply(Action::LlmPicker);
        tokio::task::yield_now().await;

        drop(app);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(dropped.load(Ordering::SeqCst));
    }
}
