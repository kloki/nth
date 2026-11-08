//! An app drawn into a test backend, and the tests of the app as a whole:
//! its layout and its loop. Each concern's own tests sit next to its code.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

mod delivery_acceptance;
mod provider;

use crossterm::event::Event as TermEvent;
use nth_context::{Context as ProjectContext, Paths};
use nth_protocol::{
    Effort, Event, Message, Panel, Phase, Preview, Provider, Screen, ToolCall, Usage,
};
use nth_session::Session;
pub(crate) use provider::{Reply, Scripted};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};

use super::{App, ModeLlms, Panes, Queued, Tab, input::Input, keys::Action};
use crate::{PreparedRuntime, llm_picker::tests::model};

/// An idle app whose provider must never be asked for anything.
pub(crate) fn app() -> App {
    let session = Session::new("glm", "/repo".into());
    App::new(runtime(session, Arc::new(Scripted::idle())))
}

/// `session` on `provider` with no tools, a shell, idle checkers and each
/// mode on the session's llm, as a repository with an empty config would
/// prepare it, but for two keys: notifications are off, so no test reaches
/// your desktop, and the panel never splits unless a test asks.
pub(crate) fn runtime(session: Session, provider: Arc<dyn Provider>) -> PreparedRuntime {
    let llm = nth_protocol::Llm {
        model: session.model.clone(),
        effort: session.effort,
    };
    PreparedRuntime {
        session,
        paths: Paths::default(),
        provider,
        model_tools: Arc::new(Vec::new()),
        shell: Arc::new(nth_tools::Bash::new(Default::default())),
        checkers: crate::Checkers {
            lsp: nth_lsp::Lsp::new(&Default::default()),
            formatters: Arc::new(nth_format::Formatters::new(&Default::default())),
            rtk: false,
        },
        mode_llms: ModeLlms::same(llm),
        profiles: Vec::new(),
        warnings: Vec::new(),
        config: nth_config::Config {
            layout: nth_config::LayoutConfig { split_width: 0 },
            notify: nth_notify::NotifyConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        },
    }
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
    buffer_of(app, 40)
}

fn buffer_of(app: &mut App, width: u16) -> Buffer {
    buffer_sized(app, width, 16)
}

fn buffer_sized(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
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
    rows_of(app, 40)
}

/// The rows of `app` drawn `width` columns wide.
pub(crate) fn rows_of(app: &mut App, width: u16) -> Vec<String> {
    let buffer = buffer_of(app, width);
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// Rows counted from the bottom, independent of terminal height.
pub(crate) fn status_row(rows: &[String], offset: usize) -> &str {
    &rows[rows.len() - usize::from(crate::status::ROWS) + offset]
}

/// A row of the standard prompt panel, counted from its top.
pub(crate) fn prompt_row(rows: &[String], offset: usize) -> &str {
    &rows[rows.len() - usize::from(crate::status::ROWS + crate::prompt::ROWS) - 1 + offset]
}

/// A preview of a session whose system prompt is `body` lines long.
fn context_preview(app: &App, body: usize) -> Arc<Preview> {
    let mut session = Session::new("glm", "/repo".into());
    session.messages = vec![Message::System("body\n".repeat(body))];
    Arc::new(session.context_preview(&app.tools, &app.inbox))
}

#[test]
fn context_document_scrolls_and_keeps_its_position_on_update() {
    let mut app = app();
    let idle = rows(&mut app);
    app.run_command(crate::command::Command::Context);
    app.context_tab.view.update(Some(context_preview(&app, 30)));
    let first = rows(&mut app);
    assert_eq!(app.content.active(), Tab::Context);
    assert!(first.iter().any(|row| row.contains("summary")));
    assert_eq!(prompt_row(&first, 0), prompt_row(&idle, 0));
    assert_eq!(status_row(&first, 0), status_row(&idle, 0));
    app.context_tab.view.pager.scroll_down(5);
    app.context_tab.view.update(Some(context_preview(&app, 35)));
    rows(&mut app);
    assert_eq!(app.context_tab.view.pager.top(), 5);
    app.context_tab.view.pager.jump_bottom();
    let bottom = rows(&mut app);
    assert!(bottom.iter().any(|row| row.contains("body")));
    app.close_content();
    assert_eq!(app.content.active(), Tab::Chat);
}

#[tokio::test]
async fn context_wheel_and_prompt_keys_keep_their_normal_routing() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    for (width, column) in [(40, 5), (120, 90)] {
        let mut app = app().with_split_width(80);
        app.run_command(crate::command::Command::Context);
        app.context_tab.view.update(Some(context_preview(&app, 50)));
        rows_of(&mut app, width);
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.context_tab.view.pager.top(), 3);
        for key in [
            KeyCode::Char('a'),
            KeyCode::Char('c'),
            KeyCode::Left,
            KeyCode::Char('b'),
        ] {
            app.on_key(KeyEvent::from(key));
        }
        assert_eq!(app.prompt.text(), "abc");
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.prompt.is_empty());
        assert!(!app.chat.transcript.is_empty());
    }
}

