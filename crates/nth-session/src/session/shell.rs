use std::time::SystemTime;

use nth_protocol::{AssistantMessage, Event, Message, Tool, ToolCall};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{SHELL_PROMPT, Session};
use crate::{Error, FrontEnd, agent_loop::run_call};

impl Session {
    /// Runs `command` on `shell` for the user, without the model. It lands
    /// in the history as a bash call the model made, after a user message
    /// saying the user ran it, so the model sees it on its next turn.
    /// It runs untimed in the session's tool context for what bash reads:
    /// the working directory, the session id (one browser with the model's
    /// commands), the extra directories and the state root.
    /// Cancelling kills the command and still leaves `messages` valid.
    pub async fn shell(
        &mut self,
        command: impl Into<String>,
        shell: &dyn Tool,
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let call = ToolCall {
            id: format!("shell-{}", Uuid::new_v4()),
            name: shell.spec().name.into(),
            arguments: serde_json::json!({ "command": command.into() }).to_string(),
        };
        self.push_user(Vec::new(), SHELL_PROMPT.into());
        self.messages.push(Message::Assistant(AssistantMessage {
            tool_calls: vec![call.clone()],
            ..AssistantMessage::default()
        }));
        self.updated_at = SystemTime::now();
        let mut ctx = self.tool_context(front_end);
        // No tool ceiling: you watch your own command and stop it yourself,
        // so a long build is never cut off.
        ctx.tool_timeout = None;
        let answer = run_call(Some(shell), &ctx, &call, events, cancel).await;
        self.messages.push(answer);
        match cancel.is_cancelled() {
            true => Err(Error::Interrupted),
            false => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use nth_protocol::{INTERRUPTED, ToolContext, failed};

    use super::*;

    /// Says what it was asked to run, or never finishes on `hang`.
    struct Shell;

    impl Tool for Shell {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "bash",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            use futures::FutureExt;
            async move {
                let command = args["command"].as_str().unwrap_or_default().to_string();
                if command == "hang" {
                    std::future::pending::<()>().await;
                }
                if command == "slow" {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                ctx.output.send(command.clone()).await;
                Ok(format!("ran {command}"))
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn a_shell_command_lands_as_a_bash_call() {
        let mut session = Session::new("glm-5.3", ".".into());
        let (events, mut rx) = mpsc::channel(16);

        let result = session
            .shell(
                "ls",
                &Shell,
                &FrontEnd::default(),
                &events,
                &CancellationToken::new(),
            )
            .await;

        assert!(result.is_ok());
        let [
            _,
            Message::User(said),
            Message::Assistant(reply),
            Message::ToolResult {
                call_id, content, ..
            },
        ] = &session.messages[..]
        else {
            panic!("unexpected messages: {:?}", session.messages);
        };
        assert_eq!(said, SHELL_PROMPT);
        assert_eq!(reply.tool_calls[0].name, "bash");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(call_id, &reply.tool_calls[0].id);
        assert_eq!(content, "ran ls");
        assert_eq!(session.title().as_deref(), Some("!ls"));
        assert!(matches!(rx.recv().await, Some(Event::ToolStarted(_))));
        assert!(matches!(rx.recv().await, Some(Event::ToolOutput { .. })));
        assert!(matches!(rx.recv().await, Some(Event::ToolFinished { .. })));
    }

    #[tokio::test]
    async fn a_shell_command_after_a_failed_turn_joins_the_dangling_prompt() {
        use crate::agent_loop::tests::{Failure, Reply, Scripted};

        let mut session = Session::new("glm-5.3", ".".into());
        let (events, _rx) = mpsc::channel(64);
        let failed = session
            .prompt(
                "first",
                &Scripted::always(Reply::Refused(Failure::Down)),
                &[],
                &FrontEnd::default(),
                &events,
                &CancellationToken::new(),
            )
            .await;
        assert!(failed.is_err());

        session
            .shell(
                "ls",
                &Shell,
                &FrontEnd::default(),
                &events,
                &CancellationToken::new(),
            )
            .await
            .expect("runs");

        let [
            _,
            Message::User(said),
            Message::Assistant(_),
            Message::ToolResult { .. },
        ] = &session.messages[..]
        else {
            panic!("unexpected messages: {:?}", session.messages);
        };
        assert_eq!(said, &format!("first\n\n{SHELL_PROMPT}"));
        assert_eq!(session.title().as_deref(), Some("first"));
    }

    /// Says which session, directories and state root it ran for.
    struct Where;

    impl Tool for Where {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "bash",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            let said = serde_json::json!({
                "session": ctx.session,
                "extra_dirs": ctx.extra_dirs,
                "state_root": ctx.state_root,
                "timeout": ctx.tool_timeout.is_some(),
            });
            Box::pin(async move { Ok(said.to_string()) })
        }
    }

    #[tokio::test]
    async fn a_shell_command_runs_in_the_sessions_context() {
        let mut session =
            Session::new("glm-5.3", "/repo".into()).with_extra_dirs(vec!["/docs".into()]);
        let (events, _rx) = mpsc::channel(16);

        session
            .shell(
                "pwd",
                &Where,
                &FrontEnd::default(),
                &events,
                &CancellationToken::new(),
            )
            .await
            .expect("runs");

        let Some(Message::ToolResult { content, .. }) = session.messages.last() else {
            panic!("unexpected messages: {:?}", session.messages);
        };
        let said: serde_json::Value = serde_json::from_str(content).expect("json");
        assert_eq!(said["session"], session.id.to_string());
        assert_eq!(said["extra_dirs"], serde_json::json!(["/docs"]));
        assert_eq!(
            said["state_root"],
            serde_json::json!(session.state_root()),
            "plans and work stay where the model's tools find them"
        );
        assert_eq!(said["timeout"], false, "you stop your own command");
    }

    #[tokio::test]
    async fn a_shell_command_outlasts_the_tool_ceiling() {
        let mut session = Session::new("glm-5.3", ".".into());
        session.env.tool_timeout = Some(Duration::from_millis(10));
        let (events, _rx) = mpsc::channel(16);

        let result = session
            .shell(
                "slow",
                &Shell,
                &FrontEnd::default(),
                &events,
                &CancellationToken::new(),
            )
            .await;

        assert!(result.is_ok());
        assert!(matches!(
            session.messages.last(),
            Some(Message::ToolResult { content, .. }) if content == "ran slow"
        ));
    }

    #[tokio::test]
    async fn cancelling_a_shell_command_answers_its_call() {
        let mut session = Session::new("glm-5.3", ".".into());
        let (events, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = session
            .shell("hang", &Shell, &FrontEnd::default(), &events, &cancel)
            .await;

        assert!(matches!(result, Err(Error::Interrupted)));
        assert!(matches!(
            session.messages.last(),
            Some(Message::ToolResult { content, .. }) if *content == failed(INTERRUPTED)
        ));
    }
}
