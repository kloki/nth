//! Puts the terminal into the modes the chat needs, and back again on every
//! exit path, including a panic. Also the clipboard, which is the terminal's
//! too.

use std::io::{Write, stdout};

use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
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

/// The most text `copy` sends. Terminals drop an OSC 52 payload over their
/// limit without a word, so a bigger copy would look done and not be.
pub const COPY_LIMIT: usize = 64 * 1024;

/// Puts `text` on the clipboard through the terminal (OSC 52), which works
/// over ssh too; tmux needs `set-clipboard on`. There is no acknowledgement,
/// so `Ok` means the terminal was told.
pub fn copy(text: &str) -> std::io::Result<()> {
    let mut out = stdout();
    out.write_all(osc52(text).as_bytes())?;
    out.flush()
}

fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_sequence_carries_the_text_in_base64() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
