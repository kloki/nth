//! An app drawn into a test backend, and the tests of the app as a whole:
//! its layout and its loop. Each concern's own tests sit next to its code.

use std::{sync::Arc, time::Instant};

use crossterm::event::Event as TermEvent;
use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
use nth_context::{Context as ProjectContext, Paths};
use nth_protocol::{
    BoxError, Event, Listing, Message, Panel, Provider, Request, Screen, StreamEvent, ToolCall,
    Usage,
};
use nth_session::Session;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};

use super::{App, Queued, Tab, input::Input, keys::Action};
use crate::llm_picker::tests::model;

pub(crate) struct Idle;

impl Provider for Idle {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        _: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        async { Err("unused".into()) }.boxed()
    }
}

/// An idle app whose provider must never be asked for anything.
pub(crate) fn app() -> App {
    let session = Session::new("glm", "/repo".into());
    App::new(session, Arc::new(Idle), Arc::new(Vec::new()))
}

/// The context of a project in `dir` with one skill, `fix`, whose body
/// is `Fix $ARGUMENTS.`.
pub(crate) fn with_fix_skill(dir: &std::path::Path) -> Arc<ProjectContext> {
    let skill = dir.join(".agents/skills/fix");
    std::fs::create_dir_all(&skill).expect("dirs");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: fix\ndescription: fix what is broken\n---\nFix $ARGUMENTS.\n",
    )
    .expect("writes");
    Arc::new(ProjectContext::discover(dir, &Paths::default()))
}

pub(crate) fn buffer(app: &mut App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).expect("test backend");
    terminal.draw(|frame| app.draw(frame)).expect("draws");
    terminal.backend().buffer().clone()
}

