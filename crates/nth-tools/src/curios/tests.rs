use std::{collections::BTreeSet, process::Command};

use nth_protocol::{Curios, Screen, Show};

use super::*;

fn context() -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let mut ctx = ToolContext::new(dir.path().to_path_buf());
    ctx.session = "owner".into();
    ctx.curios = Curios::new(ctx.cwd.clone(), ctx.session.clone(), None, BTreeSet::new());
    (dir, ctx)
}

async fn publish(ctx: &ToolContext) -> Value {
    let paths: Value =
        serde_json::from_str(&CurioRead.call(json!({}), ctx).await.unwrap()).unwrap();
    let path = std::path::Path::new(paths["source_path"].as_str().unwrap()).join("review.html");
    assert!(std::path::Path::new(paths["template_path"].as_str().unwrap()).is_file());
    std::fs::write(
        &path,
        "<!doctype html><html><body><h1 id='title'>Review</h1></body></html>",
    )
    .unwrap();
    serde_json::from_str(
        &CurioPublish
            .call(json!({"source_path":path,"request_id":"publish"}), ctx)
            .await
            .unwrap(),
    )
    .unwrap()
}

fn comment(store: &core::Store, cid: &str, key: &str, body: String) -> core::Thread {
    store
        .comment(
            cid,
            core::Comment {
                request_id: key.into(),
                revision: 1,
                target: core::Target {
                    element_id: Some("title".into()),
                    kind: core::TargetKind::Element,
                    rect: None,
                    label: "Review title".into(),
                    quote: "Original context".into(),
                },
                body,
            },
        )
        .unwrap()
}