#[tokio::test]
async fn context_projects_idle_session_and_live_snapshots() {
    let mut app = app();
    app.session
        .as_mut()
        .unwrap()
        .messages
        .push(Message::User("submitted message".into()));
    app.run_command(crate::command::Command::Context);
    rows(&mut app);
    app.context_tab.view.pager.jump_bottom();
    let rendered = rows(&mut app);
    assert!(!rendered.iter().any(|r| r.contains("submitted message")));
    let mut preview = app
        .session
        .as_ref()
        .unwrap()
        .context_preview(&app.tools, &app.inbox);
    preview.phase = Phase::PendingResults;
    preview.entries.push(nth_protocol::Entry {
        message: Message::User("live revision".into()),
        estimated_tokens: 3,
        checkpoint: false,
        images_available: vec![],
    });
    // As a turn starting with the tab open does.
    app.context_turn_starting();
    app.context_tab.slot.publish(Arc::new(preview));
    let snapshot = app.context_tab.next().await;
    app.context_tab.show(snapshot);
    app.context_tab.view.pager.jump_top();
    let rendered = rows(&mut app);
    assert!(rendered.iter().any(|r| r.contains("pending results")));
    assert!(!rendered.iter().any(|r| r.contains("live revision")));
}

#[test]
fn context_refresh_uses_selected_settings_without_mutating_session() {
    let mut app = app();
    app.run_command(crate::command::Command::Context);
    app.model = "chosen-model".into();
    app.mode = nth_protocol::Mode::Plan;
    app.prompt.insert_str("unsent secret");
    app.refresh_context();
    let rendered = rows(&mut app).join("\n");
    assert!(rendered.contains("chosen-model"), "{rendered}");
    assert!(rendered.contains("plan"), "{rendered}");
    assert_eq!(app.session.as_ref().unwrap().model, "glm");
    app.context_tab.view.pager.jump_bottom();
    let rendered = rows(&mut app)[1..8].join("\n");
    assert!(!rendered.contains("unsent secret"));
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
fn tiny_terminal_draws_without_overflow() {
    let mut app = app();
    let buffer = buffer_sized(&mut app, 40, 4);
    assert_eq!(buffer.area, ratatui::layout::Rect::new(0, 0, 40, 4));
    let rows: Vec<String> = (0..4)
        .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    assert!(rows.iter().any(|row| row.contains("act")), "{rows:?}");
    for width in 1..8 {
        for height in 1..8 {
            let _ = buffer_sized(&mut app, width, height);
        }
    }
}

#[test]
fn a_tiny_terminal_draws_an_empty_picker_without_overflow() {
    let mut app = app();
    let mut picker = crate::session_picker::SessionPicker::new(uuid::Uuid::new_v4());
    picker.load(Ok(Vec::new()));
    app.input = Input::SessionPicker(picker);

    for width in 1..8 {
        for height in 1..8 {
            let _ = buffer_sized(&mut app, width, height);
        }
    }
}

#[test]
fn a_tiny_terminal_draws_a_question_without_overflow() {
    use crate::question::tests::{panel, question};

    let mut app = app();
    let (panel, _reply) = panel(vec![question("auth", false, &["OAuth", "API key"])]);
    app.input = Input::Question(panel);

    for width in 1..8 {
        for height in 1..8 {
            let _ = buffer_sized(&mut app, width, height);
        }
    }
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
    assert_eq!(prompt_row(&idle, 0).trim_end(), " ▎ act  · glm");
    assert_eq!(prompt_row(&idle, 1).trim_end(), " ▎ Ask anything.");
    assert_eq!(prompt_row(&idle, 2).trim_end(), " ▎");
    assert_eq!(prompt_row(&idle, 3).trim_end(), " ▎");
    assert_eq!(status_row(&idle, 0).trim_end(), " /repo 0m");
    assert!(status_row(&idle, 1).trim().is_empty(), "no hint when idle");

    for i in 0..20 {
        app.chat.transcript.push_user(format!("message {i}"));
    }
    let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
    app.prompt.insert_str(&lines.join("\n"));
    app.busy_since = Some(Instant::now());
    let busy = rows(&mut app);

    assert_eq!(busy[0], idle[0], "the header stays on top");
    assert_eq!(busy[7].trim_end(), " ▎ message 19");
    let spinner = prompt_row(&busy, 0)
        .chars()
        .skip(3)
        .take(4)
        .collect::<String>();
    assert!(
        spinner.chars().all(|c| ('⠀'..='⣿').contains(&c)),
        "spinner in place of the mode: {:?}",
        prompt_row(&busy, 0)
    );
    assert!(prompt_row(&busy, 0).trim_end().ends_with("esc to cancel"));
    // Byte offsets shift with the spinner's multi-byte glyphs; count chars.
    let at = |row: &str| row.find("· glm").map(|at| row[..at].chars().count());
    assert_eq!(
        at(prompt_row(&busy, 0)),
        at(prompt_row(&idle, 0)),
        "the model stays put while busy"
    );
    let text: Vec<&str> = busy[10..=12].iter().map(|r| r.trim_end()).collect();
    assert_eq!(
        text,
        [" ▎ line 8", " ▎ line 9", " ▎ line 10"],
        "scrolled to the cursor"
    );
    assert_eq!(
        status_row(&busy, 0).trim_end(),
        " /repo 0m",
        "no git outside a repo"
    );
    assert!(
        status_row(&busy, 1).trim().is_empty(),
        "nothing below while busy"
    );
}

#[test]
fn the_prompt_shows_the_model_with_its_provider_and_effort_coloured() {
    let mut app = app();
    app.model = "opencode/glm-5.3".into();
    app.effort = Effort::High;
    let buffer = buffer(&mut app);
    let row: String = (0..buffer.area.width)
        .map(|x| {
            buffer[(
                x,
                buffer.area.height - crate::status::ROWS - crate::prompt::ROWS - 1,
            )]
                .symbol()
        })
        .collect();

    assert_eq!(row.trim_end(), " ▎ act  · opencode/glm-5.3 · high");
    let fg = |text: &str| {
        let at = row.find(text).expect(text);
        buffer[(
            u16::try_from(row[..at].chars().count()).expect("fits"),
            buffer.area.height - crate::status::ROWS - crate::prompt::ROWS - 1,
        )]
            .fg
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

    app.diagnostics.pager.jump_bottom();
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
    assert!(
        status_row(&up, 1).trim().is_empty(),
        "no hint in the status bar"
    );

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

    assert_eq!(
        status_row(&rows, 0).trim_end(),
        " /repo 0m",
        "the first line stays"
    );
    assert_eq!(
        status_row(&rows, 1).trim_end(),
        " > 2 queued · fix the build"
    );
}

#[test]
fn the_status_bar_shows_the_session_time() {
    let mut app = app();
    app.session_since = SystemTime::now() - Duration::from_secs(24 * 60);
    let rows = rows(&mut app);

    assert_eq!(status_row(&rows, 0).trim_end(), " /repo 24m");
}

#[test]
fn status_widgets_show_directories_and_worst_lsp_state() {
    use nth_lsp::{ServerState, ServerStatus};
    let mut app = app();
    app.extra_dirs = vec!["/a/docs".into(), "/b/docs".into()];
    app.servers = vec![
        ServerStatus {
            id: "rust".into(),
            root: "/a".into(),
            state: ServerState::Connected,
        },
        ServerStatus {
            id: "rust".into(),
            root: "/b".into(),
            state: ServerState::Broken("failed".into()),
        },
    ];
    let drawn = buffer_of(&mut app, 80);
    let y = drawn.area.height - 1;
    let row: String = (0..drawn.area.width)
        .map(|x| drawn[(x, y)].symbol())
        .collect();
    assert!(
        row.trim_end().ends_with("+ a/docs, b/docs ● lsp"),
        "{row:?}"
    );
    let dot = row.chars().position(|c| c == '●').unwrap() as u16;
    assert_eq!(drawn[(dot, y)].fg, Color::Red);
    assert!(!status_row(&rows_of(&mut app, 80), 0).contains("(+2)"));
}

#[test]
fn the_minute_wake_lands_within_the_minute() {
    let before = tokio::time::Instant::now();
    let at = super::next_minute();
    // Up to a whole minute from when it was asked, a moment after `before`.
    let after = tokio::time::Instant::now();

    assert!(at > before, "in the future");
    assert!(at <= after + Duration::from_secs(60), "within the minute");
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

    let row = status_row(&rows, 0).trim();
    assert!(row.starts_with("/repo 0m git · a-very"), "{row:?}");
    assert!(!row.ends_with("*1"), "the counts are cut: {row:?}");
}

#[test]
fn the_context_usage_shows_a_coloured_percentage() {
    let mut app = app();
    let place = status_row(&rows(&mut app), 0).trim_end().to_string();
    assert!(
        place.ends_with("/repo 0m"),
        "nothing while the window is unknown"
    );

    let mut glm = model("glm", true);
    glm.context = Some(1000);
    app.llms = Some(vec![glm].into());
    let empty = rows(&mut app);
    assert!(
        status_row(&empty, 0).starts_with(&format!("{place} ◘ 0%")),
        "zero before the first reply: {:?}",
        status_row(&empty, 0)
    );

    // The colour of the `◘ NN%` span in row 14, found by its text.
    let colour = |app: &mut App, text: &str| -> Color {
        let buffer = buffer(app);
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - crate::status::ROWS)].symbol())
            .collect();
        let at = row.find(text).expect(text);
        let x = u16::try_from(row[..at].chars().count()).expect("fits");
        buffer[(x, buffer.area.height - crate::status::ROWS)].fg
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
    let place = status_row(&rows(&mut app), 0).trim_end().to_string();
    app.on_session(Event::Usage(Usage {
        input: 1_000,
        output: 10,
        ..Usage::default()
    }));
    assert_eq!(
        status_row(&rows(&mut app), 0).trim_end(),
        place,
        "no price known, nothing said"
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
        reasoning: None,
    }));
    // 1000 fresh at 1000/M, 1000 read at 100/M, 20 out at 10000/M.
    assert!(
        status_row(&rows(&mut app), 0).contains(" $1.30 "),
        "{:?}",
        status_row(&rows(&mut app), 0)
    );
    let drawn = buffer(&mut app);
    let y = drawn.area.height - crate::status::ROWS;
    let row: String = (0..drawn.area.width)
        .map(|x| drawn[(x, y)].symbol())
        .collect();
    let x = row.chars().position(|c| c == '$').unwrap() as u16;
    assert_eq!(drawn[(x, y)].fg, Color::Yellow);
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

