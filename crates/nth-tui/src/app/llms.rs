//! The LLMs the endpoint serves: listed for the context window on the
//! status bar, and picked from in the model picker.

use std::path::Path;

use nth_protocol::{BoxError, Cost, Listing};
use nth_session::Price;

use super::{App, input::Input};
use crate::llm_picker::LlmPicker;

impl App {
    /// Opens the picker on the LLM in use, listing LLMs the first time.
    pub(super) fn open_llm_picker(&mut self) {
        self.completion = None;
        let mut picker = LlmPicker::new(&self.model, self.effort);
        match &self.llms {
            Some(llms) => picker.load(Ok(llms.clone())),
            // The answer fills this picker when it comes.
            None => self.list_llms(),
        }
        self.input = Input::LlmPicker(picker);
    }

    /// Asks for the LLMs unless they are listed or already asked for. Done
    /// at start-up too, since the status bar needs the context window.
    pub(super) fn list_llms(&mut self) {
        if self.llms.is_none() && !self.llm_listing.is_running() {
            let provider = self.provider.clone();
            self.llm_listing
                .start(|_| tokio::spawn(async move { provider.models().await }));
        }
    }

    /// The context window of the model in use, when the provider says.
    pub fn context_window(&self) -> Option<u64> {
        self.context_window_of(&self.model)
    }

    /// The context window of `model`, when the provider says.
    pub fn context_window_of(&self, model: &str) -> Option<u64> {
        let llms = self.llms.as_ref()?;
        llms.models.iter().find(|llm| llm.id == model)?.context
    }

    /// What `model` costs, when the catalogue says.
    pub fn cost_of(&self, model: &str) -> Option<Cost> {
        let llms = self.llms.as_ref()?;
        llms.models.iter().find(|llm| llm.id == model)?.cost
    }

    /// What the session spent so far, at the catalogue's prices.
    pub fn price(&self) -> Option<Price> {
        self.spent.ledger.price(|model| self.cost_of(model))
    }

    /// Only a list is kept; after a failure the next open asks again.
    pub(super) fn llms_listed(&mut self, llms: Result<Listing, BoxError>) {
        let llms = llms.map_err(|e| e.to_string());
        if let Ok(llms) = &llms {
            self.llms = Some(llms.clone());
        }
        if let Input::LlmPicker(picker) = &mut self.input {
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

    /// Switches later turns to the highlighted model; the session picks it
    /// up when the next turn starts, since mid-turn it is in the turn task.
    pub(super) fn choose_llm(&mut self) {
        let Input::LlmPicker(picker) = &self.input else {
            return;
        };
        if let Some((model, effort)) = picker.chosen() {
            self.model = model;
            self.effort = effort;
            self.input = Input::Prompt;
        }
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
    use nth_protocol::{Effort, Failed, Origin, Provider, Request, StreamEvent};
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
        assert_eq!(rows[14].trim_end(), " glm · /repo", "status stays");
    }

    #[test]
    fn the_picker_switches_model_and_effort() {
        let mut app = llm_listed_app();
        app.apply(Action::LlmPicker);
        app.apply(Action::Right);
        app.apply(Action::Right);
        app.apply(Action::Submit);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!((app.model.as_str(), app.effort), ("glm", Effort::Medium));
        assert_eq!(rows(&mut app)[14].trim_end(), " glm · medium · /repo");

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
        assert!(matches!(app.input, Input::Prompt));
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
