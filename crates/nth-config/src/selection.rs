//! What every kind of turn runs on, and the rules that pick it: plan and
//! act each keep a model and effort, subagents and the clerk may have their
//! own, a primary agent runs on one of its own for the run, and the
//! `[[profiles]]` ctrl+p cycles set them all at once. Plain data, so the
//! chat, `nth run` and the tests read one copy of each rule.

use std::collections::BTreeMap;

use nth_protocol::{Llm, Mode};

use crate::Config;

/// What plan, act, the subagents and the clerk run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    pub plan: Llm,
    pub act: Llm,
    /// `None` runs subagents on what the turn that starts them runs on.
    pub subagent: Option<Llm>,
    /// `None` runs the built-in clerk on the subagents' model.
    pub clerk: Option<Llm>,
}

impl Slots {
    /// Both modes on `llm`, subagents and the clerk on theirs.
    pub fn same(llm: Llm) -> Self {
        Self {
            plan: llm.clone(),
            act: llm,
            subagent: None,
            clerk: None,
        }
    }

    pub fn mode(&self, mode: Mode) -> &Llm {
        match mode {
            Mode::Plan => &self.plan,
            Mode::Act => &self.act,
        }
    }

    fn mode_mut(&mut self, mode: Mode) -> &mut Llm {
        match mode {
            Mode::Plan => &mut self.plan,
            Mode::Act => &mut self.act,
        }
    }
}

/// A `[[profiles]]` entry, its ids qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// What the prompt's label and the cycling hint call it.
    pub name: String,
    pub slots: Slots,
    /// Why it cannot run, when its provider has no key set: cycling skips
    /// it and says this. `None` means it runs.
    pub unavailable: Option<String>,
    /// Runs a `chatgpt/` model, so it runs only while ChatGPT is
    /// connected; asked on every cycle, so a `/connect` mid-run counts.
    pub needs_chatgpt: bool,
}

/// What a picked LLM goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Plan,
    Act,
    /// The primary agent running in place of the mode; offered only while
    /// one does.
    Agent,
    Subagent,
    /// The built-in clerk.
    Clerk,
}

impl Target {
    pub const ALL: [Target; 5] = [
        Target::Plan,
        Target::Act,
        Target::Agent,
        Target::Subagent,
        Target::Clerk,
    ];

    /// The ones offered: the agent's only while one runs.
    pub fn offered(agent: bool) -> Vec<Target> {
        Self::ALL
            .into_iter()
            .filter(|&t| agent || t != Target::Agent)
            .collect()
    }

    /// The one for what `mode` runs with.
    pub fn of(mode: Mode) -> Self {
        match mode {
            Mode::Plan => Target::Plan,
            Mode::Act => Target::Act,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Plan => "plan",
            Target::Act => "act",
            Target::Agent => "agent",
            Target::Subagent => "subagent",
            Target::Clerk => "clerk",
        }
    }

    /// The next one offered, wrapping, as Tab moves.
    pub fn next(self, agent: bool) -> Self {
        let offered = Self::offered(agent);
        let i = offered.iter().position(|&t| t == self).unwrap_or(0);
        offered[(i + 1) % offered.len()]
    }

    /// The previous one offered, wrapping, as shift+Tab moves.
    pub fn prev(self, agent: bool) -> Self {
        let offered = Self::offered(agent);
        let i = offered.iter().position(|&t| t == self).unwrap_or(0);
        offered[(i + offered.len() - 1) % offered.len()]
    }
}

/// What ctrl+p did.
#[derive(Debug, PartialEq, Eq)]
pub enum Cycled {
    /// Every slot is on the profile `name` now, past the ones that cannot
    /// run, each said as `name (reason)`.
    Applied { name: String, skipped: Vec<String> },
    /// The config lists no profiles.
    NoProfiles,
    /// No other profile can run, each said as `name (reason)`; empty when
    /// there is only the one.
    Stayed(Vec<String>),
}

