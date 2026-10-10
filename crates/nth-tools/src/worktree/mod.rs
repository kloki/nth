//! Moving the session into a git worktree under `.nth/worktrees` and back.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, Writable};
use nth_worktree::Created;
use serde::Deserialize;
use serde_json::json;

/// Creates the worktree on first use and moves the session into it.
pub struct EnterWorktree;

/// Moves the session back to where it entered the worktree from.
pub struct ExitWorktree;

#[derive(Deserialize)]
struct Args {
    name: String,
}

impl Tool for EnterWorktree {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "enter_worktree",
            description: include_str!("enter.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The worktree's name, which is also its branch" }
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
            let Args { name } = crate::parse_args(args)?;
            refuse_in_plan_mode(ctx)?;
            // From one worktree into another, the new one still goes in the
            // main checkout.
            let origin = ctx.workdir.origin().unwrap_or_else(|| ctx.cwd.clone());
            let root = nth_worktree::toplevel(&origin).await?;
            let worktree = nth_worktree::open(&root, &name).await?;
            let created = match worktree.created {
                None => "",
                Some(Created::NewBranch) => "Created it on a new branch from HEAD. ",
                Some(Created::ExistingBranch) => "Created it on the existing branch. ",
            };
            let path = worktree.path;
            ctx.workdir.enter(&ctx.cwd, path.clone());
            let back = ctx.workdir.origin().unwrap_or(origin);
            Ok(format!(
                "Entered the worktree at {} on branch {name}. {created}\
                 The working directory is now that worktree: relative paths, \
                 glob, grep and bash run there from your next step on. Call \
                 exit_worktree to go back to {}.",
                path.display(),
                back.display(),
            ))
        }
        .boxed()
    }
}

impl Tool for ExitWorktree {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_worktree",
            description: include_str!("exit.txt").into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn call<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            refuse_in_plan_mode(ctx)?;
            let Some(origin) = ctx.workdir.exit() else {
                return Err("the session is not in a worktree".into());
            };
            Ok(format!(
                "Back in {}. The worktree at {} and its branch are kept; \
                 `git worktree remove <path>` deletes the worktree.",
                origin.display(),
                ctx.cwd.display(),
            ))
        }
        .boxed()
    }
}

/// The plan file lives under the working directory, so moving would lose
/// it.
fn refuse_in_plan_mode(ctx: &ToolContext) -> Result<(), String> {
    match ctx.writable {
        Writable::Only(_) => Err(
            "the working directory cannot change in plan mode; it can once the plan is approved"
                .into(),
        ),
        Writable::Any => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use nth_protocol::Workdir;

    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit, canonical so it compares with what
    /// git prints.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        run(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "a").expect("write");
        run(&root, &["add", "."]);
        run(
            &root,
            &[
                "-c",
                "user.name=nth",
                "-c",
                "user.email=nth@example.com",
                "commit",
                "-qm",
                "init",
            ],
        );
        (dir, root)
    }

    async fn enter(ctx: &ToolContext, name: &str) -> ToolResult {
        EnterWorktree.call(json!({ "name": name }), ctx).await
    }

    #[tokio::test]
    async fn creates_an_ignored_worktree_on_its_own_branch_and_moves_there() {
        let (_dir, root) = repo();
        let ctx = ToolContext::new(root.clone());

        let out = enter(&ctx, "feature").await.expect("entered");

        let path = root.join(".nth/worktrees/feature");
        assert!(out.contains("new branch"), "{out}");
        assert_eq!(ctx.workdir.moved_to(), Some(path.clone()));
        assert_eq!(ctx.workdir.origin(), Some(root.clone()));
        assert_eq!(run(&path, &["branch", "--show-current"]), "feature");
        assert_eq!(run(&root, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn enters_an_existing_worktree_without_creating_it_again() {
        let (_dir, root) = repo();
        enter(&ToolContext::new(root.clone()), "x")
            .await
            .expect("first");
        let ctx = ToolContext::new(root.clone());

        let out = enter(&ctx, "x").await.expect("second");

        assert!(!out.contains("Created"), "{out}");
        assert_eq!(ctx.workdir.moved_to(), Some(root.join(".nth/worktrees/x")));
    }

    #[tokio::test]
    async fn a_second_worktree_goes_in_the_main_checkout() {
        let (_dir, root) = repo();
        let first = root.join(".nth/worktrees/a");
        let ctx = ToolContext {
            workdir: Workdir::new(Some(root.clone())),
            ..ToolContext::new(first.clone())
        };
        enter(&ToolContext::new(root.clone()), "a")
            .await
            .expect("a");

        enter(&ctx, "b").await.expect("b");

        assert_eq!(ctx.workdir.moved_to(), Some(root.join(".nth/worktrees/b")));
        assert_eq!(ctx.workdir.origin(), Some(root));
    }

    #[tokio::test]
    async fn exit_goes_back_to_the_origin() {
        let (_dir, root) = repo();
        let ctx = ToolContext {
            workdir: Workdir::new(Some(root.clone())),
            ..ToolContext::new(root.join(".nth/worktrees/a"))
        };

        let out = ExitWorktree.call(json!({}), &ctx).await.expect("exited");

        assert!(
            out.starts_with(&format!("Back in {}", root.display())),
            "{out}"
        );
        assert_eq!(ctx.workdir.moved_to(), Some(root));
        assert_eq!(ctx.workdir.origin(), None);
    }

    #[tokio::test]
    async fn exit_outside_a_worktree_is_refused() {
        let out = ExitWorktree
            .call(json!({}), &ToolContext::new(".".into()))
            .await;

        assert!(out.expect_err("refused").contains("not in a worktree"));
    }

    #[tokio::test]
    async fn refuses_in_plan_mode() {
        let (_dir, root) = repo();
        let ctx = ToolContext {
            writable: Writable::Only(root.join("plan.md")),
            ..ToolContext::new(root)
        };

        assert!(
            enter(&ctx, "x")
                .await
                .expect_err("plan")
                .contains("plan mode")
        );
        assert_eq!(ctx.workdir.moved_to(), None);
    }
}
