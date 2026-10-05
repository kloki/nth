//! Puts the terminal into the modes the chat needs, and back again on every
//! exit path, including a panic.

use std::io::stdout;

use anyhow::Result;
use crossterm::{
    cursor::Show,
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
    capture();
    Ok(terminal)
}

/// Gives the terminal to another program, such as your editor, as it was
/// before nth started. `resume` takes it back.
pub fn suspend() {
    // The last frame may have hidden the cursor, and the editor expects
    // one; `restore` leaves the cursor as it is.
    let _ = execute!(stdout(), Show);
    restore();
}

/// Takes the terminal back after `suspend`, on the same `terminal`, whose
/// next draw repaints everything. Not `enter` again: that would add
/// another panic hook.
pub fn resume(terminal: &mut DefaultTerminal) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    capture();
    terminal.clear()?;
    Ok(())
}

/// The modes on top of what `ratatui::init` enables: the mouse, pasting,
/// and keys told apart.
fn capture() {
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    // Tells the keys apart that share a byte without it, where the terminal
    // supports it: shift+Enter and ctrl+Enter from Enter, ctrl+m from
    // Enter, ctrl+1 to ctrl+4 from the digits.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

pub fn restore() {
    release();
    ratatui::restore();
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