#[tokio::test]
async fn headless_publish_update_and_reply_are_idempotent_and_conflict_checked() {
    let (_dir, ctx) = context();
    let first = publish(&ctx).await;
    let cid = first["curio"]["id"].as_str().unwrap();
    let path = first["source_path"].as_str().unwrap();
    let identical = CurioPublish
        .call(json!({"source_path":path,"request_id":"publish"}), &ctx)
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&identical).unwrap(), first);
    let missing = CurioPublish
        .call(
            json!({"source_path":path,"curio_id":cid,"request_id":"missing"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(missing.starts_with("curios.invalid:"));
    let update =
        json!({"source_path":path,"curio_id":cid,"expected_revision":1,"request_id":"update"});
    let second: Value =
        serde_json::from_str(&CurioPublish.call(update.clone(), &ctx).await.unwrap()).unwrap();
    assert_eq!(second["revision"]["number"], 2);
    let mut stale = update;
    stale["request_id"] = json!("stale");
    assert!(
        CurioPublish
            .call(stale, &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.conflict:")
    );
    let store = ctx.curios.store().unwrap();
    let thread = comment(&store, cid, "comment", "Please revise".into());
    let reply = json!({"curio_id":cid,"thread_id":thread.id,"expected_version":thread.version,"request_id":"reply","body":"Revised","mark_addressed":true,"revision":2});
    let result = CurioReply.call(reply.clone(), &ctx).await.unwrap();
    assert_eq!(CurioReply.call(reply.clone(), &ctx).await.unwrap(), result);
    let mut stale = reply;
    stale["request_id"] = json!("stale-reply");
    assert!(
        CurioReply
            .call(stale, &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.conflict:")
    );
    let stored = store.snapshot(cid, None).unwrap();
    assert_eq!(stored.threads[0].status, core::Status::Addressed);
    assert_eq!(
        stored.threads[0].messages.last().unwrap().author,
        core::Author::Agent
    );
    let read: Value = serde_json::from_str(
        &CurioRead
            .call(
                json!({"action":"thread","curio_id":cid,"thread_id":thread.id}),
                &ctx,
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read["thread"]["target"]["quote"], "Original context");
    assert_eq!(read["page"]["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn rejects_untrusted_scope_agent_and_owner_arguments() {
    let (_dir, mut ctx) = context();
    ctx.session = "different".into();
    assert!(
        CurioRead
            .call(json!({}), &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.permission:")
    );
    ctx.session = "owner".into();
    ctx.is_subagent = true;
    assert!(
        CurioRead
            .call(json!({}), &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.permission:")
    );
    ctx.is_subagent = false;
    ctx.agent = Some(nth_context::Agent {
        name: "read-only".into(),
        mode: nth_context::agents::Mode::Primary,
        description: None,
        prompt: None,
        tools: Some(vec!["curio_read".into()]),
        denied: vec![],
        model: None,
        clerk: false,
        hidden: false,
        path: None,
        source: nth_context::agents::Source::Builtin,
        root: None,
    });
    assert!(
        CurioPublish
            .call(json!({"source_path":"a","request_id":"a"}), &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.permission:")
    );
    assert!(
        CurioRead
            .call(json!({"session_id":"other"}), &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.invalid:")
    );
    ctx.agent.as_mut().unwrap().tools = Some(vec!["show_user".into()]);
    assert!(
        crate::curios::store(&ctx, "show_user")
            .unwrap_err()
            .starts_with("curios.permission:")
    );
    ctx.agent.as_mut().unwrap().tools = None;
    ctx.agent.as_mut().unwrap().denied = vec!["curio_reply".into()];
    assert!(
        crate::curios::store(&ctx, "curio_reply")
            .unwrap_err()
            .starts_with("curios.permission:")
    );
}

#[tokio::test]
async fn read_pages_large_threads_and_immutable_batches_without_dumping_state() {
    let (_dir, ctx) = context();
    let first = publish(&ctx).await;
    let cid = first["curio"]["id"].as_str().unwrap();
    let store = ctx.curios.store().unwrap();
    let mut ids = Vec::new();
    for n in 0..18 {
        let t = comment(&store, cid, &format!("comment-{n}"), "é".repeat(8000));
        ids.push(t.messages[0].id.clone());
    }
    let sent = store
        .send(
            cid,
            core::Send {
                request_id: "send".into(),
                message_ids: ids,
            },
        )
        .unwrap();
    let mut cursor = Value::Null;
    let mut count = 0;
    loop {
        let mut a = json!({"action":"batch","batch_id":sent.id,"limit":100});
        if !cursor.is_null() {
            a["cursor"] = cursor;
        }
        let output = CurioRead.call(a, &ctx).await.unwrap();
        assert!(output.len() <= MAX_OUTPUT);
        let out: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(out["page"]["text_truncated"], true);
        count += out["page"]["items"].as_array().unwrap().len();
        cursor = out["page"]["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(count, 18);
    assert!(
        CurioRead
            .call(json!({"cursor":"oops"}), &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.invalid:")
    );
}

#[tokio::test]
async fn source_scope_and_symlinks_cannot_publish_outside_workspace() {
    let (dir, ctx) = context();
    let outside = dir.path().join("outside.html");
    std::fs::write(&outside, "<p>outside</p>").unwrap();
    let args = json!({"source_path":outside,"request_id":"escape"});
    assert!(
        CurioPublish
            .call(args, &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.invalid:")
    );
    #[cfg(unix)]
    {
        let store = ctx.curios.store().unwrap();
        let link = store.source_dir().join("link.html");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(
            CurioPublish
                .call(json!({"source_path":link,"request_id":"symlink"}), &ctx)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn show_user_validates_selection_and_sends_only_scoped_ids() {
    let (_dir, mut ctx) = context();
    let first = publish(&ctx).await;
    let cid = first["curio"]["id"].clone();
    for selection in [
        json!({}),
        json!({"filePath":"x","curio":{"curio_id":cid}}),
        json!({"curio":null}),
        json!({"curio":{"curio_id":cid,"session_id":"other"}}),
    ] {
        assert!(crate::ShowUser.call(selection, &ctx).await.is_err());
    }
    assert!(
        crate::ShowUser
            .call(json!({"curio":{"curio_id":cid}}), &ctx)
            .await
            .unwrap_err()
            .starts_with("curios.unavailable:")
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    ctx.screen = Screen::new(tx);
    crate::ShowUser
        .call(json!({"curio":{"curio_id":cid,"revision":1}}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        rx.recv().await,
        Some(Show::Curio {
            session_id: "owner".into(),
            curio_id: cid.as_str().unwrap().into(),
            revision: Some(1)
        })
    );
    assert!(
        crate::ShowUser
            .call(json!({"curio":{"curio_id":cid,"revision":99}}), &ctx)
            .await
            .is_err()
    );
}
