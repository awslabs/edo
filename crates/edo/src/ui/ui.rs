//! Thin RAII wrapper around the crossterm/ratatui terminal.
//!
//! Owns raw-mode toggling and the alternate-screen enter/leave. Does **not**
//! spawn a task and does **not** own an event channel — the App task in
//! `ui/app.rs` polls `crossterm::event::EventStream` directly. Keeping the
//! event stream owned by the App is invariant #1 from the
//! `memory://tui-input-starvation-fix.md` note: exactly one `EventStream`
//! exists in the process, and it lives on the App stack.
//!
//! `Drop` is belt-and-braces for panics / early returns
//! (`memory://tui-shutdown-discipline.md`).
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{
    cursor,
    terminal::{Clear, ClearType},
};
use snafu::ResultExt;

use super::Result;
use super::error;

/// Global one-shot flag: at most one `Ui` may be `enter`ed at a time.
/// Enforces the "one `EventStream` in the process" invariant at
/// runtime with a hard error (works in release too). The compile-time
/// surface is the grep test `event_stream_only_constructed_in_ui`.
static ENTERED: AtomicBool = AtomicBool::new(false);

/// Thin RAII wrapper: on `enter` puts the terminal into raw mode; on `exit`
/// (and `Drop`) restores it. The `App` task uses it as a scope guard.
pub struct Ui {
    /// Set to `true` while raw-mode is enabled. Used by `Drop` to decide
    /// whether teardown is needed.
    raw_active: bool,
}

impl Ui {
    /// Construct a fresh `Ui` handle. Does **not** enter raw mode; call
    /// `enter` from inside the App task so both toggles happen on the
    /// same runtime.
    pub fn new() -> Result<Self> {
        Ok(Self { raw_active: false })
    }

    /// Enable raw mode. Must be called from inside the App task and at
    /// most once concurrently across the process.
    ///
    /// Returns `Err(SecondEntry)` in **both** debug and release builds
    /// if another `Ui` is already entered. A silent second entry would
    /// spawn a competing `EventStream` reading the same stdin, which
    /// no test can detect at runtime — hence the hard error.
    pub fn enter(&mut self) -> Result<()> {
        if ENTERED.swap(true, Ordering::AcqRel) {
            return error::SecondEntrySnafu.fail();
        }
        crossterm::terminal::enable_raw_mode().context(error::IoSnafu)?;
        self.raw_active = true;
        Ok(())
    }

    /// Disable raw mode and clear from cursor down. Idempotent.
    pub fn exit(&mut self) -> Result<()> {
        if !self.raw_active {
            return Ok(());
        }
        use std::io::Write;
        if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
            let mut out = std::io::stderr();
            let _ = crossterm::execute!(out, Clear(ClearType::FromCursorDown), cursor::Show);
            let _ = out.flush();
            crossterm::terminal::disable_raw_mode().context(error::IoSnafu)?;
        }
        self.raw_active = false;
        ENTERED.store(false, Ordering::Release);
        Ok(())
    }

    /// Suspend the terminal for the duration of a sub-invocation (pager,
    /// interactive shell). Callers must pair every `suspend` with a
    /// `resume`.
    pub fn suspend(&mut self) -> Result<()> {
        self.exit()
    }

    /// Resume raw-mode after a `suspend`. Note: unlike the previous
    /// implementation, resume does **not** clear the terminal — the
    /// next frame tick redraws the viewport in place. The caller
    /// (`App::suspend_and_run`) still issues a `terminal.clear()`
    /// explicitly because a pager/shell may have left arbitrary output
    /// on-screen.
    pub fn resume(&mut self) -> Result<()> {
        self.enter()
    }
}

/// Belt-and-braces terminal restoration on drop. Errors are swallowed:
/// panicking inside `Drop` would abort the process, and there is nothing
/// sensible to do with a raw-mode-restoration failure at drop time.
impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.exit();
    }
}
