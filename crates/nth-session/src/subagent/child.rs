//! The session a subagent runs on, made from the call that starts it.

use nth_context::Agent;
use nth_protocol::{Llm, Mode, ModelSettings, ToolContext};

use crate::{Session, TurnEnv};

/// A new subagent session of `agent` in the parent's directory, in act
/// mode: a planning parent's child is kept from writing by its tools
/// instead, so it gets no plan file and no plan-mode reminder. It runs on
/// the agent's own model, else what the front-end picked for the clerk (for
/// the clerk), else for subagents, else the parent's; the effort and limits
/// come from the latter three.
pub(crate) fn child_session(agent: &Agent, ctx: &ToolContext, max_steps: usize) -> Session {
    let picked = ctx.child.for_agent(agent.clerk);
    let settings = match &agent.model {
        // An agent's own model is not listed here, so it counts as taking
        // images, as an unknown one does, and prices by the live listing.
        Some(model) => ModelSettings {
            llm: Llm {
                model: model.clone(),
                effort: picked.llm.effort,
            },
            takes_images: true,
            cost: None,
            ..picked.clone()
        },
        None => picked.clone(),
    };
    let mut session = Session::new(settings.llm.model, ctx.cwd.clone())
        .with_extra_dirs(ctx.extra_dirs.clone())
        .with_context(ctx.context.clone())
        .as_subagent(agent.prompt.clone());
    session.effort = settings.llm.effort;
    if let Some(root) = &ctx.state_root {
        session.state_root = Some(root.clone());
    }
    session.mode = Mode::Act;
    session.set_limits(settings.context_window, settings.max_output);
    session.parent = ctx.session.parse().ok();
    session.env = TurnEnv {
        compaction: ctx.child.compaction,
        max_steps,
        tool_timeout: ctx.tool_timeout,
        takes_images: settings.takes_images,
        cost: settings.cost,
        // Its images go with its parent's, so deleting the parent deletes them.
        images: ctx.images.clone(),
        // A subagent has no task tool, so it starts none of its own.
        subagent: None,
        clerk: None,
    };
    session
}

#[cfg(test)]
mod tests {
    use nth_context::{Context, Paths};
    use nth_protocol::{ChildLlm, Compaction, Cost, Effort};

    use super::*;
    use crate::subagent::tests::explore;

    fn llm(model: &str, effort: Effort) -> Llm {
        Llm {
            model: model.into(),
            effort,
        }
    }

    fn clerk() -> Agent {
        Context::discover(std::path::Path::new("/nowhere"), &Paths::default())
            .agents
            .get("clerk")
            .expect("built in")
            .clone()
    }

    fn picked(model: &str) -> ModelSettings {
        ModelSettings {
            context_window: Some(32_000),
            ..ModelSettings::bare(llm(model, Effort::Minimal))
        }
    }

    /// The parent's own model, for a context that names one.
    fn own() -> ModelSettings {
        ModelSettings::bare(llm("glm-5.3", Effort::High))
    }

    fn ctx(child: ChildLlm) -> ToolContext {
        ToolContext {
            child,
            ..ToolContext::new("/repo".into())
        }
    }

    #[test]
    fn a_child_session_records_its_parent() {
        let parent = uuid::Uuid::new_v4();
        let ctx = ToolContext {
            session: parent.to_string(),
            child: ChildLlm {
                own: ModelSettings {
                    context_window: Some(200_000),
                    max_output: Some(64_000),
                    ..ModelSettings::default()
                },
                ..ChildLlm::default()
            },
            tool_timeout: Some(std::time::Duration::from_secs(60)),
            ..ToolContext::new("/repo".into())
        };
        let child = child_session(&explore(), &ctx, 5);
        assert!(child.subagent);
        assert_eq!(child.parent, Some(parent));
        assert_eq!(child.context_window, Some(200_000));
        assert_eq!(child.max_output, Some(64_000));
        assert_eq!(child.env.tool_timeout, ctx.tool_timeout);
    }

    #[test]
    fn a_child_runs_on_the_subagent_llm_unless_its_agent_names_one() {
        let ctx = ToolContext {
            child: ChildLlm {
                own: ModelSettings {
                    context_window: Some(200_000),
                    ..own()
                },
                subagent: Some(ModelSettings {
                    llm: llm("minimax", Effort::Low),
                    context_window: Some(100_000),
                    max_output: Some(8_000),
                    takes_images: false,
                    cost: None,
                }),
                ..ChildLlm::default()
            },
            images: Some("/data/images/parent".into()),
            ..ToolContext::new("/repo".into())
        };
        let child = child_session(&explore(), &ctx, 5);
        assert!(!child.env.takes_images, "the subagent model takes none");
        assert_eq!(child.env.images, ctx.images, "its parent's folder");
        assert_eq!(
            (child.model.as_str(), child.effort),
            ("minimax", Effort::Low)
        );
        assert_eq!(
            (child.context_window, child.max_output),
            (Some(100_000), Some(8_000)),
            "sized against its own model, not the parent's"
        );

        let mut own = explore();
        own.model = Some("kimi".into());
        let child = child_session(&own, &ctx, 5);
        assert_eq!(child.model, "kimi", "the agent's own model wins");
    }