/// The model and effort every kind of turn runs with, and what runs next:
/// the mode, or the primary agent in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The mode the next turn runs in, unless `agent` runs in its place.
    pub mode: Mode,
    /// The primary agent the next turn runs as, in place of `mode`.
    pub agent: Option<String>,
    slots: Slots,
    /// What each primary agent runs with once pinned: picked by hand, or
    /// left after it ran.
    agents: BTreeMap<String, Llm>,
    /// The running agent's own model, qualified. Until it is pinned the
    /// agent runs on this at act's effort, else on act's, so a profile
    /// cycle moves it along with act.
    own: Option<String>,
    /// In config order; the first is where a chat starts.
    profiles: Vec<Profile>,
    /// The profile ctrl+p is on. A model picked by hand hides its name
    /// until the next ctrl+p puts the profile back.
    profile: Option<usize>,
}

impl Selection {
    /// Each kind of turn on its slot, in act, with no profiles.
    pub fn new(slots: Slots) -> Self {
        Self {
            mode: Mode::default(),
            agent: None,
            slots,
            agents: BTreeMap::new(),
            own: None,
            profiles: Vec::new(),
            profile: None,
        }
    }

    /// Everything on `llm`, in act, with no profiles.
    pub fn same(llm: Llm) -> Self {
        Self::new(Slots::same(llm))
    }

    /// What `config` starts a chat on, in its default mode: the first
    /// profile's slots, else its model for both modes. `qualify` names
    /// each model as the provider lists it; `check` says why one cannot
    /// run, which marks its profile for cycling to skip. Whether ChatGPT
    /// is connected is left to `next_profile`, since `/connect` changes it
    /// mid-run.
    pub fn from_config(
        config: &Config,
        qualify: &dyn Fn(&str) -> String,
        check: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let llm = |(model, effort): (String, _)| Llm {
            model: qualify(&model),
            effort,
        };
        let profiles = config
            .profiles
            .iter()
            .map(|profile| {
                let slots = Slots {
                    plan: llm(config.profile_llm(profile, Mode::Plan)),
                    act: llm(config.profile_llm(profile, Mode::Act)),
                    subagent: config.profile_subagent(profile).map(llm),
                    clerk: config.profile_clerk(profile).map(llm),
                };
                let runs: Vec<&Llm> = [&slots.plan, &slots.act]
                    .into_iter()
                    .chain(&slots.subagent)
                    .chain(&slots.clerk)
                    .collect();
                let on_chatgpt = |llm: &&Llm| {
                    llm.model
                        .split_once('/')
                        .is_some_and(|(id, _)| id == nth_auth::ChatGpt::ID)
                };
                Profile {
                    name: profile.name.clone(),
                    unavailable: runs.iter().find_map(|llm| check(&llm.model)),
                    needs_chatgpt: runs.iter().any(on_chatgpt),
                    slots,
                }
            })
            .collect();
        let mut selection = Self::same(llm((config.model.clone(), Default::default())));
        selection.mode = config.mode.default;
        selection.with_profiles(profiles)
    }

    /// The profiles ctrl+p cycles, every slot on the first.
    pub fn with_profiles(mut self, profiles: Vec<Profile>) -> Self {
        if let Some(first) = profiles.first() {
            self.slots = first.slots.clone();
        }
        self.profile = (!profiles.is_empty()).then_some(0);
        self.profiles = profiles;
        self
    }

    /// What the next turn runs on: the agent's, else the mode's.
    pub fn current(&self) -> Llm {
        match &self.agent {
            Some(name) => self.agent_llm(name),
            None => self.slots.mode(self.mode).clone(),
        }
    }