#[tokio::test]
async fn a_panicking_job_is_a_hint_not_an_exit() {
    let mut app = app();
    let before = crate::git::GitStatus {
        branch: Some("main".into()),
        ..Default::default()
    };
    app.git = Some(before.clone());
    app.git_loading
        .start(|_| tokio::spawn(async { panic!("git exploded") }));

    let status = app.git_loading.join().await;
    app.job_finished(super::Finished::GitLoaded(status));

    assert_eq!(app.git, Some(before), "left as it was");
    let shown = rows_of(&mut app, 120).join("\n");
    assert!(
        shown.contains("reading git status failed: task")
            && shown.contains("panicked with message \"git exploded\""),
        "{shown}"
    );
}

#[tokio::test]
async fn a_panicking_session_listing_shows_in_the_picker() {
    let mut app = app();
    app.input = Input::SessionPicker(crate::session_picker::SessionPicker::new(app.session_id));
    app.session_listing
        .start(|_| tokio::spawn(async { panic!("listing exploded") }));

    let sessions = app.session_listing.join().await;
    app.job_finished(super::Finished::SessionsListed(sessions));

    let shown = rows_of(&mut app, 120).join("\n");
    assert!(!shown.contains("loading sessions"), "{shown}");
    assert_eq!(
        shown.matches("listing sessions failed: task").count(),
        2,
        "in the chat and in the picker: {shown}"
    );
}

