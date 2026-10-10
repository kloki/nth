//! The guard against a model stuck calling the same tool with the same
//! input: it asks before running the call again, or stops the turn when
//! there is nobody to ask.

use nth_protocol::{Message, Question, QuestionOption, Reply, ToolCall, ToolContext};

use super::{Error, INTERRUPTED, Turn, failed};

/// How many times the same call in a row trips the doom-loop guard.
pub(super) const DOOM_LOOP_THRESHOLD: usize = 3;
/// The doom-loop question's answers.
const CONTINUE: &str = "Run it";
const STOP: &str = "Stop";
/// Why calls the user stopped at the doom-loop prompt failed.
const STOPPED: &str = "stopped by the user (the same call kept repeating)";
/// Why calls stopped at a doom loop with nobody to ask failed: a subagent's,
/// or a headless run's.
const UNASKED: &str = "stopped: the same call kept repeating and nobody could be asked";

impl Turn<'_> {
    /// Asks before running the latest reply's `calls` when one of them
    /// completes a run of repeats since `start`, or stops when there is
    /// nobody to ask. Stopped, every call is answered, so the history stays
    /// valid.
    pub(super) async fn check(&mut self, start: usize, calls: &[ToolCall]) -> Result<(), Error> {
        let Some(call) = repeating(&self.messages[start..]).cloned() else {
            return Ok(());
        };
        let reason = match self.ctx.asker.reaches_someone() {
            true => STOPPED,
            false => UNASKED,
        };
        let (ctx, cancel) = (self.ctx, self.cancel);
        let stopped = tokio::select! {
            biased;
            _ = cancel.cancelled() => Some((INTERRUPTED, Error::Interrupted)),
            run = confirm_doom_loop(ctx, &call) => {
                (!run).then(|| (reason, Error::DoomLoop(call.name.clone())))
            }
        };
        let Some((reason, error)) = stopped else {
            return Ok(());
        };
        self.messages
            .extend(calls.iter().map(|call| Message::ToolResult {
                call_id: call.id.clone(),
                content: failed(reason),
            }));
        Err(error)
    }
}

/// The first of the latest reply's calls that completes a run of identical
/// calls (same name and arguments) `DOOM_LOOP_THRESHOLD` long, counted over
/// `turn`. Only whole multiples of the threshold count, so after a "Run it"
/// the question comes back after another full run, not on every repeat.
fn repeating(turn: &[Message]) -> Option<&ToolCall> {
    let replies = turn.iter().filter_map(|message| match message {
        Message::Assistant(reply) => Some(reply),
        _ => None,
    });
    let latest = replies.clone().next_back()?.tool_calls.len();
    let calls: Vec<&ToolCall> = replies.flat_map(|reply| &reply.tool_calls).collect();
    let first_new = calls.len() - latest;
    let mut run = 0;
    for (i, call) in calls.iter().enumerate() {
        let same =
            i > 0 && (&calls[i - 1].name, &calls[i - 1].arguments) == (&call.name, &call.arguments);
        run = if same { run + 1 } else { 1 };
        if i >= first_new && run % DOOM_LOOP_THRESHOLD == 0 {
            return Some(call);
        }
    }
    None
}

/// Asks whether to run a call that keeps repeating. `true` means run it. A
/// decline stops the turn, and so does having nobody to ask (a subagent, or
/// a headless run): letting the loop through would only burn the step
/// budget on the same call.
async fn confirm_doom_loop(ctx: &ToolContext, call: &ToolCall) -> bool {
    if !ctx.asker.reaches_someone() {
        return false;
    }
    let question = Question {
        question: format!(
            "The model called {} with the same input {} times in a row. Run it again?",
            call.name, DOOM_LOOP_THRESHOLD
        ),
        header: "Doom loop".into(),
        multiple: false,
        options: [CONTINUE, STOP]
            .map(|label| QuestionOption {
                label: label.into(),
                description: None,
                preview: None,
            })
            .into(),
    };
    match ctx
        .asker
        .for_call(call.id.clone())
        .ask(vec![question])
        .await
    {
        Some(Reply::Answered(answers)) => !answers
            .first()
            .is_some_and(|answer| answer.picked.iter().any(|picked| picked == STOP)),
        // Declined, or nobody answered: stop.
        _ => false,
    }
}

#[cfg(test)]
mod tests {

