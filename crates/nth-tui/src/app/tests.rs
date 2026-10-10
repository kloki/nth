//! An app drawn into a test backend, and the tests of the app as a whole:
//! its layout and its loop. Each concern's own tests sit next to its code.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crossterm::event::Event as TermEvent;
use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
use nth_context::{Context as ProjectContext, Paths};
use nth_protocol::{
    BoxError, Effort, Event, Listing, Message, Panel, Provider, Request, Screen, StreamEvent,
    ToolCall, Usage,
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
fn an_empty_chat_shows_the_field_until_something_is_said() {
    let mut app = app();
    assert!(app.showing_hero(), "it keeps moving");
    let content = |app: &mut App| rows(app)[2..8].concat();
    assert!(
        content(&mut app).chars().any(|c| !c.is_whitespace()),
        "the field fills the content panel"
    );

    app.chat.apply(&Event::TextDelta("hello".into()));
    assert!(!app.showing_hero());
    let after = content(&mut app);
    assert!(after.contains("hello"), "{after:?}");
    assert!(
        rows(&mut app)[3..8].iter().all(|row| row.trim().is_empty()),
        "no field left behind"
    );
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
    assert_eq!(idle[9].trim_end(), " ▎ act  · glm");
    assert_eq!(idle[10].trim_end(), " ▎ Ask anything.");
    assert_eq!(idle[11].trim_end(), " ▎");
    assert_eq!(idle[12].trim_end(), " ▎");
    assert_eq!(idle[14].trim_end(), " /repo 0m");
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
    // Byte offsets shift with the spinner's multi-byte glyphs; count chars.
    let at = |row: &str| row.find("· glm").map(|at| row[..at].chars().count());
    assert_eq!(at(&busy[9]), at(&idle[9]), "the model stays put while busy");
    let text: Vec<&str> = busy[10..=12].iter().map(|r| r.trim_end()).collect();
    assert_eq!(
        text,
        [" ▎ line 8", " ▎ line 9", " ▎ line 10"],
        "scrolled to the cursor"
    );
    assert_eq!(busy[14].trim_end(), " /repo 0m", "no git outside a repo");
    assert!(busy[15].trim().is_empty(), "nothing below while busy");
}

#[test]
fn the_prompt_shows_the_model_with_its_provider_and_effort_coloured() {
    let mut app = app();
    app.model = "opencode/glm-5.3".into();
    app.effort = Effort::High;
    let buffer = buffer(&mut app);
    let row: String = (0..buffer.area.width)
        .map(|x| buffer[(x, 9)].symbol())
        .collect();

    assert_eq!(row.trim_end(), " ▎ act  · opencode/glm-5.3 · high");
    let fg = |text: &str| {
        let at = row.find(text).expect(text);
        buffer[(u16::try_from(row[..at].chars().count()).expect("fits"), 9)].fg
    };
    assert_eq!(fg("opencode/"), Color::White, "the provider is bright");
    assert_eq!(fg("glm-5.3"), Color::Blue);
    assert_eq!(fg("high"), Color::Gray);
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

    assert_eq!(rows[14].trim_end(), " /repo 0m", "the first line stays");
    assert_eq!(rows[15].trim_end(), " ⏵ 2 queued · fix the build");
}

#[test]
fn the_status_bar_shows_the_session_time() {
    let mut app = app();
    app.session_since = SystemTime::now() - Duration::from_secs(24 * 60);
    let rows = rows(&mut app);

    assert_eq!(rows[14].trim_end(), " /repo 24m");
}

#[test]
fn the_minute_wake_lands_within_the_minute() {
    let now = tokio::time::Instant::now();
    let at = super::next_minute();

    assert!(at > now, "in the future");
    assert!(at <= now + Duration::from_secs(60), "within the minute");
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
    assert!(row.starts_with("/repo 0m git · a-very"), "{row:?}");
    assert!(!row.ends_with("*1"), "the counts are cut: {row:?}");
}

#[test]
fn the_context_usage_shows_a_coloured_percentage() {
    let mut app = app();
    let place = rows(&mut app)[14].trim_end().to_string();
    assert!(
        place.ends_with("/repo 0m"),
        "nothing while the window is unknown"
    );

    let mut glm = model("glm", true);
    glm.context = Some(1000);
    app.llms = Some(vec![glm].into());
    let empty = rows(&mut app);
    assert!(
        empty[14].starts_with(&format!("{place} ◘ 0%")),
        "zero before the first reply: {:?}",
        empty[14]
    );

    // The colour of the `◘ NN%` span in row 14, found by its text.
    let colour = |app: &mut App, text: &str| -> Color {
        let buffer = buffer(app);
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 14)].symbol())
            .collect();
        let at = row.find(text).expect(text);
        let x = u16::try_from(row[..at].chars().count()).expect("fits");
        buffer[(x, 14)].fg
    };

    app.on_session(Event::Usage(Usage {
        input: 500,
        output: 0,
        ..Usage::default()
    }));
    assert_eq!(colour(&mut app, "◘ 50%"), Color::Yellow);

    app.on_session(Event::Usage(Usage {
        input: 800,
        output: 0,
        ..Usage::default()
    }));
    assert_eq!(colour(&mut app, "◘ 80%"), Color::Magenta);

    app.on_session(Event::Usage(Usage {
        input: 1000,
        output: 0,
        ..Usage::default()
    }));
    assert_eq!(colour(&mut app, "◘ 100%"), Color::Magenta);
}