    /// What the agent `name` runs on: its pin, else its own model at act's
    /// effort, else act's. An agent named without `set_agent` has no own
    /// model, so it runs on act's.
    fn agent_llm(&self, name: &str) -> Llm {
        if let Some(llm) = self.agents.get(name) {
            return llm.clone();
        }
        let act = &self.slots.act;
        match &self.own {
            Some(model) => Llm {
                model: model.clone(),
                effort: act.effort,
            },
            None => act.clone(),
        }
    }

    /// Keeps the running agent on what it runs on now, whatever act and
    /// the profiles do from here.
    fn pin(&mut self) {
        if let Some(name) = self.agent.clone() {
            let llm = self.current();
            self.agents.insert(name, llm);
        }
    }

    /// Puts what runs next on `llm`, as a pick in the model picker does.
    pub fn set_current(&mut self, llm: Llm) {
        match &self.agent {
            Some(name) => {
                self.agents.insert(name.clone(), llm);
            }
            None => *self.slots.mode_mut(self.mode) = llm,
        }
    }

    /// What `target` runs on: subagents on their own, else the current;
    /// the clerk on its own, else the subagents'.
    pub fn llm(&self, target: Target) -> Llm {
        let slots = &self.slots;
        match target {
            Target::Plan => Some(slots.plan.clone()),
            Target::Act => Some(slots.act.clone()),
            Target::Agent => None,
            Target::Subagent => slots.subagent.clone(),
            Target::Clerk => slots.clerk.clone().or_else(|| slots.subagent.clone()),
        }
        .unwrap_or_else(|| self.current())
    }

    /// Puts `target` on `llm`. `Agent` does nothing while no agent runs,
    /// since it is offered only while one does.
    pub fn set(&mut self, target: Target, llm: Llm) {
        match target {
            Target::Plan => self.slots.plan = llm,
            // A pick for act is not one for the agent running on it.
            Target::Act => {
                self.pin();
                self.slots.act = llm;
            }
            Target::Agent if self.agent.is_some() => self.set_current(llm),
            Target::Agent => {}
            Target::Subagent => self.slots.subagent = Some(llm),
            Target::Clerk => self.slots.clerk = Some(llm),
        }
    }

    /// Runs `mode` next, leaving the agent that ran in its place on what
    /// it ran on.
    pub fn switch_mode(&mut self, mode: Mode) {
        self.pin();
        self.own = None;
        self.agent = None;
        self.mode = mode;
    }

    /// Tab: back to the mode an agent ran in place of, else to the other
    /// mode.
    pub fn toggle_mode(&mut self) {
        match self.agent {
            Some(_) => self.switch_mode(self.mode),
            None => self.switch_mode(self.mode.toggled()),
        }
    }

    /// Runs the primary agent `name` next: on what it was picked or left
    /// on in this run, else on `own`, its own model, at act's effort, else
    /// on act's. The agent running before is left on what it ran on.
    pub fn set_agent(&mut self, name: String, own: Option<String>) {
        self.pin();
        self.own = own;
        self.agent = Some(name);
    }

    /// Takes on a session's mode, agent and the llm it runs on; the other
    /// slots keep theirs.
    pub fn resume(&mut self, mode: Mode, agent: Option<String>, llm: Llm) {
        self.own = None;
        self.mode = mode;
        self.agent = agent;
        self.set_current(llm);
    }

    pub fn slots(&self) -> &Slots {
        &self.slots
    }

    /// The active profile's name, while every slot still runs it. A model
    /// picked by hand, or a resumed session's own, leaves the profile, and
    /// ctrl+p puts it back.
    pub fn profile_name(&self) -> Option<&str> {
        let profile = self.profiles.get(self.profile?)?;
        (profile.slots == self.slots).then_some(profile.name.as_str())
    }