    #[test]
    fn the_clerk_runs_on_its_llm_else_the_subagents_else_the_parents() {
        let mut ctx = ctx(ChildLlm {
            own: own(),
            subagent: Some(picked("minimax")),
            clerk: Some(picked("flash")),
            ..ChildLlm::default()
        });
        let child = child_session(&clerk(), &ctx, 5);
        assert_eq!(
            (child.model.as_str(), child.effort, child.context_window),
            ("flash", Effort::Minimal, Some(32_000))
        );
        assert_eq!(
            child_session(&explore(), &ctx, 5).model,
            "minimax",
            "only the clerk runs on it"
        );

        ctx.child.clerk = None;
        assert_eq!(child_session(&clerk(), &ctx, 5).model, "minimax");
        ctx.child.subagent = None;
        assert_eq!(child_session(&clerk(), &ctx, 5).model, "glm-5.3");
    }

    /// The `[context]` policy rides where the limits do: a child of a
    /// session with automatic compaction off must not compact on its own.
    #[test]
    fn a_child_session_inherits_the_compaction_policy() {
        let ctx = ctx(ChildLlm {
            compaction: Compaction {
                auto: false,
                reserved: 1_500,
                preserve: 700,
            },
            ..ChildLlm::default()
        });
        let child = child_session(&explore(), &ctx, 5);
        assert_eq!(child.env.compaction, ctx.child.compaction);
        assert!(
            !child.env.compaction.auto,
            "the parent turned it off and the child copies it"
        );
        assert_eq!(
            (child.env.compaction.reserved, child.env.compaction.preserve),
            (1_500, 700)
        );
    }

    #[test]
    fn a_child_picks_clerk_over_subagent_over_own_and_an_agent_model_wins() {
        let cost = |input| Cost {
            input,
            output: 1.0,
            cache_read: None,
            cache_write: None,
        };
        let settings = |model: &str, window, images, price| ModelSettings {
            llm: llm(model, Effort::Low),
            context_window: Some(window),
            max_output: Some(window / 10),
            takes_images: images,
            cost: Some(cost(price)),
        };
        let own = settings("own", 1_000, false, 1.0);
        let subagent = settings("sub", 2_000, false, 2.0);
        let clerk_llm = settings("clerk", 3_000, false, 3.0);
        let child = |with_subagent: bool, with_clerk: bool| ChildLlm {
            own: own.clone(),
            subagent: with_subagent.then(|| subagent.clone()),
            clerk: with_clerk.then(|| clerk_llm.clone()),
            compaction: Compaction::default(),
        };
        // (subagent picked, clerk picked, runs the clerk, what it runs on)
        let table = [
            (false, false, false, &own),
            (false, false, true, &own),
            (true, false, false, &subagent),
            (true, false, true, &subagent),
            (false, true, false, &own),
            (false, true, true, &clerk_llm),
            (true, true, false, &subagent),
            (true, true, true, &clerk_llm),
        ];
        for (with_subagent, with_clerk, is_clerk, expected) in table {
            let case =
                format!("subagent {with_subagent}, clerk {with_clerk}, runs clerk {is_clerk}");
            let ctx = ctx(child(with_subagent, with_clerk));
            let mut agent = match is_clerk {
                true => clerk(),
                false => explore(),
            };
            let session = child_session(&agent, &ctx, 5);
            assert_eq!(
                (session.model.as_str(), session.effort),
                (expected.llm.model.as_str(), expected.llm.effort),
                "{case}"
            );
            assert_eq!(
                (session.context_window, session.max_output),
                (expected.context_window, expected.max_output),
                "{case}"
            );
            assert_eq!(session.env.takes_images, expected.takes_images, "{case}");
            assert_eq!(session.env.cost, expected.cost, "{case}");

            // An agent's own model is not listed: it takes images and
            // prices by the live listing, on the picked effort and limits.
            agent.model = Some("zen/own".into());
            let session = child_session(&agent, &ctx, 5);
            assert_eq!(
                (session.model.as_str(), session.effort),
                ("zen/own", expected.llm.effort),
                "{case}"
            );
            assert_eq!(
                (session.context_window, session.max_output),
                (expected.context_window, expected.max_output),
                "{case}"
            );
            assert!(session.env.takes_images, "{case}");
            assert_eq!(session.env.cost, None, "{case}");
        }
    }
}
