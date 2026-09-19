use std::io;
use std::ops::{Deref, DerefMut};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::DefaultTerminal;

/// Owns the terminal for the life of a screen: `ratatui::init` on the way in, and
/// `ratatui::restore` when it drops — including on an early return or a panic unwinding
/// through the screen, which the hand-paired `init`/`restore` calls only covered by way of
/// ratatui's own panic hook.
pub(crate) struct TerminalGuard {
    terminal: DefaultTerminal,
    paste: bool,
}

impl TerminalGuard {
    pub(crate) fn new() -> Self {
        TerminalGuard {
            terminal: ratatui::init(),
            paste: false,
        }
    }

    /// As [`new`](Self::new), plus bracketed paste, for the screens with text fields:
    /// `ratatui::init()` does not turn it on, so a pasted value would otherwise arrive as
    /// one key press per character (see `fields.rs`).
    pub(crate) fn with_paste() -> Self {
        let mut guard = Self::new();
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        guard.paste = true;
        guard
    }
}

impl Deref for TerminalGuard {
    type Target = DefaultTerminal;

    fn deref(&self) -> &DefaultTerminal {
        &self.terminal
    }
}

impl DerefMut for TerminalGuard {
    fn deref_mut(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.paste {
            let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        }
        ratatui::restore();
    }
}