    /// ctrl+p: every slot on the next profile that can run. A slot picked
    /// by hand since the profile was applied means it no longer runs: the
    /// first ctrl+p puts the profile back, the next moves on. A profile
    /// on ChatGPT runs only while `chatgpt_connected`.
    pub fn next_profile(&mut self, chatgpt_connected: bool) -> Cycled {
        if self.profiles.is_empty() {
            return Cycled::NoProfiles;
        }
        let len = self.profiles.len();
        // Back where the profile was left, before moving on.
        if let Some(i) = self.profile
            && self.profile_name().is_none()
        {
            return self.apply(i, Vec::new());
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
            let profile = &self.profiles[i];
            let blocked = profile.unavailable.clone().or_else(|| {
                (profile.needs_chatgpt && !chatgpt_connected)
                    .then(|| "ChatGPT is not connected".into())
            });
            match blocked {
                None => return self.apply(i, skipped),
                Some(reason) => skipped.push(format!("{} ({reason})", profile.name)),
            }
        }
        Cycled::Stayed(skipped)
    }

    fn apply(&mut self, i: usize, skipped: Vec<String>) -> Cycled {
        let profile = &self.profiles[i];
        self.slots = profile.slots.clone();
        self.profile = Some(i);
        Cycled::Applied {
            name: profile.name.clone(),
            skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Effort;

    use super::*;

    fn llm(model: &str, effort: Effort) -> Llm {
        Llm {
            model: model.into(),
            effort,
        }
    }

    fn plain(model: &str) -> Llm {
        llm(model, Effort::Default)
    }

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.into(),
            slots: Slots {
                plan: plain(&format!("{name}-plan")),
                act: plain(&format!("{name}-act")),
                subagent: Some(plain(&format!("{name}-subagent"))),
                clerk: Some(plain(&format!("{name}-clerk"))),
            },
            unavailable: None,
            needs_chatgpt: false,
        }
    }

    fn profiled(profiles: Vec<Profile>) -> Selection {
        Selection::same(plain("unused")).with_profiles(profiles)
    }

