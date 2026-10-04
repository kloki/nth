use std::ops::RangeInclusive;

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Answer, Question as Asked, Reply, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

const QUESTIONS: RangeInclusive<usize> = 1..=4;
const OPTIONS: RangeInclusive<usize> = 2..=4;
const HEADER_CHARS: usize = 12;

/// Asks you multiple-choice questions mid-turn and waits for the answers.
/// Every question also gets an open field, which the front-end adds.
pub struct Question;

#[derive(Deserialize)]
struct Args {
    questions: Vec<Asked>,
}

impl Tool for Question {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "question",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": QUESTIONS.start(),
                        "maxItems": QUESTIONS.end(),
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string", "description": "The full question, ending with a question mark" },
                                "header": { "type": "string", "description": "A label of at most 12 characters, such as \"Auth\"" },
                                "multiple": { "type": "boolean", "description": "Allow picking more than one option", "default": false },
                                "options": {
                                    "type": "array",
                                    "minItems": OPTIONS.start(),
                                    "maxItems": OPTIONS.end(),
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string", "description": "The choice, in 1 to 5 words" },
                                            "description": { "type": "string", "description": "What the choice means or costs, in one line" }
                                        },
                                        "required": ["label"]
                                    }
                                }
                            },
                            "required": ["question", "header", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let Args { questions } = crate::parse_args(args)?;
            check(&questions)?;
            match ctx.asker.ask(questions.clone()).await {
                Some(Reply::Answered(answers)) => Ok(format(&questions, &answers)),
                Some(Reply::Declined) => {
                    Err("the user declined to answer; decide yourself and say what you assumed".into())
                }
                None => Err("there is no user to ask in this session; decide yourself and say what you assumed".into()),
            }
        }
        .boxed()
    }
}

/// Holds the model to what the input panel can show, so it hears why
/// rather than the panel cutting things off.
fn check(questions: &[Asked]) -> Result<(), String> {
    if !QUESTIONS.contains(&questions.len()) {
        return Err(format!("ask 1 to 4 questions, not {}", questions.len()));
    }
    for q in questions {
        if !OPTIONS.contains(&q.options.len()) {
            return Err(format!(
                "\"{}\" has {} options; give 2 to 4",
                q.header,
                q.options.len()
            ));
        }
        let chars = q.header.chars().count();
        if chars == 0 || chars > HEADER_CHARS {
            return Err(format!(
                "header \"{}\" must be 1 to {HEADER_CHARS} characters",
                q.header
            ));
        }
    }
    Ok(())
}

/// One line per question: the question, then what was picked and, in
/// quotes, what was typed.
fn format(questions: &[Asked], answers: &[Answer]) -> String {
    let lines: Vec<String> = questions
        .iter()
        .zip(answers)
        .map(|(q, answer)| {
            let typed = answer.typed.iter().map(|t| format!("\"{t}\""));
            let parts: Vec<String> = answer.picked.iter().cloned().chain(typed).collect();
            format!("\"{}\" = {}", q.question, parts.join(", "))
        })
        .collect();
    format!("The user answered:\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Ask, Asker};
    use tokio::sync::mpsc;

    use super::*;

    fn args() -> serde_json::Value {
        json!({ "questions": [
            { "question": "Which auth?", "header": "Auth", "options": [
                { "label": "OAuth", "description": "works with SSO" },
                { "label": "API key" } ] },
            { "question": "Which checks?", "header": "Checks", "multiple": true, "options": [
                { "label": "fmt" }, { "label": "clippy" }, { "label": "test" } ] }
        ]})
    }

    /// A context whose questions land on the returned receiver.
    fn asking() -> (ToolContext, mpsc::Receiver<Ask>) {
        let (tx, rx) = mpsc::channel(1);
        let ctx = ToolContext {
            asker: Asker::new(tx).for_call("c1".into()),
            ..ToolContext::new(".".into())
        };
        (ctx, rx)
    }

    #[tokio::test]
    async fn returns_one_line_per_answer() {
        let (ctx, mut rx) = asking();
        let answering = tokio::spawn(async move {
            let ask = rx.recv().await.expect("asks");
            assert_eq!(ask.call_id, "c1");
            assert_eq!(ask.questions.len(), 2);
            assert!(ask.questions[1].multiple);
            let answers = vec![
                Answer {
                    picked: vec!["OAuth".into()],
                    typed: None,
                },
                Answer {
                    picked: vec!["fmt".into(), "clippy".into()],
                    typed: Some("and a doc check".into()),
                },
            ];
            ask.reply
                .send(Reply::Answered(answers))
                .expect("tool waits");
        });

        let out = Question.call(args(), &ctx).await;
        answering.await.expect("answers");

        assert_eq!(
            out,
            Ok("The user answered:\n\
                \"Which auth?\" = OAuth\n\
                \"Which checks?\" = fmt, clippy, \"and a doc check\""
                .into())
        );
    }

    #[tokio::test]
    async fn tells_the_model_when_you_decline() {
        let (ctx, mut rx) = asking();
        tokio::spawn(async move {
            let ask = rx.recv().await.expect("asks");
            ask.reply.send(Reply::Declined).expect("tool waits");
        });

        let out = Question.call(args(), &ctx).await;

        assert!(out.expect_err("declined").contains("declined"));
    }

    #[tokio::test]
    async fn without_a_user_the_model_decides() {
        let ctx = ToolContext::new(".".into());

        let out = Question.call(args(), &ctx).await;

        assert!(out.expect_err("nobody").contains("no user to ask"));
    }

    #[tokio::test]
    async fn refuses_what_the_panel_cannot_show() {
        let ctx = ToolContext::new(".".into());
        let one_option = json!({ "questions": [
            { "question": "Sure?", "header": "Sure", "options": [{ "label": "yes" }] }
        ]});
        let long_header = json!({ "questions": [
            { "question": "Which?", "header": "a header that is far too long",
              "options": [{ "label": "a" }, { "label": "b" }] }
        ]});

        assert_eq!(
            Question.call(json!({ "questions": [] }), &ctx).await,
            Err("ask 1 to 4 questions, not 0".into())
        );
        assert_eq!(
            Question.call(one_option, &ctx).await,
            Err("\"Sure\" has 1 options; give 2 to 4".into())
        );
        assert!(
            Question
                .call(long_header, &ctx)
                .await
                .expect_err("too long")
                .contains("1 to 12 characters")
        );
    }
}
