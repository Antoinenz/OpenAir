//! Terminal setup and — more importantly — teardown.
//!
//! Two screens with different needs: the picker uses an **inline viewport** so
//! it behaves like a prompt and leaves scrollback intact, while the dashboard
//! takes the **alternate screen** because it is a mode you are in and it needs
//! the room.
//!
//! Restoring is handled twice over. [`TermGuard`] covers the normal path via
//! `Drop`; [`install_panic_hook`] covers the path where a render panics. A
//! panic that left the user in raw mode with an invisible cursor would be a
//! far worse bug than whatever caused it.

use std::io::{self, IsTerminal, Stdout, Write};
use std::sync::Once;

use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal when dropped.
///
/// Held alongside the `Terminal`; dropping it puts the terminal back however
/// the screen was entered.
pub struct TermGuard {
    alternate: bool,
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore(self.alternate);
    }
}

/// Enter an inline viewport of `rows` rows, keeping scrollback.
pub fn enter_inline(rows: u16) -> io::Result<(Tui, TermGuard)> {
    install_panic_hook();
    enable_raw_mode()?;
    let terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(rows),
        },
    )?;
    Ok((terminal, TermGuard { alternate: false }))
}

/// Enter the alternate screen for a full-window view.
pub fn enter_alt() -> io::Result<(Tui, TermGuard)> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        // Undo the raw mode we just set, or the shell is left unusable.
        let _ = disable_raw_mode();
        return Err(e);
    }
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok((terminal, TermGuard { alternate: true }))
}

/// Put the terminal back. Safe to call more than once, and every step is
/// best-effort: on the panic path there is nobody left to report a failure to,
/// and giving up early would skip the steps that still could have worked.
fn restore(alternate: bool) {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    if alternate {
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
    let _ = execute!(stdout, Show);
    let _ = stdout.flush();
}

static PANIC_HOOK: Once = Once::new();

/// Wrap the panic hook so the terminal is restored *before* the message is
/// printed. Installed automatically by `enter_*`; idempotent.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // We cannot know which screen was active, so undo both. Leaving an
            // alternate screen we never entered is a no-op.
            restore(true);
            previous(info);
        }));
    });
}

/// Whether stdout is a terminal we can draw on.
///
/// False when piped or redirected, which is how the TUI switches itself off
/// for scripts and CI without anyone passing `--no-tui`.
pub fn is_interactive() -> bool {
    io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_interactive_is_false_under_a_captured_stdout() {
        // cargo test captures stdout, so this is the non-tty path — the same
        // one a shell pipeline takes.
        assert!(!is_interactive());
    }

    #[test]
    fn panic_hook_installs_once() {
        // Second call must not stack another wrapper; if it did, every panic
        // would restore the terminal N times and print the message N times.
        install_panic_hook();
        install_panic_hook();
        assert!(PANIC_HOOK.is_completed());
    }

    #[test]
    fn restore_is_safe_when_no_screen_was_entered() {
        // The panic path calls this without knowing what was set up.
        restore(true);
        restore(false);
    }
}
