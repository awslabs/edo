//! Rendering helpers for the interactive TUI.
//!
//! Split from `mod.rs` so the visuals live next to each other and can
//! evolve independently of the transport (`Console`), the runtime
//! (`App`), and the emit sugar (`macros`).

use jiff::Timestamp;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::store::{PromptSelect, State};

/// Layout: 2-row statusline + task pane. The task pane fills the
/// remainder of the inline viewport. `render_tasks` self-caps at 10 so a
/// large `[scheduler] workers` value doesn't push the viewport off the
/// bottom of the screen.
pub(crate) fn draw_frame(state: &State, frame: &mut Frame) {
    // Compute wall-clock once per frame so `Task::to_line` doesn't call
    // `Timestamp::now()` N times per redraw. Saves ~N × ~100ns on Linux
    // per frame; more importantly makes durations line up across rows.
    let now = Timestamp::now();
    let area = frame.area();
    // The statusline slot is 2 rows in the common case, but when a
    // prompt overlay is active it renders inside a bordered Block that
    // consumes 2 rows for the top/bottom borders. Without extra height
    // the body lines (`addr:/error:` and the choice row) are clipped
    // and the user sees an empty red-bordered strip.
    let status_height: u16 = if state.prompt.is_some() { 4 } else { 2 };
    let chunks =
        Layout::vertical([Constraint::Length(status_height), Constraint::Min(1)]).split(area);
    render_tasks(state, frame, chunks[1], now);
    render_statusline(state, frame, chunks[0]);
}

/// Two-line statusline. When a prompt is active it takes over the slot
/// and renders as a bordered modal-style overlay; otherwise it shows the
/// progress summary (`done/total active N waiting M`).
pub(crate) fn render_statusline(state: &State, frame: &mut Frame, area: Rect) {
    if let Some(prompt) = state.prompt.as_ref() {
        // Prompt overlay lives in the statusline slot.
        let mut line_0: Vec<Span> = Vec::new();
        line_0.push(Span::styled(
            format!("addr: {}", prompt.addr),
            Style::default().bold(),
        ));
        line_0.push(Span::raw(" "));
        line_0.push(Span::styled(
            format!(
                "error: {}",
                truncate(&prompt.error, area.width.saturating_sub(10) as usize)
            ),
            Style::default().fg(Color::Red),
        ));
        let mut line_1: Vec<Span> = Vec::new();
        if prompt.log_file.is_some() {
            line_1.push(Span::styled(
                "[v]iew log ",
                if prompt.selected == PromptSelect::ViewLog {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::Cyan)
                },
            ));
        }
        if prompt.allow_shell {
            line_1.push(Span::styled(
                "[s]hell ",
                if prompt.selected == PromptSelect::Shell {
                    Style::default().fg(Color::Yellow).bold()
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ));
        }
        if prompt.allow_retry {
            line_1.push(Span::styled(
                "[r]etry ",
                if prompt.selected == PromptSelect::Retry {
                    Style::default().fg(Color::Green).bold()
                } else {
                    Style::default().fg(Color::Green)
                },
            ));
        }
        line_1.push(Span::styled(
            "[q]uit",
            if prompt.selected == PromptSelect::Quit {
                Style::default().fg(Color::Red).bold()
            } else {
                Style::default().fg(Color::Red)
            },
        ));
        let body = vec![Line::from(line_0), Line::from(line_1)];
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            " transform failed ",
            Style::default().fg(Color::Red).bold(),
        ));
        let p = Paragraph::new(body).block(block);
        frame.render_widget(p, area);
        return;
    }

    // No build has started (and none has finished): render an empty
    // statusline instead of a misleading `0/0 <target>` progress row.
    // Session commands like `update`, `list`, and `prune` never emit
    // `StartBuild` / `StartTask` / `BuildFinish`, so their state stays
    // at defaults; showing a "0/0" progress indicator alongside those
    // commands' diagnostics is noise at best and, when the command
    // then errors out, visually splices the error text onto the
    // progress line.
    if !state.done
        && state.total == 0
        && state.finished == 0
        && state.in_flight == 0
        && state.waiting == 0
    {
        return;
    }

    let total = state.total.max(state.finished);
    let root = state
        .addr
        .as_ref()
        .map(|x| x.to_string())
        .unwrap_or_default();
    let msg = if state.done {
        if state.ok {
            format!(
                "ok {done}/{total} {root}",
                done = state.finished,
                total = total,
                root = root
            )
        } else {
            format!(
                "failed {done}/{total} failed {failed} {root}",
                done = state.finished,
                total = total,
                failed = state.failed.len(),
                root = root
            )
        }
    } else if state.waiting > 0 || state.in_flight > 0 {
        if state.waiting > 0 {
            format!(
                "{done}/{total} active {running} waiting {waiting} {root}",
                done = state.finished,
                total = total,
                running = state.in_flight,
                waiting = state.waiting,
                root = root
            )
        } else {
            format!(
                "{done}/{total} active {running} {root}",
                done = state.finished,
                total = total,
                running = state.in_flight,
                root = root
            )
        }
    } else {
        format!(
            "{done}/{total} {root}",
            done = state.finished,
            total = total,
            root = root
        )
    };
    let style = if state.done {
        if state.ok {
            Style::default().fg(Color::Green).bold()
        } else {
            Style::default().fg(Color::Red).bold()
        }
    } else {
        Style::default().fg(Color::Cyan).bold()
    };
    let p = Paragraph::new(Line::from(Span::styled(msg, style)));
    frame.render_widget(p, area);
}

