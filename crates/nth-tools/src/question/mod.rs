use std::ops::RangeInclusive;

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Answer, Question as Asked, Reply, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

const QUESTIONS: RangeInclusive<usize> = 1..=4;
const OPTIONS: RangeInclusive<usize> = 2..=4;
const HEADER_CHARS: usize = 16;

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
                                "header": { "type": "string", "description": format!("A label of at most {HEADER_CHARS} characters, such as \"Auth\"") },
                                "multiple": { "type": "boolean", "description": "Allow picking more than one option", "default": false },
                                "options": {
                                    "type": "array",
                                    "minItems": OPTIONS.start(),
                                    "maxItems": OPTIONS.end(),
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string", "description": "The choice, in 1 to 5 words" },
                                            "description": { "type": "string", "description": "What the choice means or costs, in one line" },
                                            "preview": { "type": "string", "description": "Several lines shown beside the options while this one is highlighted, as written: an ASCII mockup, a code snippet or a config" }
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
            let Args { mut questions } = nth_protocol::parse_args(args)?;
            check(&mut questions)?;
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
/// rather than the panel cutting things off. A header too long for its tab
/// is only cut: refusing it costs the model a step to say the same in fewer
/// letters.
fn check(questions: &mut [Asked]) -> Result<(), String> {
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
        if q.header.trim().is_empty() {
            return Err(format!("\"{}\" has an empty header", q.question));
        }
        if q.header.chars().count() > HEADER_CHARS {
            let cut: String = q.header.chars().take(HEADER_CHARS - 1).collect();
            q.header = format!("{}…", cut.trim_end());
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
                { "label": "OAuth", "description": "works with SSO", "preview": "login → idp\n  ← token" },
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
            assert_eq!(
                ask.questions[0].options[0].preview.as_deref(),
                Some("login → idp\n  ← token")
            );
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
        let no_header = json!({ "questions": [
            { "question": "Which?", "header": " ",
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
        assert_eq!(
            Question.call(no_header, &ctx).await,
            Err("\"Which?\" has an empty header".into())
        );
    }

    #[tokio::test]
    async fn cuts_a_header_too_long_for_its_tab() {
        let (ctx, mut rx) = asking();
        let asked = tokio::spawn(async move {
            let ask = rx.recv().await.expect("asks");
            let headers: Vec<String> = ask.questions.iter().map(|q| q.header.clone()).collect();
            ask.reply.send(Reply::Declined).expect("tool waits");
            headers
        });
        let questions = json!({ "questions": [
            { "question": "Which?", "header": "All-time surface", "options": [{ "label": "a" }, { "label": "b" }] },
            { "question": "Where?", "header": "Claude on lenovo laptop", "options": [{ "label": "a" }, { "label": "b" }] },
            { "question": "What?", "header": "Session switch it", "options": [{ "label": "a" }, { "label": "b" }] }
        ]});

        let _ = Question.call(questions, &ctx).await;

        assert_eq!(
            asked.await.expect("asked"),
            ["All-time surface", "Claude on lenov…", "Session switch…"]
        );
    }
}
