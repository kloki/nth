use ratatui::style::Color;

use super::*;
use crate::{
    app::{
        completion::Completion,
        tests::{app, buffer, prompt_row, rows},
    },
    command::{Command, Entry},
};

fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
    action(KeyEvent::new(code, modifiers))
}

#[test]
fn maps_keys_to_actions() {
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;

    assert_eq!(key(KeyCode::Esc, none), Some(Action::Interrupt));
    assert_eq!(key(KeyCode::Enter, none), Some(Action::Submit));
    assert_eq!(key(KeyCode::Enter, ctrl), Some(Action::Newline));
    assert_eq!(
        key(KeyCode::Enter, KeyModifiers::SHIFT),
        Some(Action::Newline)
    );
    assert_eq!(key(KeyCode::Char('j'), ctrl), Some(Action::Newline));
    assert_eq!(key(KeyCode::Char('c'), ctrl), Some(Action::ClearOrQuit));
    assert_eq!(key(KeyCode::Char('c'), none), Some(Action::Insert('c')));
    assert_eq!(key(KeyCode::Home, ctrl), Some(Action::Top));
    assert_eq!(key(KeyCode::Home, none), Some(Action::LineStart));
    assert_eq!(key(KeyCode::Char('n'), ctrl), Some(Action::Accept));
    assert_eq!(key(KeyCode::Char('m'), ctrl), Some(Action::LlmPicker));
    assert_eq!(key(KeyCode::Char('p'), ctrl), Some(Action::NextProfile));
    assert_eq!(key(KeyCode::Down, none), Some(Action::SelectNext));
    assert_eq!(key(KeyCode::Up, none), Some(Action::SelectPrev));
    assert_eq!(key(KeyCode::Tab, none), Some(Action::NextTab));
    assert_eq!(key(KeyCode::BackTab, none), Some(Action::PrevTab));
    assert_eq!(key(KeyCode::Char('g'), ctrl), Some(Action::Edit));
    assert_eq!(key(KeyCode::Char('t'), ctrl), Some(Action::NextContent));
    let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    assert_eq!(
        key(KeyCode::Char('T'), ctrl_shift),
        Some(Action::PrevContent)
    );
    assert_eq!(
        key(KeyCode::Char('t'), ctrl_shift),
        Some(Action::PrevContent)
    );
    assert_eq!(key(KeyCode::Char('q'), ctrl), Some(Action::CloseContent));
    assert_eq!(key(KeyCode::Char('1'), ctrl), Some(Action::Content(0)));
    assert_eq!(key(KeyCode::Char('4'), ctrl), Some(Action::Content(3)));
    assert_eq!(key(KeyCode::Char('5'), ctrl), None);
    assert_eq!(key(KeyCode::Char('1'), none), Some(Action::Insert('1')));
    assert_eq!(key(KeyCode::Char('x'), ctrl), None);
}

fn typed(text: &str) -> App {
    let mut app = app();
    for c in text.chars() {
        app.apply(Action::Insert(c));
    }
    app
}

fn selected(app: &App) -> Command {
    match &app.completion {
        Some(Completion::Command(popup)) => match popup.selected() {
            Entry::Builtin(command) => *command,
            skill => panic!("{skill:?} is not a command"),
        },
        _ => panic!("command popup not open"),
    }
}

fn names(app: &App) -> Vec<&str> {
    match &app.completion {
        Some(Completion::Command(popup)) => popup.items().iter().map(Entry::name).collect(),
        _ => panic!("command popup not open"),
    }
}

/// An app whose project has the `fix` skill, with `text` typed.
fn skilled(dir: &std::path::Path, text: &str) -> App {
    let mut app = app();
    app.context = crate::app::tests::with_fix_skill(dir);
    for c in text.chars() {
        app.apply(Action::Insert(c));
    }
    app
}

fn with_files(text: &str) -> App {
    let mut app = app();
    app.files = vec!["crates/nth-tui/src/app/keys.rs".into(), "README.md".into()];
    for c in text.chars() {
        app.apply(Action::Insert(c));
    }
    app
}

fn file(app: &App) -> &str {
    match &app.completion {
        Some(Completion::Mention { popup, .. }) => popup.selected().name(),
        _ => panic!("file popup not open"),
    }
}

#[test]
fn slash_opens_the_popup_and_typing_filters_it() {
    let mut app = typed("/");
    assert_eq!(selected(&app), Command::AddDir);

    app.apply(Action::Insert('e'));
    assert_eq!(selected(&app), Command::Exit);

    app.apply(Action::Insert('x'));
    app.apply(Action::Insert('x'));
    assert!(app.completion.is_none());
}