/// Render the currently-running task list. Uses `state.running_order`
/// (an incrementally-maintained insertion-ordered vec) so no per-frame
/// sort is needed. `hard_cap` (min(10, area_h)) prevents runaway
/// scheduler configurations from pushing the viewport off-screen; if
/// there are more running tasks than fit, the last row is replaced with
/// a `(+N more running)` marker so users see the overflow instead of
/// silent clipping.
pub(crate) fn render_tasks(state: &State, frame: &mut Frame, area: Rect, now: Timestamp) {
    if area.height == 0 {
        return;
    }
    let area_h = (area.height as usize).max(1);
    let hard_cap = std::cmp::min(10, area_h);

    let total_running = state.running_order.len();
    let overflow = total_running > hard_cap;
    let task_slots = if overflow {
        hard_cap.saturating_sub(1)
    } else {
        hard_cap
    };

    let mut lines: Vec<Line> = Vec::with_capacity(hard_cap);
    for addr in state.running_order.iter().take(task_slots) {
        let task = match state.active.get(addr) {
            Some(t) => t,
            None => continue,
        };
        lines.push(task.to_line(now));
    }
    if overflow {
        let extra = total_running - task_slots;
        lines.push(Line::from(Span::styled(
            format!("  (+{extra} more running)"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.truncate(area_h);
    let p = Paragraph::new(lines);
    frame.render_widget(p, area);
}

/// Truncate `s` at `max` characters, appending `…` if truncation
/// happened. `max == 0` returns an empty string.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max == 0 {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

/// Move the prompt's `selected` field to the next visible option. The
/// available options are the subset of `ViewLog`, `Shell`, `Retry`, `Quit`
/// gated on the request flags.
pub(crate) fn next_selection(
    prompt: &super::store::Prompt,
    current: PromptSelect,
    forward: bool,
) -> PromptSelect {
    let mut order: Vec<PromptSelect> = Vec::with_capacity(4);
    if prompt.log_file.is_some() {
        order.push(PromptSelect::ViewLog);
    }
    if prompt.allow_shell {
        order.push(PromptSelect::Shell);
    }
    if prompt.allow_retry {
        order.push(PromptSelect::Retry);
    }
    order.push(PromptSelect::Quit);
    let idx = order.iter().position(|s| *s == current).unwrap_or(0);
    let next = if forward {
        (idx + 1) % order.len()
    } else {
        (idx + order.len() - 1) % order.len()
    };
    order[next]
}
