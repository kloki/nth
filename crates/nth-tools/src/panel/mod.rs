use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Panel as Shown, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

/// Switches the content panel the user sees.
pub struct Panel;

#[derive(Deserialize)]
struct Args {
    panel: Shown,
}

impl Tool for Panel {
    fn spec(&self) -> ToolSpec {
        let names: Vec<&str> = Shown::ALL.iter().map(|p| p.name()).collect();
        ToolSpec {
            name: "panel",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "panel": { "type": "string", "enum": names, "description": "The panel to show" }
                },
                "required": ["panel"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let Args { panel } = crate::parse_args(args)?;
            if ctx.screen.show(panel).await {
                Ok(format!("The user now sees the {} panel.", panel.name()))
            } else {
                Err("there is no screen in this session to show a panel on".into())
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Screen;
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn sends_the_panel_to_the_screen() {
        let (tx, mut rx) = mpsc::channel(1);
        let ctx = ToolContext {
            screen: Screen::new(tx),
            ..ToolContext::new(".".into())
        };

        let out = Panel.call(json!({ "panel": "diagnostics" }), &ctx).await;

        assert_eq!(out, Ok("The user now sees the diagnostics panel.".into()));
        assert_eq!(rx.recv().await, Some(Shown::Diagnostics));
    }

    #[tokio::test]
    async fn rejects_an_unknown_panel() {
        let out = Panel
            .call(json!({ "panel": "plan" }), &ToolContext::new(".".into()))
            .await;

        assert!(out.expect_err("unknown").contains("invalid arguments"));
    }

    #[tokio::test]
    async fn tells_the_model_when_there_is_no_screen() {
        let out = Panel
            .call(json!({ "panel": "chat" }), &ToolContext::new(".".into()))
            .await;

        assert!(out.expect_err("headless").contains("no screen"));
    }
}
