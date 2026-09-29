//! The interactive chat: one scrollable history above a prompt bar.

mod app;
mod prompt;
mod scroll;
mod transcript;
mod view;

use std::{io::stdout, sync::Arc};

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::supports_keyboard_enhancement,
};
use nth_protocol::{Provider, Tool};
use nth_session::Session;

/// Runs the chat until the user quits. The terminal is restored on every
/// exit path, including a panic.
pub async fn run(
    session: Session,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release_terminal();
        hook(info);
    }));
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    // Lets shift+Enter arrive as its own key where the terminal supports it.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let result = app::App::new(session, provider, tools)
        .run(&mut terminal)
        .await;

    release_terminal();
    ratatui::restore();
    result
}

/// Undoes the modes set on top of what `ratatui::init` enables; popping
/// keyboard flags that were never pushed is a no-op for the terminal.
fn release_terminal() {
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture
    );
}