    use nth_protocol::{
        Answer, Asker, AssistantMessage, Message, Reply, StreamEvent, Tool, ToolContext,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{Error, Turn, tests::*};

    /// `n` identical calls in one reply, then an answer.
    fn looping_reply(n: usize) -> Vec<Vec<StreamEvent>> {
        vec![
            (1..=n)
                .map(|i| StreamEvent::ToolCall(call(&i.to_string(), "echo", r#"{"say":"hi"}"#)))
                .collect(),
            vec![StreamEvent::TextDelta("done".into())],
        ]
    }

    /// Answers every question with `answer`; the task returns how many it
    /// answered once the asker is dropped.
    fn answering(answer: Reply) -> (Asker, tokio::task::JoinHandle<usize>) {
        let (asks, mut asked) = mpsc::channel::<nth_protocol::Ask>(1);
        let task = tokio::spawn(async move {
            let mut answered = 0;
            while let Some(ask) = asked.recv().await {
                ask.reply.send(answer.clone()).expect("the loop waits");
                answered += 1;
            }
            answered
        });
        (Asker::new(asks), task)
    }

    fn run_it() -> Reply {
        Reply::Answered(vec![Answer {
            picked: vec![CONTINUE.into()],
            typed: None,
        }])
    }

    #[tokio::test]
    async fn the_same_call_three_times_asks_before_running() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (asker, answering) = answering(run_it());
        let ctx = ToolContext {
            asker,
            ..ToolContext::new(".".into())
        };
        let mut messages = vec![Message::User("go".into())];

        turn(&provider, &tools, &ctx, &mut messages)
            .await
            .0
            .expect("turn completes");
        drop(ctx);

        assert_eq!(answering.await.expect("answers"), 1);
        let results: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult { .. }))
            .collect();
        assert_eq!(results.len(), DOOM_LOOP_THRESHOLD, "all three ran");
    }

    #[tokio::test]
    async fn running_a_loop_on_asks_again_only_after_another_full_run() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD + 1));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (asker, answering) = answering(run_it());
        let ctx = ToolContext {
            asker,
            ..ToolContext::new(".".into())
        };
        let mut messages = vec![Message::User("go".into())];

        turn(&provider, &tools, &ctx, &mut messages)
            .await
            .0
            .expect("turn completes");
        drop(ctx);

        assert_eq!(answering.await.expect("answers"), 1, "asked once for four");
    }

    #[tokio::test]
    async fn declining_the_doom_loop_stops_the_turn() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (asker, answering) = answering(Reply::Declined);
        let ctx = ToolContext {
            asker,
            ..ToolContext::new(".".into())
        };
        let mut messages = vec![Message::User("go".into())];

        let (result, _) = turn(&provider, &tools, &ctx, &mut messages).await;
        drop(ctx);
        answering.await.expect("answers");

        assert!(matches!(result, Err(Error::DoomLoop(name)) if name == "echo"));
        assert_eq!(
            messages[2..],
            ["1", "2", "3"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: failed(STOPPED),
            }),
            "every call is answered, so the session stays valid"
        );
    }

    #[tokio::test]
    async fn cancelling_at_the_doom_loop_question_interrupts() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let cancel = CancellationToken::new();
        // Cancels once asked, and never answers.
        let (asks, mut asked) = mpsc::channel::<nth_protocol::Ask>(1);
        let canceller = cancel.clone();
        let unanswered = tokio::spawn(async move {
            let ask = asked.recv().await.expect("asks");
            canceller.cancel();
            ask
        });
        let ctx = ToolContext {
            asker: Asker::new(asks),
            ..ToolContext::new(".".into())
        };
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = Turn {
            provider: &provider,
            route: ROUTE,
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spend(),
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await;
        let _ask = unanswered.await.expect("asked");

        assert!(matches!(result, Err(Error::Interrupted)));
        assert_eq!(
            messages[2..],
            ["1", "2", "3"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: failed(INTERRUPTED),
            }),
            "every call is answered, so the session stays valid"
        );
    }

    #[tokio::test]
    async fn with_nobody_to_ask_a_loop_stops_the_turn() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let mut messages = vec![Message::User("go".into())];

        let (result, _) = turn(&provider, &tools, &ctx, &mut messages).await;

        assert!(matches!(result, Err(Error::DoomLoop(name)) if name == "echo"));
        assert_eq!(
            messages[2..],
            ["1", "2", "3"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: failed(UNASKED),
            }),
            "stopped without blocking on a question nobody would answer"
        );
    }

    #[test]
    fn a_new_turn_starts_the_count_over() {
        let reply = |id: &str| {
            Message::Assistant(AssistantMessage {
                tool_calls: vec![call(id, "echo", "{}")],
                ..Default::default()
            })
        };
        let earlier = [reply("1"), reply("2")];
        let turn = [reply("3")];

        assert!(repeating(&earlier).is_none());
        assert!(
            repeating(&turn).is_none(),
            "the earlier turn is not counted"
        );
        assert_eq!(
            repeating(&[reply("1"), reply("2"), reply("3")]).map(|c| c.id.as_str()),
            Some("3")
        );
    }
}