#[test]
fn arrows_cycle_without_touching_the_prompt() {
    let mut app = typed("/");
    app.apply(Action::SelectNext);
    assert_eq!(selected(&app), Command::Agents);
    for _ in 0..18 {
        app.apply(Action::SelectNext);
    }
    assert_eq!(selected(&app), Command::AddDir, "around the whole list");
    app.apply(Action::SelectPrev);
    assert_eq!(selected(&app), Command::Work);
    assert_eq!(app.prompt.text(), "/");
}

#[test]
fn ctrl_n_fills_in_the_highlighted_command() {
    let mut app = typed("/");
    app.apply(Action::SelectNext);
    app.apply(Action::Accept);
    assert_eq!(app.prompt.text(), "/agents");
    assert!(app.completion.is_some());
}

#[test]
fn enter_runs_the_highlighted_command() {
    let mut app = typed("/ex");
    app.apply(Action::Submit);
    assert!(app.quit);
    assert!(app.prompt.is_empty());
    assert!(app.completion.is_none());
}

#[test]
fn skills_are_listed_after_the_commands() {
    let dir = tempfile::tempdir().expect("tempdir");

    let app = skilled(dir.path(), "/");
    assert_eq!(
        names(&app),
        [
            "add-dir",
            "agents",
            "approve",
            "clear",
            "close",
            "compact",
            "connect",
            "copy-id",
            "curios",
            "diagnostics",
            "exit",
            "keymaps",
            "memory",
            "models",
            "plan",
            "resume",
            "settings",
            "usage",
            "work",
        ],
        "the commands fill the popup; the skills come after them"
    );

    let app = skilled(dir.path(), "/f");
    assert_eq!(names(&app), ["fix"]);
}

#[test]
fn a_skill_is_filled_in_ready_for_its_arguments() {
    let dir = tempfile::tempdir().expect("tempdir");

    let mut app = skilled(dir.path(), "/f");
    app.apply(Action::Accept);
    assert_eq!(app.prompt.text(), "/fix ");
    assert!(app.completion.is_none());

    let mut app = skilled(dir.path(), "/f");
    app.apply(Action::Submit);
    assert_eq!(
        app.prompt.text(),
        "/fix ",
        "Enter fills in a half-typed name"
    );
    assert!(!app.is_busy());
}

#[tokio::test]
async fn enter_on_a_typed_out_skill_runs_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = skilled(dir.path(), "/fix");

    app.apply(Action::Submit);

    assert!(app.is_busy());
    assert!(app.prompt.is_empty());
}

#[test]
fn esc_closes_the_popup_until_the_next_edit() {
    let mut app = typed("/c");
    app.apply(Action::Interrupt);
    assert!(app.completion.is_none());

    app.apply(Action::SelectNext);
    assert!(app.completion.is_none());

    app.apply(Action::Backspace);
    assert!(app.completion.is_some());
}

#[test]
fn plain_prompts_never_open_the_popup() {
    let mut app = typed("hello");
    assert!(app.completion.is_none());
    app.apply(Action::Accept);
    assert_eq!(app.prompt.text(), "hello");
}

#[test]
fn at_opens_the_file_popup_anywhere_in_the_prompt() {
    let mut app = with_files("see @");
    assert_eq!(file(&app), "README.md");

    app.apply(Action::Insert('k'));
    assert_eq!(file(&app), "crates/nth-tui/src/app/keys.rs");

    app.apply(Action::Insert(' '));
    assert!(app.completion.is_none());
}

#[test]
fn ctrl_n_and_enter_fill_in_the_file() {
    let mut app = with_files("see @ke");
    app.apply(Action::Accept);
    assert_eq!(app.prompt.text(), "see @crates/nth-tui/src/app/keys.rs ");
    assert!(app.completion.is_none());

    for c in "and @rea".chars() {
        app.apply(Action::Insert(c));
    }
    app.apply(Action::Submit);
    assert_eq!(
        app.prompt.text(),
        "see @crates/nth-tui/src/app/keys.rs and @README.md "
    );
    assert!(!app.is_busy());
}

#[test]
fn a_mention_before_the_cursor_is_completed_in_place() {
    let mut app = with_files("@rea tail");
    for _ in 0.." tail".len() {
        app.apply(Action::Left);
    }
    app.apply(Action::Backspace);
    app.apply(Action::Insert('a'));
    app.apply(Action::Accept);
    assert_eq!(app.prompt.text(), "@README.md tail");
}

#[test]
fn esc_closes_the_file_popup() {
    let mut app = with_files("@");
    app.apply(Action::Interrupt);
    assert!(app.completion.is_none());
    assert!(!app.is_busy());
}

