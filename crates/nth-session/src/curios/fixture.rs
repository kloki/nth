//! A repository with a session that published one curio, for the tests.
use nth_curios::{Comment, Curio, Publish, Send, Store, Target, TargetKind};

use crate::Session;

/// A fresh repository and a session in it that published one curio.
pub(super) fn published() -> (tempfile::TempDir, Session, Store, Curio) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("fake", repo.path().to_path_buf());
    let store = session.prepare_curios().unwrap();
    let curio = publish(&store, "Review");
    (repo, session, store, curio)
}

pub(super) fn publish(store: &Store, title: &str) -> Curio {
    store
        .publish(Publish {
            request_id: format!("publish-{}", title.to_lowercase()),
            curio_id: None,
            expected_revision: None,
            title: Some(title.into()),
            html: format!("<h1 id=title>{title}</h1>"),
        })
        .unwrap()
        .curio
}

/// Comments on `curio` and sends the comment, as the viewer does.
pub(super) fn send(store: &Store, curio: &Curio) {
    let thread = store
        .comment(
            &curio.id,
            Comment {
                request_id: "comment".into(),
                revision: 1,
                target: Target {
                    element_id: None,
                    kind: TargetKind::Document,
                    rect: None,
                    label: "Review".into(),
                    quote: "".into(),
                },
                body: "revise this".into(),
            },
        )
        .unwrap();
    store
        .send(
            &curio.id,
            Send {
                request_id: "send".into(),
                message_ids: vec![thread.messages[0].id.clone()],
            },
        )
        .unwrap();
}
