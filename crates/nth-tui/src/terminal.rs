//! Puts the terminal into the modes the chat needs, and back again on every
//! exit path, including a panic.

use std::io::stdout;

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::DefaultTerminal;

pub fn enter() -> Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release();
        hook(info);
    }));
    hold();
    Ok(terminal)
}

pub fn restore() {
    release();
    ratatui::restore();
}

/// Hands the terminal back as the shell left it, for a program such as
/// the user's editor to take over; `resume` takes it back.
pub fn suspend() {
    restore();
}

/// Takes the terminal back after `suspend`. The caller clears the screen,
/// since what the other program left there is not the app's last frame.
pub fn resume() -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    hold();
    Ok(())
}

/// Sets the modes the chat needs on top of what `ratatui::init` enables.
fn hold() {
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    // Lets shift+Enter arrive as its own key where the terminal supports it.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

/// Undoes the modes set on top of what `ratatui::init` enables; popping
/// keyboard flags that were never pushed is a no-op for the terminal.
fn release() {
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture
    );
}