/// An app that sent `sent`, oldest first, with `draft` typed.
fn recalling(sent: &[&str], draft: &str) -> App {
    let mut app = app();
    for prompt in sent {
        app.history.push(prompt.to_string());
    }
    app.prompt.insert_str(draft);
    app
}

#[test]
fn up_and_down_recall_sent_prompts_and_give_the_draft_back() {
    let mut app = recalling(&["first", "second"], "draft");

    app.apply(Action::SelectPrev);
    assert_eq!(app.prompt.text(), "second");
    assert_eq!(app.prompt.cursor(), "second".len());
    app.apply(Action::SelectPrev);
    assert_eq!(app.prompt.text(), "first");
    app.apply(Action::SelectPrev);
    assert_eq!(app.prompt.text(), "first", "stops at the oldest");

    app.apply(Action::SelectNext);
    app.apply(Action::SelectNext);
    assert_eq!(app.prompt.text(), "draft");
}

#[test]
fn up_leaves_an_edited_prompt_alone() {
    let mut app = recalling(&["first", "second"], "");
    app.apply(Action::SelectPrev);
    app.apply(Action::Insert('!'));

    app.apply(Action::SelectPrev);
    assert_eq!(app.prompt.text(), "second!");
}

#[test]
fn up_moves_an_open_popup_rather_than_recalling() {
    let mut app = recalling(&["sent"], "");
    for c in "/".chars() {
        app.apply(Action::Insert(c));
    }
    app.apply(Action::SelectPrev);
    assert_eq!(selected(&app), Command::Work);
    assert_eq!(app.prompt.text(), "/");
}

#[test]
fn a_recalled_command_does_not_open_the_popup() {
    let mut app = recalling(&["/models"], "");
    app.apply(Action::SelectPrev);
    assert_eq!(app.prompt.text(), "/models");
    assert!(app.completion.is_none());
}

#[test]
fn ctrl_c_clears_the_prompt_before_quitting() {
    let mut app = app();
    app.prompt.insert_str("draft");

    app.apply(Action::ClearOrQuit);
    assert!(app.prompt.is_empty());
    assert!(!app.quit);

    app.apply(Action::ClearOrQuit);
    assert!(app.quit);
}

#[test]
fn bang_at_the_start_makes_the_prompt_a_command() {
    let mut app = typed("!");
    assert!(app.prompt.shell());
    assert!(app.prompt.is_empty(), "the ! is not part of the command");
    let rows = rows(&mut app);
    assert_eq!(prompt_row(&rows, 0).trim_end(), " ▎ cmd");
    let buffer = buffer(&mut app);
    let prompt_y = buffer.area.height - crate::status::ROWS - crate::prompt::ROWS - 1;
    assert_eq!(buffer[(1, prompt_y)].fg, Color::Yellow);
    assert_eq!(prompt_row(&rows, 1).trim_end(), " ▎ Run a command.");

    app.apply(Action::Insert('!'));
    assert_eq!(app.prompt.text(), "!", "a second ! is text");

    let app = typed("hi!");
    assert!(!app.prompt.shell());
    assert_eq!(app.prompt.text(), "hi!");
}

#[test]
fn backspace_at_the_start_and_esc_leave_command_mode() {
    let mut app = typed("!ls");
    app.apply(Action::LineStart);
    app.apply(Action::Backspace);
    assert!(!app.prompt.shell());
    assert_eq!(app.prompt.text(), "ls");

    let mut app = typed("!ls");
    app.apply(Action::Interrupt);
    assert!(!app.prompt.shell());
    assert_eq!(app.prompt.text(), "ls");
}

#[test]
fn tab_keeps_command_mode() {
    // Plan, since act is the default and a switch to it would pass too.
    let mut app = app();
    app.set_mode(nth_protocol::Mode::Plan);
    app.apply(Action::Insert('!'));
    app.apply(Action::NextTab);
    assert!(app.prompt.shell());
    assert_eq!(
        app.mode,
        nth_protocol::Mode::Plan,
        "a command has no mode to switch"
    );
}

#[test]
fn a_recalled_command_comes_back_in_command_mode() {
    let mut app = recalling(&["!cargo test", "fix it"], "");
    app.apply(Action::SelectPrev);
    assert!(!app.prompt.shell());
    app.apply(Action::SelectPrev);
    assert!(app.prompt.shell());
    assert_eq!(app.prompt.text(), "cargo test");
    app.apply(Action::SelectNext);
    assert!(!app.prompt.shell());
    assert_eq!(app.prompt.text(), "fix it");
}