/// The colour of the tab in the header whose text starts with `tab`.
pub(crate) fn tab_colour(app: &mut App, tab: &str) -> Color {
    let buffer = buffer(app);
    let row: Vec<&str> = (0..buffer.area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect();
    let x = (0..row.len())
        .find(|&x| row[x..].concat().starts_with(tab))
        .unwrap_or_else(|| panic!("no {tab:?} in {:?}", row.concat()));
    buffer[(u16::try_from(x).expect("fits"), 0)].fg
}

pub(crate) fn rows(app: &mut App) -> Vec<String> {
    let buffer = buffer(app);
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn prompt_and_status_rows_never_move() {
    let mut app = app();
    let idle = rows(&mut app);
    assert!(idle[0].starts_with(" [› chat] "), "tabs on the left");
    assert!(
        idle[0].ends_with(&format!(" nth {} ", env!("CARGO_PKG_VERSION"))),
        "right-aligned inside the margin"
    );
    assert!(
        [1, 8, 13].iter().all(|&i| idle[i].trim().is_empty()),
        "an empty line between bands"
    );
    assert_eq!(idle[9].trim_end(), " ▎ act");
    assert_eq!(idle[10].trim_end(), " ▎ Ask anything.");
    assert_eq!(idle[11].trim_end(), " ▎");
    assert_eq!(idle[12].trim_end(), " ▎");
    assert_eq!(idle[14].trim_end(), " glm · /repo");
    assert!(idle[15].trim().is_empty(), "no hint when idle");

    for i in 0..20 {
        app.chat.transcript.push_user(format!("message {i}"));
    }
    let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
    app.prompt.insert_str(&lines.join("\n"));
    app.busy_since = Some(Instant::now());
    let busy = rows(&mut app);

    assert_eq!(busy[0], idle[0], "the header stays on top");
    assert_eq!(busy[7].trim_end(), " ▎ message 19");
    let spinner = busy[9].chars().skip(3).take(4).collect::<String>();
    assert!(
        spinner.chars().all(|c| ('⠀'..='⣿').contains(&c)),
        "spinner in place of the mode: {:?}",
        busy[9]
    );
    assert!(busy[9].trim_end().ends_with("esc to cancel"));
    let text: Vec<&str> = busy[10..=12].iter().map(|r| r.trim_end()).collect();
    assert_eq!(
        text,
        [" ▎ line 8", " ▎ line 9", " ▎ line 10"],
        "scrolled to the cursor"
    );
    assert_eq!(busy[14].trim_end(), " glm · /repo", "no git outside a repo");
    assert!(busy[15].trim().is_empty(), "nothing below while busy");
}

/// The scrollbar column (the right margin) of the content panel's rows.
fn scrollbar(rows: &[String]) -> String {
    rows[2..=7]
        .iter()
        .map(|row| row.chars().nth(39).expect("40 wide"))
        .collect()
}

#[tokio::test]
async fn diagnostics_that_overflow_have_a_scrollbar() {
    let mut app = app();
    app.open_content(Tab::Diagnostics);
    let bar = scrollbar(&rows(&mut app));
    assert!(bar.starts_with('┃'), "at the top: {bar:?}");

    app.diagnostics.jump_bottom();
    let bar = scrollbar(&rows(&mut app));
    assert!(
        bar.ends_with('┃') && !bar.starts_with('┃'),
        "at the bottom: {bar:?}"
    );
}

#[test]
fn a_scrollbar_shows_only_while_scrolled_up() {
    let mut app = app();
    for i in 0..20 {
        app.chat.transcript.push_user(format!("message {i}"));
    }
    assert_eq!(scrollbar(&rows(&mut app)), " ".repeat(6), "following");

    app.chat.scroll_up(10);
    let up = rows(&mut app);
    let bar = scrollbar(&up);
    assert!(bar.contains('┃'), "{bar:?}");
    assert!(
        !bar.starts_with('┃') && !bar.ends_with('┃'),
        "partway: {bar:?}"
    );
    assert!(up[15].trim().is_empty(), "no hint in the status bar");

    app.chat.jump_top();
    assert!(scrollbar(&rows(&mut app)).starts_with('┃'));

    app.chat.jump_bottom();
    assert_eq!(scrollbar(&rows(&mut app)), " ".repeat(6), "following again");
}

#[test]
fn the_status_bar_shows_the_queue_on_its_second_line() {
    let mut app = app();
    app.queue = [
        Queued::Prompt("\nfix the build\nand the tests".into()),
        Queued::Prompt("then lint".into()),
    ]
    .into();
    let rows = rows(&mut app);

    assert_eq!(rows[14].trim_end(), " glm · /repo", "the first line stays");
    assert_eq!(rows[15].trim_end(), " ⏵ 2 queued · fix the build");
}

#[test]
fn a_narrow_status_line_cuts_the_right_first() {
    let mut app = app();
    app.git = Some(crate::git::GitStatus {
        branch: Some("a-very-long-branch-name".into()),
        modified: 1,
        ..Default::default()
    });
    app.busy_since = Some(Instant::now());
    let rows = rows(&mut app);

    let row = rows[14].trim();
    assert!(row.starts_with("glm · /repo git · a-very"), "{row:?}");
    assert!(!row.ends_with("*1"), "the counts are cut: {row:?}");
}

#[test]
fn the_context_bar_fills_with_usage() {
    let mut app = app();
    let place = rows(&mut app)[14].trim_end().to_string();
    assert!(
        place.ends_with("/repo"),
        "no bar while the window is unknown"
    );

    let mut glm = model("glm", true);
    glm.context = Some(1000);
    app.llms = Some(vec![glm].into());
    let empty = rows(&mut app);
    assert!(
        empty[14].starts_with(&format!("{place} {} ", " ".repeat(13))),
        "an empty bar before the first reply: {:?}",
        empty[14]
    );

    app.on_session(Event::Usage(Usage {
        input: 1000,
        output: 0,
    }));
    let full = rows(&mut app);
    assert!(full[14].starts_with(&format!("{place} {}", "⣿".repeat(13))));
}

#[tokio::test]
async fn tool_output_stays_in_the_transcript() {
    let mut app = app();
    app.chat.transcript.push_user("test it".into());
    let bash = ToolCall {
        id: "1".into(),
        name: "bash".into(),
        arguments: r#"{"command":"cargo test"}"#.into(),
    };
    app.on_session(Event::ToolStarted(bash.clone()));
    app.on_session(Event::ToolOutput {
        call_id: "1".into(),
        text: "running 3 tests\n".into(),
    });
    let running = rows(&mut app);

    assert_eq!(running[2].trim_end(), " ▎ test it");
    assert_eq!(running[4].trim_end(), " ▎ $ bash   cargo test");
    assert_eq!(running[5].trim_end(), " ▎   running 3 tests");

    app.on_session(Event::ToolFinished {
        call: bash,
        result: Ok(String::new()),
    });
    let finished = rows(&mut app);
    assert_eq!(finished[4].trim_end(), " ▎ $ bash   cargo test");
    assert_eq!(finished[5].trim_end(), " ▎   running 3 tests", "kept");
    assert!(
        app.git_loading.is_running(),
        "bash may have changed the tree"
    );
}

#[test]
fn exit_command_quits() {
    let mut app = app();
    app.prompt.insert_str("/exit");
    app.submit();
    assert!(app.quit);
}

#[tokio::test]
async fn clear_command_starts_a_fresh_session() {
    let mut app = app();
    let old = app.session.as_ref().expect("idle").id;
    app.session
        .as_mut()
        .expect("idle")
        .messages
        .push(Message::User("hi".into()));
    app.chat.transcript.push_user("hi".into());

    app.prompt.insert_str("/clear");
    app.submit();

    let session = app.session.as_ref().expect("idle");
    assert_ne!(session.id, old);
    assert_eq!(session.messages.len(), 1, "only the system prompt");
    assert!(app.chat.transcript.is_empty());
    assert!(app.prompt.is_empty());
    assert!(!app.is_busy());
}

#[tokio::test]
async fn tabs_open_switch_and_close() {
    let mut app = app();
    app.prompt.insert_str("/diagnostics");
    app.submit();
    let opened = rows(&mut app);
    assert!(
        opened[0].starts_with("  › chat [● diagnostics] "),
        "{opened:#?}"
    );
    assert!(opened[2].starts_with(" model "), "{:?}", opened[2]);
    assert!(opened[3].trim_start().starts_with("glm"));

    app.apply(Action::Content(0));
    assert_eq!(app.content.active(), Tab::Chat);
    app.apply(Action::CloseContent);
    assert_eq!(app.content.tabs().len(), 2, "the chat stays");
    app.apply(Action::NextContent);
    assert_eq!(app.content.active(), Tab::Diagnostics);

    app.apply(Action::CloseContent);
    assert_eq!(app.content.active(), Tab::Chat);
    assert!(rows(&mut app)[0].trim_end().starts_with(" [› chat] "));
    assert!(!rows(&mut app)[0].contains("diagnostics"));

    app.open_content(Tab::Diagnostics);
    app.prompt.insert_str("/close");
    app.submit();
    assert_eq!(app.content.tabs(), [Tab::Chat]);
}

#[tokio::test]
async fn tabs_switch_with_an_input_panel_open() {
    let mut app = app();
    app.open_content(Tab::Diagnostics);
    app.open_llm_picker();
    app.apply(Action::Content(0));
    assert_eq!(app.content.active(), Tab::Chat);
    assert!(matches!(app.input, Input::LlmPicker(_)), "the picker stays");
}

#[tokio::test]
async fn the_agent_switches_the_tab() {
    let mut app = app();
    let screen = Screen::new(app.screen_tx.clone());
    assert!(screen.show(Panel::Diagnostics).await);

    let panel = app.screen_rx.recv().await.expect("sent");
    app.open_content(panel.into());
    assert_eq!(app.content.active(), Tab::Diagnostics);
}

#[test]
fn a_large_paste_collapses_and_a_small_one_does_not() {
    let mut app = app();
    let pasted = (1..=5)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    app.on_terminal(TermEvent::Paste(pasted));

    assert_eq!(app.prompt.text(), "[pasted 5 lines] ");
    assert_eq!(rows(&mut app)[10].trim_end(), " ▎ [pasted 5 lines]");

    app.on_terminal(TermEvent::Paste("one\ntwo".into()));
    assert_eq!(
        app.prompt.text(),
        "[pasted 5 lines] one\ntwo",
        "a small paste goes in as is"
    );
}
