//! Persistence and filesystem boundaries exercised independently of the builders.
use nth_curios::Publish;
use nth_protocol::{Message, Writable};

struct ScopedWrite;

impl nth_protocol::Tool for ScopedWrite {
    fn spec(&self) -> nth_protocol::ToolSpec {
        nth_protocol::ToolSpec {
            name: "write",
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a nth_protocol::ToolContext,
    ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
        Box::pin(async move {
            let path = std::path::Path::new(args["path"].as_str().unwrap());
            ctx.writable.check(path)?;
            std::fs::write(path, "accepted source").map_err(|e| e.to_string())?;
            Ok("source written".into())
        })
    }
}

#[tokio::test]
async fn planning_turn_passes_only_owner_source_to_file_writer() {
    use nth_protocol::{FrontEnd, Mode, StreamEvent};

    use crate::agent_loop::tests::{Scripted, call};
    let repo = repository();
    let mut session = Session::new("fake", repo.path().to_path_buf());
    session.mode = Mode::Plan;
    let store = session.prepare_curios().unwrap();
    let source = session.curio_source_dir().unwrap().join("review.html");
    let private = store.path().join("state.json");
    let outside = repo.path().join("project.html");
    let events = [
        (&source, "source"),
        (&private, "private"),
        (&outside, "project"),
    ]
    .into_iter()
    .map(|(path, id)| {
        StreamEvent::ToolCall(call(
            id,
            "write",
            &serde_json::json!({"path": path}).to_string(),
        ))
    })
    .collect();
    let provider = Scripted::new(vec![events, vec![StreamEvent::TextDelta("Planned".into())]]);
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    session
        .prompt(
            "Draft review",
            &provider,
            &[Box::new(ScopedWrite)],
            &FrontEnd::default(),
            &tx,
            &crate::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(source).unwrap(), "accepted source");
    assert!(!outside.exists());
    assert!(session.messages.iter().any(|m| matches!(m, Message::ToolResult { call_id, content, .. } if call_id == "source" && content == "source written")));
    for denied in ["private", "project"] {
        assert!(session.messages.iter().any(|m| matches!(m, Message::ToolResult { call_id, content, .. } if call_id == denied && content.starts_with("Error: ") && content.contains("plan mode"))));
    }
}

#[tokio::test]
async fn subagent_and_restricted_primary_cannot_call_unoffered_curios_tools() {
    use nth_protocol::{FrontEnd, StreamEvent};

    use crate::agent_loop::tests::{Named, Scripted, call};
    let repo = repository();
    let agents = repo.path().join(".nth/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("review.md"),
        "---\ndescription: Read only\nmode: primary\ntools: read\n---\nReview only.\n",
    )
    .unwrap();
    let context = std::sync::Arc::new(nth_context::Context::discover(
        repo.path(),
        &nth_context::Paths::default(),
    ));
    for subagent in [false, true] {
        let mut session =
            Session::new("fake", repo.path().to_path_buf()).with_context(context.clone());
        if subagent {
            session = session.as_subagent(None);
        } else {
            session.set_agent(Some("review".into()));
        }
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("forged", "curio_publish", "{}"))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        session
            .prompt(
                "Review",
                &provider,
                &[Box::new(Named("curio_publish"))],
                &FrontEnd::default(),
                &tx,
                &crate::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            provider
                .tools
                .lock()
                .unwrap()
                .iter()
                .all(|offered| !offered.iter().any(|name| name == "curio_publish"))
        );
        assert!(session.messages.iter().any(|m| matches!(m, Message::ToolResult { call_id, content, .. } if call_id == "forged" && content.starts_with("Error: "))), "forged call succeeded for subagent={subagent}: {:?}", session.messages);
    }
}

use crate::{Session, Store};

fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    dir
}

#[tokio::test]
async fn persisted_root_survives_managed_move_and_distinguishes_separate_checkout() {
    let repo = repository();
    let other = repository();
    let mut owner = Session::new("fake", repo.path().to_path_buf());
    let store = owner.prepare_curios().unwrap();
    let published = store
        .publish(Publish {
            request_id: "publish".into(),
            curio_id: None,
            expected_revision: None,
            title: Some("Owned".into()),
            html: "<h1>Owned</h1>".into(),
        })
        .unwrap();
    let source = owner.curio_source_dir().unwrap();
    // A real second repository prevents accidental rediscovery from passing.
    owner.set_cwd(other.path().to_path_buf(), Some(repo.path().to_path_buf()));
    owner.messages.push(Message::User("Persist owner".into()));
    let saved = Store::at(repo.path().join("session-fixtures"));
    saved.save(&owner).await.unwrap();
    let resumed = saved.load(owner.id).await.unwrap();
    assert_eq!(resumed.curio_source_dir().unwrap(), source);
    assert_eq!(
        resumed
            .curios()
            .store()
            .unwrap()
            .snapshot(&published.curio.id, None)
            .unwrap()
            .revision
            .title,
        "Owned"
    );
    let mut independent = Session::new("fake", other.path().to_path_buf());
    assert_eq!(
        independent.prepare_curios().unwrap().repo_root(),
        other.path()
    );
    assert!(
        independent
            .curios()
            .store()
            .unwrap()
            .snapshot(&published.curio.id, None)
            .is_err()
    );
    assert!(resumed.clone().as_subagent(None).curios().store().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn current_source_permission_rejects_existing_file_and_directory_symlinks() {
    use std::os::unix::fs::symlink;
    let repo = repository();
    let mut owner = Session::new("fake", repo.path().to_path_buf());
    owner.prepare_curios().unwrap();
    let source = owner.curio_source_dir().unwrap();
    let private = source.parent().unwrap().join("state.json");
    std::fs::write(&private, "private sentinel").unwrap();
    symlink(&private, source.join("existing.html")).unwrap();
    symlink(source.parent().unwrap(), source.join("linked-parent")).unwrap();
    let writable = Writable::Plan {
        file: owner.plan_path(),
        memory: None,
        source: Some(source.clone()),
    };
    assert!(writable.check(&source.join("nested/new.html")).is_ok());
    for path in [
        source.join("existing.html"),
        source.join("linked-parent/state.json"),
        source.join("../state.json"),
        source
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("another-owner/source/new.html"),
    ] {
        assert!(
            writable.check(&path).is_err(),
            "unexpected write permission: {}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(private).unwrap(),
        "private sentinel"
    );
}
