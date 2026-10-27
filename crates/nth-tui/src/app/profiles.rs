//! Profiles: named presets of what plan, act and subagents run with. ctrl+p cycles
//! through them; a model picked by hand in the model picker leaves the
//! profile, and the next ctrl+p puts it back.

use nth_auth::ChatGpt;
use nth_protocol::Mode;

use super::{App, Llm, ModeLlms};

/// A named preset, from a `[[profiles]]` entry in the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// What the prompt's label and the cycling hint call it.
    pub name: String,
    /// What plan, act and subagents run with on this profile.
    pub llms: ModeLlms,
    /// Why it cannot run, when its provider has no key set: cycling skips
    /// it and says this. `None` means it runs.
    pub unavailable: Option<String>,
    /// Runs a `chatgpt/` model, so it runs only while ChatGPT is
    /// connected; asked on every cycle, so a `/connect` mid-run counts.
    pub needs_chatgpt: bool,
}

/// What `profile` runs with in `mode`.
fn llm(profile: &Profile, mode: Mode) -> &Llm {
    match mode {
        Mode::Plan => &profile.llms.plan,
        Mode::Act => &profile.llms.act,
    }
}

impl App {
    /// The profiles ctrl+p cycles, in config order; the first is the one
    /// the chat started on. Empty when the config lists none.
    pub fn with_profiles(mut self, profiles: Vec<Profile>) -> Self {
        self.profile = (!profiles.is_empty()).then_some(0);
        self.profiles = profiles;
        self
    }

    /// The active profile's name, while both modes still run its llms. A
    /// model picked by hand — or a resumed session's own — leaves the
    /// profile for both, and ctrl+p puts it, name and all, back.
    pub(super) fn profile_name(&self) -> Option<&str> {
        let profile = self.profiles.get(self.profile?)?;
        self.runs(profile).then_some(profile.name.as_str())
    }

    /// Whether both modes, the subagents and the clerk still run `profile`'s llms.
    fn runs(&self, profile: &Profile) -> bool {
        Mode::ALL
            .iter()
            .all(|&mode| self.llm(mode) == *llm(profile, mode))
            && self.mode_llms.subagent == profile.llms.subagent
            && self.mode_llms.clerk == profile.llms.clerk
    }

    /// ctrl+p: to the next profile, both modes on its llms. A model
    /// picked by hand since the profile was applied means both modes no
    /// longer run it: the first ctrl+p puts the profile back, the next
    /// moves on. Profiles that cannot run are skipped, and one left
    /// behind is said on the status bar.
    pub(super) fn next_profile(&mut self) {
        if self.profiles.is_empty() {
            self.hint = Some("no profiles configured".into());
            return;
        }
        let len = self.profiles.len();
        // Back where the profile was left, before moving on.
        if let Some(i) = self.profile
            && !self.runs(&self.profiles[i])
        {
            self.apply_profile(i);
            return;
        }
        // The other profiles, past the active one; from `None`, which is
        // before the first, the first itself is due.
        let steps = if self.profile.is_some() { len - 1 } else { len };
        let mut skipped = Vec::new();
        for step in 0..steps {
            let i = match self.profile {
                Some(i) => (i + 1 + step) % len,
                None => step,
            };
            match self.why_unavailable(&self.profiles[i]) {
                None => {
                    self.apply_profile(i);
                    if !skipped.is_empty() {
                        self.hint = Some(
                            format!(
                                "Profile set to {} · skipped {}",
                                self.profiles[i].name,
                                skipped.join(", ")
                            )
                            .into(),
                        );
                    }
                    return;
                }
                Some(reason) => {
                    skipped.push(format!("{} ({reason})", self.profiles[i].name));
                }
            }
        }
        self.hint = Some(if skipped.is_empty() {
            "only one profile".into()
        } else {
            format!("no other profile can run: {}", skipped.join(", ")).into()
        });
    }

    /// Why `profile` cannot run now; `None` when it can.
    fn why_unavailable(&self, profile: &Profile) -> Option<String> {
        if let Some(reason) = &profile.unavailable {
            return Some(reason.clone());
        }
        let connected = self.chatgpt.as_ref().is_some_and(ChatGpt::connected);
        (profile.needs_chatgpt && !connected).then(|| "ChatGPT is not connected".into())
    }

