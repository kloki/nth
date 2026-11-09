//! A turn that moves the session to another directory.

use super::*;

#[tokio::test]
async fn a_turn_that_moved_the_session_moves_the_app() {
    let mut app = crate::app::tests::test_app();
    let mut session = app.session.take().expect("idle");
    let original_plan = session.plan_path();
    let worktree = app.cwd.join(".nth/worktrees/x");
    session.set_cwd(worktree.clone(), Some(app.cwd.clone()));

    app.end_turn(Ended {
        session,
        result: Ok(()),
        saved: Ok(()),
        acknowledged: Ok(()),
        ran: Ran::Turn,
    });

    assert_eq!(app.cwd, worktree);
    assert!(app.place.ends_with(".nth/worktrees/x"), "{}", app.place);
    let plan = app.session.as_ref().expect("back").plan_path();
    assert_eq!(plan, original_plan);
    assert_eq!(app.plan.path(), plan);
}

#[tokio::test]
async fn the_status_bar_follows_a_move_while_the_turn_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    let worktree = root.join(".nth/worktrees/x");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "-c",
        "user.name=nth",
        "-c",
        "user.email=nth@example.com",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "x",
        &worktree.to_string_lossy(),
    ]);
    let mut app = crate::app::tests::test_app();
    // Mid-turn the session is in the turn's task.
    app.session = None;

    app.on_session(Event::Moved(worktree.clone()));
    // The pull request loads beside the status, and may end first.
    while app.git.loading.is_running() {
        let loaded = app.git.next().await;
        app.git_finished(loaded);
    }

    assert_eq!(app.cwd, worktree);
    assert!(app.place.ends_with(".nth/worktrees/x"), "{}", app.place);
    // What the status bar's git summary shows.
    let branch = app
        .git
        .status
        .as_ref()
        .and_then(|git| git.branch.as_deref());
    assert_eq!(branch, Some("x"));
}
