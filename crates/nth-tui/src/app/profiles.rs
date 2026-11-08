//! Profiles: named presets of what plan, act, subagents and the clerk run
//! with. ctrl+p cycles through them; a model picked by hand in the model
//! picker leaves the profile, and the next ctrl+p puts it back. Which
//! profile comes next is `Selection::next_profile`'s; this says it.

use nth_auth::ChatGpt;
use nth_config::Cycled;

use super::App;

impl App {
    /// ctrl+p: to the next profile that can run, saying on the status bar
    /// where it went and what it skipped.
    pub(super) fn next_profile(&mut self) {
        let connected = self.chatgpt.as_ref().is_some_and(ChatGpt::connected);
        let hint = match self.selection.next_profile(connected) {
            Cycled::Applied { name, skipped } => {
                // Its models may be of a provider that could not list,
                // whose limits only another listing brings.
                if self.llms_partial() {
                    self.list_llms();
                }
                match skipped.is_empty() {
                    true => format!("Profile set to {name}"),
                    false => format!("Profile set to {name} · skipped {}", skipped.join(", ")),
                }
            }
            Cycled::NoProfiles => "no profiles configured".into(),
            Cycled::Stayed(skipped) if skipped.is_empty() => "only one profile".into(),
            Cycled::Stayed(skipped) => {
                format!("no other profile can run: {}", skipped.join(", "))
            }
        };
        self.hint = Some(hint.into());
    }
}

#[cfg(test)]
mod tests {
    use nth_config::{Profile, Selection, Slots};
    use nth_protocol::{Effort, Llm, Mode};

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, rows},
    };

    fn llm(model: &str) -> Llm {
        Llm {
            model: model.into(),
            effort: Effort::Default,
        }
    }

    fn profile(name: &str, model: &str) -> Profile {
        Profile {
            name: name.into(),
            slots: Slots {
                plan: llm(&format!("{model}-plan")),
                act: llm(&format!("{model}-act")),
                subagent: Some(llm(&format!("{model}-subagent"))),
                clerk: Some(llm(&format!("{model}-clerk"))),
            },
            unavailable: None,
            needs_chatgpt: false,
        }
    }

    /// An app whose chat started on `profiles[0]`, as `chat.rs` starts one.
    fn profiled_with(profiles: Vec<Profile>) -> App {
        app().with_selection(Selection::same(llm("unused")).with_profiles(profiles))
    }

    fn profiled() -> App {
        profiled_with(vec![profile("go", "go"), profile("zen", "zen")])
    }

    #[test]
    fn cycling_puts_both_modes_on_the_profile() {
        let mut app = profiled();
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ act  · go-act · go"
        );

        app.apply(Action::NextProfile);
        assert_eq!(app.selection.current().model, "zen-act");
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ act  · zen-act · zen"
        );

        app.apply(Action::NextTab);
        assert_eq!(app.selection.mode, Mode::Plan);
        assert_eq!(app.selection.current().model, "zen-plan");
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ plan · zen-plan · zen"
        );

        app.apply(Action::NextProfile);
        assert_eq!(
            app.selection.current().model,
            "go-plan",
            "wraps to the first"
        );
        assert_eq!(app.hint.as_deref(), Some("Profile set to go"));
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ plan · go-plan · go"
        );
    }

    #[tokio::test]
    async fn cycling_lists_again_while_a_provider_is_missing() {
        let mut app = profiled();
        app.llms = Some(Default::default());
        app.apply(Action::NextProfile);
        assert!(!app.llm_listing.is_running(), "listed in full");

        app.llms = Some(nth_protocol::Listing {
            models: Vec::new(),
            failed: vec![nth_protocol::Failed {
                origin: nth_protocol::Origin {
                    id: "chatgpt".into(),
                    name: "ChatGPT".into(),
                },
                error: "401: expired".into(),
            }],
        });
        app.apply(Action::NextProfile);
        assert!(app.llm_listing.is_running(), "asked again");
        app.llm_listing.take().expect("listing").abort();
    }

    #[test]
    fn a_profile_that_cannot_run_is_skipped_and_said() {
        let mut zen = profile("zen", "zen");
        zen.unavailable = Some("ZEN_API_KEY is not set".into());
        let mut app = profiled_with(vec![profile("go", "go"), zen, profile("lyceum", "ly")]);

        app.apply(Action::NextProfile);
        assert_eq!(app.selection.current().model, "ly-act");
        assert_eq!(
            app.hint.as_deref(),
            Some("Profile set to lyceum · skipped zen (ZEN_API_KEY is not set)")
        );
    }

    #[test]
    fn nothing_else_to_cycle_to_is_said() {
        let mut zen = profile("zen", "zen");
        zen.unavailable = Some("ZEN_API_KEY is not set".into());
        let mut app = profiled_with(vec![profile("go", "go"), zen]);

        app.apply(Action::NextProfile);
        assert_eq!(app.selection.current().model, "go-act", "stays where it is");
        assert_eq!(
            app.hint.as_deref(),
            Some("no other profile can run: zen (ZEN_API_KEY is not set)")
        );
    }

    #[tokio::test]
    async fn a_chatgpt_profile_runs_once_connected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = nth_auth::Store::at(dir.path().join("auth.json"));
        let mut codex = profile("codex", "chatgpt/gpt-5.5");
        codex.needs_chatgpt = true;
        let mut app = profiled_with(vec![profile("go", "go"), codex])
            .with_chatgpt(ChatGpt::new(store.clone()).expect("client"));

        app.apply(Action::NextProfile);
        assert_eq!(app.selection.current().model, "go-act");
        assert_eq!(
            app.hint.as_deref(),
            Some("no other profile can run: codex (ChatGPT is not connected)")
        );

        // As `/connect` leaves it.
        let oauth = nth_auth::OAuth {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: u64::MAX,
            account_id: None,
            client_id: Some("oaiapp_issued".into()),
            id_token: None,
            scopes: Vec::new(),
        };
        store.save(ChatGpt::ID, &oauth).await.expect("saves");
        app.apply(Action::NextProfile);
        assert_eq!(app.selection.current().model, "chatgpt/gpt-5.5-act");
    }

    #[test]
    fn no_profiles_is_said_not_cycled() {
        let mut app = app();
        app.apply(Action::NextProfile);

        assert_eq!(app.hint.as_deref(), Some("no profiles configured"));
        assert_eq!(app.selection.current().model, "glm");
    }
}