    /// Both modes on `profile`'s llms, the current mode's model and
    /// effort with them, and the status bar says so.
    fn apply_profile(&mut self, i: usize) {
        self.set_mode_llms(self.profiles[i].llms.clone());
        self.profile = Some(i);
        self.hint = Some(format!("Profile set to {}", self.profiles[i].name).into());
        // Its models may be of a provider that could not list, whose
        // limits only another listing brings.
        if self.llms_partial() {
            self.list_llms();
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Effort;

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
            llms: ModeLlms {
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
        let first = profiles[0].llms.clone();
        app().with_mode_llms(first).with_profiles(profiles)
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
        assert_eq!(app.model, "zen-act");
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ act  · zen-act · zen"
        );

        app.apply(Action::NextTab);
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(app.model, "zen-plan");
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ plan · zen-plan · zen"
        );

        app.apply(Action::NextProfile);
        assert_eq!(app.model, "go-plan", "wraps to the first");
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
    fn a_picked_model_leaves_the_profile_and_ctrl_p_puts_it_back() {
        let mut app = profiled();
        app.model = "picked".into();

        assert_eq!(app.profile_name(), None, "the profile is left");
        app.apply(Action::NextTab);
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(app.profile_name(), None, "left for plan too");
        app.apply(Action::NextTab);

        app.apply(Action::NextProfile);
        assert_eq!(app.model, "go-act", "the same profile comes back");
        assert_eq!(app.profile_name(), Some("go"));
        assert_eq!(app.hint.as_deref(), Some("Profile set to go"));

        app.apply(Action::NextTab);
        assert_eq!(
            (app.mode, app.model.as_str()),
            (Mode::Plan, "go-plan"),
            "both modes are the profile's again"
        );
        assert_eq!(app.profile_name(), Some("go"));
        app.apply(Action::NextProfile);
        assert_eq!(app.model, "zen-plan", "then cycling moves on");
    }

    #[test]
    fn a_picked_subagent_model_leaves_the_profile() {
        let mut app = profiled();
        assert_eq!(app.subagent_llm().model, "go-subagent");
        app.set_llm(crate::llm_picker::Target::Subagent, llm("picked"));
        assert_eq!(app.profile_name(), None);

        app.apply(Action::NextProfile);
        assert_eq!(app.subagent_llm().model, "go-subagent", "put back");
        assert_eq!(app.profile_name(), Some("go"));
    }

    #[test]
    fn a_picked_clerk_model_leaves_the_profile() {
        let mut app = profiled();
        assert_eq!(app.clerk_llm().model, "go-clerk");
        app.set_llm(crate::llm_picker::Target::Clerk, llm("picked"));
        assert_eq!(app.clerk_llm().model, "picked");
        assert_eq!(app.profile_name(), None);

        app.apply(Action::NextProfile);
        assert_eq!(app.clerk_llm().model, "go-clerk", "put back");
        assert_eq!(app.profile_name(), Some("go"));
    }

    #[test]
    fn a_profile_that_cannot_run_is_skipped_and_said() {
        let mut zen = profile("zen", "zen");
        zen.unavailable = Some("ZEN_API_KEY is not set".into());
        let mut app = profiled_with(vec![profile("go", "go"), zen, profile("lyceum", "ly")]);

        app.apply(Action::NextProfile);
        assert_eq!(app.model, "ly-act");
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
        assert_eq!(app.model, "go-act", "stays where it is");
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
        assert_eq!(app.model, "go-act");
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
        };
        store.save(ChatGpt::ID, &oauth).await.expect("saves");
        app.apply(Action::NextProfile);
        assert_eq!(app.model, "chatgpt/gpt-5.5-act");
    }

    #[test]
    fn no_profiles_is_said_not_cycled() {
        let mut app = app();
        app.apply(Action::NextProfile);

        assert_eq!(app.hint.as_deref(), Some("no profiles configured"));
        assert_eq!(app.model, "glm");
    }
}
