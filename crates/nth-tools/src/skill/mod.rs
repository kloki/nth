use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

/// Loads a skill's body into the conversation. The skills come from the
/// session's context, so they follow it to whichever directory it is in.
pub struct Skill;

#[derive(Deserialize)]
struct Args {
    name: String,
}

impl Tool for Skill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The name of the skill from available_skills" }
                },
                "required": ["name"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::parse_args(args)?;
            let skills = &ctx.context.skills;
            let Some(skill) = skills.get(&args.name) else {
                let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
                let available = match names.is_empty() {
                    true => "none".to_string(),
                    false => names.join(", "),
                };
                return Err(format!(
                    "Skill \"{}\" not found. Available skills: {available}",
                    args.name
                ));
            };
            // Nothing goes to the output sink: the chat shows a loaded
            // skill as one row, without its body.
            skill.render().await
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_context::{Context, Paths};

    use super::*;

    fn context_with_skill(dir: &std::path::Path) -> ToolContext {
        let skill = dir.join(".agents/skills/deploy");
        std::fs::create_dir_all(&skill).expect("dirs");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: ship it\n---\nRun the release.\n",
        )
        .expect("writes");
        let context = Context::discover(dir, &Paths::default());
        ToolContext {
            context: Arc::new(context),
            ..ToolContext::new(dir.to_path_buf())
        }
    }

    #[tokio::test]
    async fn loads_a_known_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = context_with_skill(dir.path());

        let out = Skill
            .call(json!({ "name": "deploy" }), &ctx)
            .await
            .expect("loads");

        assert!(
            out.starts_with(
                "<skill_content name=\"deploy\">\n# Skill: deploy\n\nRun the release.\n"
            ),
            "{out}"
        );
    }

    #[tokio::test]
    async fn an_unknown_skill_lists_the_known_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = context_with_skill(dir.path());

        let out = Skill.call(json!({ "name": "nope" }), &ctx).await;

        assert_eq!(
            out,
            Err("Skill \"nope\" not found. Available skills: deploy".into())
        );
    }
}