#[tokio::test]
async fn a_panicking_save_still_runs_the_one_asked_for_meanwhile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = app();
    app.store = Some(Arc::new(nth_session::Store::at(dir.path())));
    app.session_saving
        .start(|_| tokio::spawn(async { panic!("save exploded") }));
    app.save_session();

    let saved = app.session_saving.join().await;
    app.job_finished(super::Finished::SessionSaved(saved));

    assert!(app.session_saving.is_running(), "the queued save runs");
    let saved = app.session_saving.join().await.expect("saves");
    saved.expect("written");
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
async fn a_fresh_session_takes_the_configured_compaction() {
    let configured = nth_protocol::Compaction {
        auto: false,
        reserved: 1_000,
        preserve: 500,
    };
    let mut session = Session::new("glm", "/repo".into());
    session.compaction = configured;
    let mut app = App::bare(session, Arc::new(Scripted::idle()), Arc::new(Vec::new()));

    app.prompt.insert_str("/clear");
    app.submit();

    assert_eq!(
        app.session.as_ref().expect("idle").compaction,
        configured,
        "the new session follows [context], not the defaults"
    );
}

#[tokio::test]
async fn usage_opens_a_tab_of_what_the_session_spent() {
    let mut app = app();
    app.on_session(Event::Usage(Usage {
        input: 2_000,
        output: 30,
        cache_read: Some(1_500),
        cache_write: None,
        reasoning: None,
    }));
    app.prompt.insert_str("/usage");
    app.submit();
    let opened = rows(&mut app);
    assert!(opened[0].starts_with("  › chat [∑ usage] "), "{opened:#?}");
    assert!(opened[2].starts_with(" ▎ current session "), "{opened:#?}");
    assert!(
        opened[3].starts_with(" ▎   all  1 step  2.0k in  75% cached"),
        "{opened:#?}"
    );
    assert!(opened[6].starts_with(" ▎   glm  1 step"), "{opened:#?}");
}