    fn applied(name: &str) -> Cycled {
        Cycled::Applied {
            name: name.into(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn tab_cycles_the_targets_and_wraps() {
        assert_eq!(Target::of(Mode::Plan).next(false), Target::Act);
        assert_eq!(Target::Act.next(false), Target::Subagent);
        assert_eq!(Target::Subagent.next(false), Target::Clerk);
        assert_eq!(Target::Clerk.next(false), Target::Plan);
        assert_eq!(Target::Plan.prev(false), Target::Clerk);
        assert_eq!(Target::Act.next(true), Target::Agent, "while one runs");
        assert_eq!(Target::Agent.next(true), Target::Subagent);
    }

    #[test]
    fn current_is_the_agents_then_the_modes() {
        let mut selection = Selection::same(plain("glm"));
        selection.set(Target::Act, llm("glm", Effort::High));
        selection.set(Target::Plan, plain("kimi"));
        selection.switch_mode(Mode::Plan);
        assert_eq!(selection.current(), plain("kimi"));

        selection.set_agent("review".into(), Some("zen/qwen".into()));
        assert_eq!(
            selection.current(),
            llm("zen/qwen", Effort::High),
            "its own model at act's effort"
        );
        selection.set_agent("explore".into(), None);
        assert_eq!(selection.current(), llm("glm", Effort::High), "act's");

        selection.set_current(plain("picked"));
        selection.set_agent("review".into(), Some("zen/qwen".into()));
        selection.set_agent("explore".into(), None);
        assert_eq!(
            selection.current(),
            plain("picked"),
            "what it ran on last holds for the run"
        );
        assert_eq!(selection.llm(Target::Act), llm("glm", Effort::High));

        selection.toggle_mode();
        assert_eq!((selection.mode, &selection.agent), (Mode::Plan, &None));
        assert_eq!(selection.current(), plain("kimi"));
    }

    #[test]
    fn an_agent_follows_a_profile_cycle_until_picked() {
        let mut selection = profiled(vec![profile("go"), profile("zen")]);
        selection.set_agent("review".into(), None);
        assert_eq!(selection.current(), plain("go-act"));

        assert_eq!(selection.next_profile(true), applied("zen"));
        assert_eq!(selection.current(), plain("zen-act"), "act's, as act moved");

        let mut selection = profiled(vec![profile("go"), profile("zen")]);
        selection.set(Target::Act, llm("go-act", Effort::High));
        selection.set_agent("review".into(), Some("own".into()));
        assert_eq!(selection.current(), llm("own", Effort::High));
        selection.next_profile(true);
        assert_eq!(
            selection.current(),
            plain("own"),
            "its own model at the new act's effort"
        );

        selection.set_current(plain("picked"));
        assert_eq!(selection.next_profile(true), applied("zen"));
        assert_eq!(selection.current(), plain("picked"), "a pick stays");

        selection.set_agent("explore".into(), None);
        selection.set(Target::Act, plain("act-pick"));
        assert_eq!(
            selection.current(),
            plain("zen-act"),
            "a pick for act is not one for the agent"
        );

        let mut selection = profiled(vec![profile("go"), profile("zen")]);
        selection.set_agent("explore".into(), None);
        selection.toggle_mode();
        assert_eq!(selection.next_profile(true), applied("zen"));
        selection.set_agent("explore".into(), None);
        assert_eq!(selection.current(), plain("go-act"), "left, it stays");
    }

    #[test]
    fn a_resumed_session_runs_its_agent_or_its_mode_on_its_llm() {
        let mut selection = Selection::new(Slots {
            plan: plain("kimi"),
            act: plain("glm"),
            subagent: None,
            clerk: None,
        });

        selection.resume(Mode::Plan, Some("review".into()), plain("saved"));
        assert_eq!(selection.agent.as_deref(), Some("review"));
        assert_eq!(selection.current(), plain("saved"), "on the agent");
        assert_eq!(selection.llm(Target::Plan), plain("kimi"));
        assert_eq!(selection.llm(Target::Act), plain("glm"));

        // An agent gone is passed as none: the session runs in its mode.
        selection.resume(Mode::Plan, None, plain("saved"));
        assert_eq!(selection.agent, None);
        assert_eq!(
            selection.llm(Target::Plan),
            plain("saved"),
            "in plan's slot"
        );
        assert_eq!(selection.llm(Target::Act), plain("glm"));
    }

    #[test]
    fn subagent_falls_back_to_current_and_clerk_to_subagent() {
        let mut selection = Selection::same(plain("glm"));
        assert_eq!(selection.llm(Target::Subagent), plain("glm"));
        assert_eq!(selection.llm(Target::Clerk), plain("glm"));

        selection.set_current(plain("kimi"));
        assert_eq!(selection.llm(Target::Clerk), plain("kimi"), "the current");

        selection.set(Target::Subagent, plain("flash"));
        assert_eq!(selection.llm(Target::Subagent), plain("flash"));
        assert_eq!(
            selection.llm(Target::Clerk),
            plain("flash"),
            "the subagents'"
        );

        selection.set(Target::Clerk, plain("mini"));
        assert_eq!(selection.llm(Target::Clerk), plain("mini"));
        assert_eq!(selection.llm(Target::Subagent), plain("flash"));
    }

    #[test]
    fn a_picked_model_leaves_the_profile_and_cycling_puts_it_back() {
        let mut selection = profiled(vec![profile("go"), profile("zen")]);
        assert_eq!(selection.profile_name(), Some("go"));

        for target in [Target::Plan, Target::Subagent, Target::Clerk] {
            selection.set(target, plain("picked"));
            assert_eq!(selection.profile_name(), None, "{target:?} leaves it");

            assert_eq!(selection.next_profile(true), applied("go"), "{target:?}");
            assert_eq!(selection.slots(), &profile("go").slots, "put back");
            assert_eq!(selection.profile_name(), Some("go"));
        }

        assert_eq!(
            selection.next_profile(true),
            applied("zen"),
            "then moves on"
        );
        assert_eq!(selection.current(), plain("zen-act"));
        assert_eq!(selection.next_profile(true), applied("go"), "and wraps");
    }

    #[test]
    fn a_blocked_profile_is_skipped_and_named() {
        let mut zen = profile("zen");
        zen.unavailable = Some("ZEN_API_KEY is not set".into());
        let mut codex = profile("codex");
        codex.needs_chatgpt = true;
        let mut selection = profiled(vec![profile("go"), zen, codex, profile("ly")]);

        assert_eq!(
            selection.next_profile(false),
            Cycled::Applied {
                name: "ly".into(),
                skipped: vec![
                    "zen (ZEN_API_KEY is not set)".into(),
                    "codex (ChatGPT is not connected)".into()
                ],
            }
        );
        assert_eq!(selection.next_profile(true), applied("go"));
        assert_eq!(
            selection.next_profile(true),
            Cycled::Applied {
                name: "codex".into(),
                skipped: vec!["zen (ZEN_API_KEY is not set)".into()],
            },
            "runs once ChatGPT is connected"
        );

        let mut zen = profile("zen");
        zen.unavailable = Some("ZEN_API_KEY is not set".into());
        let mut selection = profiled(vec![profile("go"), zen]);
        assert_eq!(
            selection.next_profile(true),
            Cycled::Stayed(vec!["zen (ZEN_API_KEY is not set)".into()])
        );
        assert_eq!(selection.current(), plain("go-act"), "stays where it is");

        let mut selection = profiled(vec![profile("go")]);
        assert_eq!(selection.next_profile(true), Cycled::Stayed(Vec::new()));
        let mut selection = Selection::same(plain("glm"));
        assert_eq!(selection.next_profile(true), Cycled::NoProfiles);
    }

    #[test]
    fn a_resumed_session_fills_the_missing_model_for_every_slot() {
        // The config names no model a slot could run, so a resumed session
        // fills it with its own (`Config::needs_model`): every slot runs
        // that, and subagents and the clerk, naming none, run on their
        // parent's.
        let expected = Slots::same(plain("zen/a"));
        for text in ["", "[[profiles]]\nname = \"p\"\n"] {
            let mut config = Config::parse(text).expect("parses");
            assert!(config.needs_model());
            config.model = "zen/a".into();

            let selection = Selection::from_config(&config, &|m| m.into(), &|_| None);

            assert_eq!(selection.slots(), &expected, "{text:?}");
            for profile in &selection.profiles {
                assert_eq!(profile.slots, expected, "{text:?}");
            }
        }
    }

    #[test]
    fn profiles_are_qualified_checked_and_marked_for_chatgpt() {
        let config = Config::parse(
            r#"
            model = "a"

            [[profiles]]
            name = "go"
            [profiles.plan]
            effort = "high"
            [profiles.clerk]
            model = "chatgpt/mini"

            [[profiles]]
            name = "zen"
            [profiles.act]
            model = "b"
            "#,
        )
        .expect("parses");
        let qualify = |model: &str| match model.contains('/') {
            true => model.to_string(),
            false => format!("zen/{model}"),
        };
        let check = |model: &str| (model == "zen/b").then(|| "ZEN_API_KEY is not set".into());

        let selection = Selection::from_config(&config, &qualify, &check);

        assert_eq!(selection.mode, Mode::Plan, "the config's default");
        assert_eq!(
            selection.slots(),
            &Slots {
                plan: llm("zen/a", Effort::High),
                act: plain("zen/a"),
                subagent: None,
                clerk: Some(plain("chatgpt/mini")),
            },
            "the first profile's"
        );
        let [go, zen] = &selection.profiles[..] else {
            panic!("two profiles");
        };
        assert!(go.needs_chatgpt && go.unavailable.is_none());
        assert!(!zen.needs_chatgpt);
        assert_eq!(zen.unavailable.as_deref(), Some("ZEN_API_KEY is not set"));
    }
}
