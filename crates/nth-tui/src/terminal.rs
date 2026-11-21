//! Puts the terminal into the modes the chat needs, and back again on every
//! exit path, including a panic. Also the clipboard, which is the terminal's
//! too, and the [`Screen`] the app's loop draws on.

use std::{
    io::{self, Write, stdout},
    sync::Mutex,
};

use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use futures::Stream;
use ratatui::DefaultTerminal;

use crate::app::App;

/// What the app's loop draws on and hands to your editor: the real
/// terminal, or in tests a buffer, so the loop runs without one.
pub trait Screen {
    /// What the terminal sends: keys, the mouse, pastes and focus reports.
    type Input: Stream<Item = io::Result<Event>> + Unpin;

    /// Settles and draws one frame of `app`.
    fn draw(&mut self, app: &mut App) -> Result<()>;

    /// Gives the terminal to another program, such as your editor, and
    /// `input` goes with it, so it takes no keys meant for that program.
    /// An error leaves both as they were.
    fn suspend(&mut self, input: &mut Option<Self::Input>) -> Result<()>;

    /// Takes the terminal back after `suspend`, with input of its own again.
    fn resume(&mut self) -> Result<Self::Input>;
}

impl Screen for DefaultTerminal {
    type Input = EventStream;

    /// The terminal holds the frame until it is all written, so a screen
    /// that changes everywhere at once (the empty chat's field) never shows
    /// half old, half new.
    fn draw(&mut self, app: &mut App) -> Result<()> {
        set_title(&mut stdout(), app.session_title().as_deref())?;
        queue!(self.backend_mut(), BeginSynchronizedUpdate)?;
        app.frame(self)?;
        execute!(self.backend_mut(), EndSynchronizedUpdate)?;
        Ok(())
    }

    fn suspend(&mut self, input: &mut Option<EventStream>) -> Result<()> {
        // Before the terminal goes, so its reader thread stops first.
        *input = None;
        suspend();
        Ok(())
    }

    fn resume(&mut self) -> Result<EventStream> {
        resume(self)?;
        Ok(EventStream::new())
    }
}

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
fn suspend() {
    // The last frame may have hidden the cursor, and the editor expects
    // one; `restore` leaves the cursor as it is.
    let _ = execute!(stdout(), Show);
    restore();
}

/// Takes the terminal back after `suspend`, on the same `terminal`, whose
/// next draw repaints everything. Not `enter` again: that would add
/// another panic hook. A resize to the current size clears the screen and
/// the back buffer as `clear` does, without `clear`'s question for the
/// cursor position, whose answer can go missing right after the editor
/// quits and would end nth.
fn resume(terminal: &mut DefaultTerminal) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    capture();
    let area = terminal.size()?.into();
    terminal.resize(area)?;
    Ok(())
}

/// The modes on top of what `ratatui::init` enables: the mouse, pasting,
/// focus reports and keys told apart.
fn capture() {
    let _ = title_push(&mut stdout());
    // Focus reports decide whether a notification goes out; tmux passes
    // them on only with `focus-events on`.
    let _ = execute!(
        stdout(),
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    );
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
    let _ = title_pop(&mut stdout());
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture
    );
}

/// The terminal title stack is shared with the panic hook and editor handoff.
static TITLE: Mutex<TitleState> = Mutex::new(TitleState {
    active: false,
    last: None,
});

struct TitleState {
    active: bool,
    last: Option<String>,
}

fn title_push(out: &mut impl Write) -> io::Result<()> {
    let mut state = TITLE.lock().unwrap_or_else(|e| e.into_inner());
    if !state.active {
        out.write_all(b"\x1b[22;0t")?;
        out.flush()?;
        state.active = true;
        state.last = None;
    }
    Ok(())
}

fn title_pop(out: &mut impl Write) -> io::Result<()> {
    let mut state = TITLE.lock().unwrap_or_else(|e| e.into_inner());
    if state.active {
        out.write_all(b"\x1b[23;0t")?;
        out.flush()?;
        state.active = false;
        state.last = None;
    }
    Ok(())
}

fn window_title(title: Option<&str>) -> String {
    let clean = crate::header::clean_title(title.unwrap_or_default());
    let clean = clean.trim();
    if clean.is_empty() {
        "nth".into()
    } else {
        crate::header::ellipsize(&format!("nth | {clean}"), 80)
    }
}

fn set_title(out: &mut impl Write, title: Option<&str>) -> io::Result<()> {
    let title = window_title(title);
    let mut state = TITLE.lock().unwrap_or_else(|e| e.into_inner());
    if state.active && state.last.as_ref() != Some(&title) {
        write!(out, "\x1b]0;{title}\x1b\\")?;
        out.flush()?;
        state.last = Some(title);
    }
    Ok(())
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

    #[test]
    fn title_stack_deduplicates_and_restores_across_handoffs() {
        let mut output = Vec::new();
        title_push(&mut output).unwrap();
        set_title(&mut output, Some("hello\x1b]0;bad\x07")).unwrap();
        let after_first = output.len();
        set_title(&mut output, Some("hello\x1b]0;bad\x07")).unwrap();
        assert_eq!(output.len(), after_first);
        title_pop(&mut output).unwrap();
        title_pop(&mut output).unwrap();
        title_push(&mut output).unwrap();
        set_title(&mut output, Some("hello\x1b]0;bad\x07")).unwrap();
        title_pop(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches("\x1b[22;0t").count(), 2);
        assert_eq!(text.matches("\x1b[23;0t").count(), 2);
        assert_eq!(text.matches("\x1b]0;nth | hello]0;bad\x1b\\").count(), 2);
        assert_eq!(window_title(None), "nth");
        assert!(
            unicode_width::UnicodeWidthStr::width(window_title(Some(&"界".repeat(100))).as_str())
                <= 80
        );
    }
}