#[tokio::test]
async fn keymaps_command_opens_a_scrollable_reference_tab_once() {
    let mut app = app();
    app.prompt.insert_str("/keymaps");
    app.submit();

    assert_eq!(app.content.active(), Tab::Keymaps);
    assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Keymaps]);
    assert!(
        !app.is_busy(),
        "opening the reference does not start a turn"
    );
    let first = rows(&mut app);
    assert!(first[0].contains("keymaps"), "{first:#?}");
    assert!(
        first.iter().any(|row| row.contains("content tabs")),
        "{first:#?}"
    );
    assert!(first.iter().any(|row| row.contains("Ctrl+T")), "{first:#?}");

    app.prompt.insert_str("/keymaps");
    app.submit();
    assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Keymaps]);

    app.apply(Action::Bottom);
    rows(&mut app);
    assert!(
        app.keymaps.pager.scrollbar().is_some(),
        "reference overflows the view"
    );

    app.apply(Action::CloseContent);
    assert_eq!(app.content.tabs(), [Tab::Chat]);
    app.prompt.insert_str("/keymaps");
    app.submit();
    assert_eq!(app.content.active(), Tab::Keymaps);
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
    assert!(opened[2].starts_with(" ▎ lsp usage"), "{:?}", opened[2]);
    assert!(opened[5].starts_with(" ▎ model "), "{:?}", opened[5]);
    assert!(opened[6].starts_with(" ▎     plan      glm"));
    assert!(
        opened[7].starts_with(" ▎   → act       glm"),
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

/// An app whose chat says `hello there`, splitting from 80 columns, with
/// the diagnostics tab open.
fn split_app() -> App {
    let mut app = app().with_split_width(80);
    app.chat.apply(&Event::TextDelta("hello there".into()));
    app.chat
        .transcript
        .finish_turn(Ok(()), "glm", Duration::from_secs(1));
    app.open_content(Tab::Diagnostics);
    app
}

/// Whether some content row holds `text` in the columns `columns`.
fn shows_in(rows: &[String], columns: std::ops::Range<usize>, text: &str) -> bool {
    rows[2..].iter().any(|row| {
        let row: Vec<char> = row.chars().collect();
        row[columns.clone()]
            .iter()
            .collect::<String>()
            .contains(text)
    })
}

#[tokio::test]
async fn a_wide_terminal_shows_the_chat_beside_the_other_tab() {
    let mut app = split_app();
    let split = rows_of(&mut app, 100);
    assert!(shows_in(&split, 0..50, "hello there"), "{split:#?}");
    assert!(shows_in(&split, 50..100, "model"), "{split:#?}");
    assert!(split[0].starts_with(" [› chat]"), "{:?}", split[0]);
    let header: Vec<char> = split[0].chars().collect();
    let right: String = header[50..].iter().collect();
    assert!(
        right.starts_with(" [● diagnostics]"),
        "over the right pane: {right:?}"
    );

    app.apply(Action::Content(0));
    let focused = rows_of(&mut app, 100);
    assert!(
        focused[0].contains("[● diagnostics]"),
        "both showing tabs bracketed"
    );
    assert!(
        shows_in(&focused, 50..100, "model"),
        "the side keeps its tab"
    );

    app.apply(Action::NextContent);
    assert_eq!(app.content.active(), Tab::Diagnostics);

    let narrow = rows_of(&mut app, 79);
    assert!(!shows_in(&narrow, 0..79, "hello there"), "{narrow:#?}");
    assert!(shows_in(&narrow, 0..79, "model"));
}

#[tokio::test]
async fn a_lone_chat_keeps_the_whole_width() {
    let mut app = split_app();
    app.apply(Action::CloseContent);
    let rows = rows_of(&mut app, 100);
    assert_eq!(app.panes(), Panes::Single(Tab::Chat));
    assert!(shows_in(&rows, 0..100, "hello there"));
}

#[tokio::test]
async fn a_zero_split_width_never_splits() {
    let mut app = split_app().with_split_width(0);
    app.apply(Action::Content(0));
    let rows = rows_of(&mut app, 200);
    assert_eq!(app.panes(), Panes::Single(Tab::Chat));
    assert!(!shows_in(&rows, 0..200, "model"), "{rows:#?}");
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

    let Some(show) = app.screen_rx.recv().await else {
        panic!("sent a panel");
    };
    app.show(show);
    assert_eq!(app.content.active(), Tab::Diagnostics);
    assert!(app.flash.is_some(), "the tab flashes");
}

#[tokio::test]
async fn a_tab_you_switch_to_does_not_flash() {
    let mut app = app();
    app.open_content(Tab::Diagnostics);
    app.apply(Action::Content(0));
    assert!(app.flash.is_none());
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
    assert_eq!(
        prompt_row(&rows(&mut app), 1).trim_end(),
        " ▎ [pasted 5 lines]"
    );

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
    assert_eq!(prompt_row(&shown, 1).trim_end(), " ▎ settings");
    assert!(
        prompt_row(&shown, 2).starts_with(" ▎ → [x] thinking "),
        "{:?}",
        prompt_row(&shown, 2)
    );
    assert!(
        prompt_row(&shown, 3).starts_with(" ▎   [x] tool output "),
        "{:?}",
        prompt_row(&shown, 3)
    );

    app.apply(Action::SelectNext);
    app.apply(Action::Submit);
    assert!(!app.settings.tool_output);
    assert!(prompt_row(&rows(&mut app), 3).starts_with(" ▎ → [ ] tool output "));

    app.apply(Action::Interrupt);
    assert!(matches!(app.input, Input::Prompt));
    assert_eq!(app.prompt.text(), "half typed", "the prompt kept its text");
    app.run_command(crate::command::Command::Clear);
    assert!(!app.settings.tool_output, "kept for the rest of the run");
}

#[test]
fn chat_settings_start_from_the_config() {
    let session = Session::new("glm", "/repo".into());
    let mut runtime = runtime(session, Arc::new(Scripted::idle()));
    runtime.config.chat.thinking = false;
    runtime.config.chat.tool_output = false;
    let mut app = App::new(runtime);
    app.run_command(crate::command::Command::Settings);
    let shown = rows(&mut app);
    assert!(
        prompt_row(&shown, 2).starts_with(" ▎ → [ ] thinking "),
        "{:?}",
        prompt_row(&shown, 2)
    );
    assert!(
        prompt_row(&shown, 3).starts_with(" ▎   [ ] tool output "),
        "{:?}",
        prompt_row(&shown, 3)
    );
}

#[tokio::test]
async fn copy_id_copies_the_session_id_even_mid_turn() {
    thread_local! {
        static COPIED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let copied = || COPIED.with(|c| c.borrow().clone());
    let mut app = app();
    app.clipboard = |text| {
        COPIED.with(|c| c.borrow_mut().push(text.to_string()));
        Ok(())
    };
    let first = app.session.as_ref().expect("idle").id;

    app.run_command(crate::command::Command::CopyId);
    assert_eq!(copied(), [first.to_string()]);
    assert_eq!(
        app.hint.as_deref(),
        Some(format!("copied {first}").as_str())
    );

    // Mid-turn the session is in the turn task.
    let session = app.session.take();
    app.run_command(crate::command::Command::CopyId);
    assert_eq!(copied()[1], first.to_string());
    app.session = session;

    app.run_command(crate::command::Command::Clear);
    app.run_command(crate::command::Command::CopyId);
    let fresh = app.session.as_ref().expect("idle").id;
    assert_ne!(fresh, first);
    assert_eq!(copied()[2], fresh.to_string());
}

#[test]
fn copy_takes_the_last_completed_answer_as_markdown() {
    thread_local! {
        static COPIED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let copied = || COPIED.with(|c| c.borrow().clone());
    let mut app = app();
    app.clipboard = |text| {
        COPIED.with(|c| c.borrow_mut().push(text.to_string()));
        Ok(())
    };
    let transcript = &mut app.chat.transcript;
    transcript.push_user("first".into());
    transcript.apply(&Event::TextDelta("**old** answer".into()));
    transcript.finish_turn(Ok(()), "glm", Duration::ZERO);
    transcript.push_user("second".into());
    transcript.apply(&Event::TextDelta("the latest\nraw `markdown`".into()));
    transcript.finish_turn(Ok(()), "glm", Duration::ZERO);
    transcript.push_user("third, not answered yet".into());

    app.run_command(crate::command::Command::Copy);
    assert_eq!(copied(), ["the latest\nraw `markdown`".to_string()]);
    assert_eq!(app.hint.as_deref(), Some("copied"));
}

#[test]
fn copy_takes_a_compaction_summary_as_the_latest_answer() {
    thread_local! {
        static COPIED: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }
    let mut app = app();
    app.clipboard = |text| {
        COPIED.with(|c| *c.borrow_mut() = Some(text.to_string()));
        Ok(())
    };
    let transcript = &mut app.chat.transcript;
    transcript.push_user("first".into());
    transcript.apply(&Event::TextDelta("an answer".into()));
    transcript.finish_turn(Ok(()), "glm", Duration::ZERO);
    transcript.apply(&Event::Compacted {
        summary: "## Summary\n- kept **this**".into(),
    });

    app.run_command(crate::command::Command::Copy);
    assert_eq!(
        COPIED.with(|c| c.borrow().clone()),
        Some("## Summary\n- kept **this**".to_string())
    );
}

#[test]
fn copy_says_when_there_is_nothing_to_copy() {
    thread_local! {
        static COPIED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let mut app = app();
    app.clipboard = |text| {
        COPIED.with(|c| c.borrow_mut().push(text.to_string()));
        Ok(())
    };

    app.run_command(crate::command::Command::Copy);
    assert_eq!(app.hint.as_deref(), Some("nothing to copy"));
    assert!(COPIED.with(|c| c.borrow().is_empty()));

    app.chat.transcript.push_user("only a question".into());
    app.run_command(crate::command::Command::Copy);
    assert_eq!(app.hint.as_deref(), Some("nothing to copy"));
    assert!(COPIED.with(|c| c.borrow().is_empty()));
}

#[test]
fn copy_reports_a_clipboard_failure() {
    let mut app = app();
    app.clipboard = |_| Err(std::io::Error::other("no clipboard"));
    app.chat
        .transcript
        .apply(&Event::TextDelta("an answer".into()));
    app.chat
        .transcript
        .finish_turn(Ok(()), "glm", Duration::ZERO);

    app.run_command(crate::command::Command::Copy);
    assert_eq!(app.hint.as_deref(), Some("could not copy: no clipboard"));
}

#[tokio::test]
async fn typing_copy_runs_the_command() {
    thread_local! {
        static COPIED: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }
    let mut app = app();
    app.clipboard = |text| {
        COPIED.with(|c| *c.borrow_mut() = Some(text.to_string()));
        Ok(())
    };
    app.chat
        .transcript
        .apply(&Event::TextDelta("the **answer**".into()));
    app.chat
        .transcript
        .finish_turn(Ok(()), "glm", Duration::ZERO);

    app.prompt.insert_str("/copy");
    app.submit();
    assert_eq!(
        COPIED.with(|c| c.borrow().clone()),
        Some("the **answer**".to_string())
    );
}
