use nth_protocol::{FrontEnd, Message, StreamEvent, Tool, ToolContext};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{Session, session::tests::send};

#[test]
fn plan_lives_under_dot_nth() {
    let id = Uuid::nil();
    assert_eq!(
        plan_path("/repo".as_ref(), &id),
        PathBuf::from(format!("/repo/.nth/plans/{id}.md"))
    );
}

#[test]
fn plan_mode_says_where_the_plan_goes_and_who_approves() {
    let plan = Path::new("/repo/.nth/plans/1.md");

    let fresh = plan_mode_reminder(plan, false, Approver::User, None);
    assert!(fresh.starts_with("<system-reminder>\nPlan mode is active."));
    assert!(fresh.contains(
        "No plan file exists yet. You should create your plan at /repo/.nth/plans/1.md using the write tool."
    ));
    assert!(fresh.contains("/approve"));
    // The guidance nth adds on top of opencode's text.
    assert!(fresh.contains("Always plan to work in a worktree"));
    assert!(fresh.contains("enter_worktree"));
    assert!(fresh.contains("the `work-plan` skill"));
    assert!(fresh.contains("work plan in the plan file as a tree"));
    assert!(fresh.contains("open a pull request with `gh` on GitHub or `tea` on Forgejo"));
    assert!(!fresh.contains('{'), "every placeholder filled");

    assert!(!fresh.contains("memory folder"));

    let again = plan_mode_reminder(plan, true, Approver::Nobody, None);
    assert!(again.contains("A plan file already exists at /repo/.nth/plans/1.md."));
    assert!(!again.contains("/approve"));
}

#[test]
fn plan_mode_lets_the_memory_be_written() {
    let plan = Path::new("/repo/.nth/plans/1.md");
    let memory = Path::new("/repo/.nth/memory");

    let reminder = plan_mode_reminder(plan, false, Approver::User, Some(memory));

    assert!(
        reminder.contains(
            "may ONLY read/inspect. The one other place you may write is the project's memory folder, /repo/.nth/memory:"
        ),
        "{reminder}"
    );
    assert!(!reminder.contains('{'), "every placeholder filled");
}

#[test]
fn approval_names_the_plan() {
    assert_eq!(
        approved(Path::new("/p.md")),
        "The plan at /p.md has been approved, you can now edit files. Execute the plan"
    );
}

#[test]
fn build_switch_points_at_an_existing_plan() {
    let plan = Path::new("/p.md");
    assert!(build_switch_reminder(plan, false).ends_with("as needed.\n</system-reminder>"));
    assert!(build_switch_reminder(plan, true).ends_with(
        "A plan file exists at /p.md. You should execute on the plan defined within it"
    ));
}

#[tokio::test]
async fn ensure_dir_writes_a_gitignore_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    ensure_dir(dir.path()).await.expect("creates .nth");

    let gitignore = dir.path().join(".nth/.gitignore");
    assert_eq!(
        std::fs::read_to_string(&gitignore).expect("reads"),
        GITIGNORE
    );

    // The user's own rules win over nth's.
    std::fs::write(&gitignore, "plans/\ncustom\n").expect("writes");
    ensure_dir(dir.path()).await.expect("leaves it alone");
    assert_eq!(
        std::fs::read_to_string(&gitignore).expect("reads"),
        "plans/\ncustom\n"
    );
}

/// Says whether the file it is asked about may be written.
struct Probe;

impl Tool for Probe {
    fn spec(&self) -> nth_protocol::ToolSpec {
        nth_protocol::ToolSpec {
            name: "probe",
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
        let path = ctx.cwd.join(args["path"].as_str().unwrap_or_default());
        let checked = ctx.writable.check(&path).map(|()| "writable".to_string());
        Box::pin(async move { checked })
    }
}

#[tokio::test]
async fn plan_mode_is_announced_once_and_its_end_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut session = Session::new("glm-5.3", dir.path().to_path_buf());
    session.mode = Mode::Plan;
    let plan = session.plan_path();

    let first = send(&mut session, "plan it").await;
    assert!(
        first.starts_with("plan it\n\n<system-reminder>\nPlan mode is active."),
        "{first}"
    );
    assert!(first.contains(&format!("create your plan at {}", plan.display())));
    assert!(first.contains("Nobody can approve it"), "headless: {first}");
    assert_eq!(send(&mut session, "more").await, "more");
    assert_eq!(session.title().as_deref(), Some("plan it"));

    std::fs::create_dir_all(plan.parent().expect("dir")).expect("dirs");
    std::fs::write(&plan, "# Plan").expect("writes");
    session.mode = Mode::Act;
    let switched = send(&mut session, "go").await;
    assert!(
        switched.starts_with(
            "go\n\n<system-reminder>\nYour operational mode has changed from plan to act."
        ),
        "{switched}"
    );
    assert!(switched.ends_with(&format!(
        "A plan file exists at {}. You should execute on the plan defined within it",
        plan.display()
    )));
    assert_eq!(send(&mut session, "next").await, "next");
}

#[tokio::test]
async fn plan_mode_keeps_a_gitignore_in_the_project_nth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut session = Session::new("glm-5.3", dir.path().to_path_buf());
    session.mode = Mode::Plan;

    send(&mut session, "plan it").await;

    let gitignore = std::fs::read_to_string(dir.path().join(".nth/.gitignore")).expect("reads");
    assert!(gitignore.contains("plans/"), "{gitignore}");
    assert!(gitignore.contains("work/"), "{gitignore}");
    assert!(gitignore.contains(".gitignore"), "{gitignore}");
}

#[tokio::test]
async fn act_mode_from_the_start_says_nothing() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    assert_eq!(send(&mut session, "go").await, "go");
}

#[tokio::test]
async fn plan_mode_lets_tools_write_only_the_plan() {
    use crate::agent_loop::tests::{Scripted, call};

    let dir = tempfile::tempdir().expect("tempdir");
    let mut session = Session::new("glm-5.3", dir.path().to_path_buf());
    session.mode = Mode::Plan;
    let plan = format!(".nth/plans/{}.md", session.id);
    let provider = Scripted::new(vec![
        vec![
            StreamEvent::ToolCall(call("1", "probe", r#"{"path":"src/main.rs"}"#)),
            StreamEvent::ToolCall(call("2", "probe", &format!(r#"{{"path":"{plan}"}}"#))),
        ],
        vec![StreamEvent::TextDelta("done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Probe)];
    let (tx, _rx) = mpsc::channel(64);

    session
        .prompt(
            "plan",
            &provider,
            &tools,
            &FrontEnd::default(),
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("turn completes");

    let results: Vec<&str> = session
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        results[0].starts_with("Error: plan mode is active"),
        "{}",
        results[0]
    );
    assert_eq!(results[1], "writable");
}
