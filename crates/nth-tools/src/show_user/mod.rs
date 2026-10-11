//! Puts a file in front of the user, in a tab that follows it on disk.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

pub struct ShowUser;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    file_path: String,
}

impl Tool for ShowUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "show_user",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "The path to the file to show" }
                },
                "required": ["filePath"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let Args { file_path } = crate::parse_args(args)?;
            let path = ctx.cwd.join(&file_path);
            match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => return Err(format!("{} is not a file", path.display())),
                Err(e) => return Err(format!("cannot show {}: {e}", path.display())),
            }
            // The tab outlives the turn, so it must not depend on where the
            // session's working directory moves next.
            let path = std::path::absolute(&path).map_err(|e| e.to_string())?;
            if ctx.screen.show_file(path.clone()).await {
                Ok(format!("The user now sees {} in a tab.", path.display()))
            } else {
                Err("there is no screen in this session to show a file on".into())
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Screen, Show};
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn sends_the_file_to_the_screen_by_its_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("notes.md"), "# Notes\n").expect("writes");
        let (tx, mut rx) = mpsc::channel(1);
        let ctx = ToolContext {
            screen: Screen::new(tx),
            ..ToolContext::new(dir.path().to_path_buf())
        };

        let out = ShowUser.call(json!({ "filePath": "notes.md" }), &ctx).await;

        let path = dir.path().join("notes.md");
        assert_eq!(
            out,
            Ok(format!("The user now sees {} in a tab.", path.display()))
        );
        assert_eq!(rx.recv().await, Some(Show::File(path)));
    }

    #[tokio::test]
    async fn refuses_what_is_not_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let missing = ShowUser.call(json!({ "filePath": "gone.rs" }), &ctx).await;
        assert!(missing.expect_err("missing").contains("cannot show"));

        let folder = ShowUser.call(json!({ "filePath": "." }), &ctx).await;
        assert!(folder.expect_err("folder").contains("is not a file"));
    }

    #[tokio::test]
    async fn tells_the_model_when_there_is_no_screen() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").expect("writes");

        let out = ShowUser
            .call(
                json!({ "filePath": "a.rs" }),
                &ToolContext::new(dir.path().to_path_buf()),
            )
            .await;

        assert!(out.expect_err("headless").contains("no screen"));
    }
}