#[test]
fn the_status_bar_shows_what_the_session_spent() {
    let mut app = app();
    let place = rows(&mut app)[14].trim_end().to_string();
    app.on_session(Event::Usage(Usage {
        input: 1_000,
        output: 10,
        ..Usage::default()
    }));
    assert!(
        rows(&mut app)[14].starts_with(&format!("{place} ↻ ?")),
        "no price known, no cache count said: {:?}",
        rows(&mut app)[14]
    );

    let mut glm = model("glm", true);
    glm.cost = Some(nth_protocol::Cost {
        input: 1_000.0,
        output: 10_000.0,
        cache_read: Some(100.0),
        cache_write: None,
    });
    app.llms = Some(vec![glm].into());
    app.on_session(Event::Usage(Usage {
        input: 1_000,
        output: 10,
        cache_read: Some(1_000),
        cache_write: None,
    }));
    // 1000 fresh at 1000/M, 1000 read at 100/M, 20 out at 10000/M.
    assert!(
        rows(&mut app)[14].contains(" $1.30 ↻ 50%"),
        "{:?}",
        rows(&mut app)[14]
    );
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
async fn usage_opens_a_tab_of_what_the_session_spent() {
    let mut app = app();
    app.on_session(Event::Usage(Usage {
        input: 2_000,
        output: 30,
        cache_read: Some(1_500),
        cache_write: None,
    }));
    app.prompt.insert_str("/usage");
    app.submit();
    let opened = rows(&mut app);
    assert!(opened[0].starts_with("  › chat [∑ usage] "), "{opened:#?}");
    assert!(opened[2].starts_with(" usage "), "{opened:#?}");
    assert!(
        opened[3].starts_with("   all  1 step  2.0k in  75% cached"),
        "{opened:#?}"
    );
    assert!(opened[6].starts_with("   glm  1 step"), "{opened:#?}");
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
    assert!(opened[3].trim_start().starts_with("plan  glm"));
    assert!(
        opened[4].trim_start().starts_with("▸ act   glm"),
        "a bare app acts"
    );

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

#[tokio::test]
async fn settings_toggle_for_this_run_and_hand_back_to_the_prompt() {
    let mut app = app();
    app.prompt.insert_str("half typed");
    app.run_command(crate::command::Command::Settings);
    let shown = rows(&mut app);
    assert_eq!(shown[10].trim_end(), " ▎ settings");
    assert!(
        shown[11].starts_with(" ▎ → [x] thinking "),
        "{:?}",
        shown[11]
    );
    assert!(
        shown[12].starts_with(" ▎   [x] tool output "),
        "{:?}",
        shown[12]
    );

    app.apply(Action::SelectNext);
    app.apply(Action::Submit);
    assert!(!app.settings.tool_output);
    assert!(rows(&mut app)[12].starts_with(" ▎ → [ ] tool output "));

    app.apply(Action::Interrupt);
    assert!(matches!(app.input, Input::Prompt));
    assert_eq!(app.prompt.text(), "half typed", "the prompt kept its text");
    app.run_command(crate::command::Command::Clear);
    assert!(!app.settings.tool_output, "kept for the rest of the run");
}

#[test]
fn chat_settings_start_from_the_config() {
    let mut app = app().with_chat_settings(crate::settings::ChatSettings {
        thinking: false,
        tool_output: false,
    });
    app.run_command(crate::command::Command::Settings);
    let shown = rows(&mut app);
    assert!(
        shown[11].starts_with(" ▎ → [ ] thinking "),
        "{:?}",
        shown[11]
    );
    assert!(
        shown[12].starts_with(" ▎   [ ] tool output "),
        "{:?}",
        shown[12]
    );
}
