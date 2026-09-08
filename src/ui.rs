//! Interactive ratatui TUI: terminal I/O, event loop, and rendering. The
//! pure state machine lives in `app.rs`; this module wires crossterm events
//! into it and performs the tmux side effects the state machine requests.

use std::io::{self, Stdout};
use std::panic;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use crate::actions;
use crate::app::{App, Effect, Key, Mode};
use crate::model;
use crate::render;
use crate::tmux::Tmux;

const NORMAL_HELP: &str = "NORMAL — enter:switch | x:kill | g:root | 1-9:jump | i:filter | q/esc:quit | [merged]=safe to close";
const INSERT_HELP: &str = "INSERT — type to filter | enter:switch | esc:normal mode";

/// Restores the terminal to its pre-TUI state (raw mode off, alternate
/// screen left). Best-effort: called on every exit path, including from the
/// panic hook, so failures here are swallowed.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Installs a panic hook that restores the terminal before delegating to
/// the previous (default) hook, so a panic mid-TUI never leaves the user's
/// terminal in raw/alternate-screen mode.
fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn key_from_event(code: KeyCode) -> Option<Key> {
    match code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        _ => None,
    }
}

/// Entry point: runs the interactive picker to completion. Never returns an
/// `Err` for tmux-side failures (per parity.md, a failed switch is silent);
/// only terminal setup I/O errors propagate.
pub fn run(tmux: &Tmux) -> io::Result<()> {
    model::run_refresh_hook(tmux);
    let rows = model::build_rows(tmux);
    let mut app = App::new(rows);

    install_panic_hook();
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let outcome = event_loop(&mut terminal, &mut app, tmux);

    restore_terminal();

    if let Some(name) = outcome? {
        tmux.switch_client(&name);
    }

    Ok(())
}

/// Runs the event loop. Returns `Ok(Some(session_name))` if the loop ended
/// wanting to switch to a session (performed by the caller AFTER the
/// terminal is restored), `Ok(None)` for a plain quit/jump-root exit.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    tmux: &Tmux,
) -> io::Result<Option<String>> {
    loop {
        terminal.draw(|f| draw(f, app))?;

        let event = event::read()?;
        let Event::Key(key_event) = event else {
            continue;
        };
        if key_event.kind != KeyEventKind::Press {
            continue;
        }
        let Some(key) = key_from_event(key_event.code) else {
            continue;
        };

        match app.handle_key(key) {
            Effect::Switch(name) => return Ok(Some(name)),
            Effect::Kill(name) => {
                actions::kill_session(tmux, &name);
                model::run_refresh_hook(tmux);
                let rows = model::build_rows(tmux);
                app.set_rows(rows);
            }
            Effect::JumpRoot => {
                actions::jump_root(tmux, None);
                return Ok(None);
            }
            Effect::Quit => return Ok(None),
            Effect::None => {}
        }
    }
}

/// Renders one frame: help line, table, prompt line, top to bottom.
fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_help_line(frame, chunks[0], app.mode());
    draw_table(frame, chunks[1], app);
    draw_prompt_line(frame, chunks[2], app);
}

fn draw_help_line(frame: &mut Frame, area: Rect, mode: Mode) {
    let text = match mode {
        Mode::Normal => NORMAL_HELP,
        Mode::Insert => INSERT_HELP,
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_prompt_line(frame: &mut Frame, area: Rect, app: &App) {
    let text = match app.mode() {
        Mode::Normal => "[N] session > ".to_string(),
        Mode::Insert => format!("[I] filter > {}", app.filter()),
    };
    frame.render_widget(Paragraph::new(text), area);
    if app.mode() == Mode::Insert {
        let cursor_x = area.x + "[I] filter > ".len() as u16 + app.filter().chars().count() as u16;
        frame.set_cursor_position((cursor_x, area.y));
    }
}

fn draw_table(frame: &mut Frame, area: Rect, app: &App) {
    let widths = render::compute_column_widths(app.rows());
    let filtered = app.filtered_rows();

    let mut lines: Vec<Line> = Vec::with_capacity(filtered.len() + 1);
    lines.push(header_line(&widths));

    for (i, row) in filtered.iter().enumerate() {
        let selected = i == app.selected();
        lines.push(data_line(row, &widths, selected));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn header_line(widths: &[usize; 8]) -> Line<'static> {
    let cells = [
        render::justify_right(render::HEADERS[0], widths[0]),
        render::justify_left(render::HEADERS[1], widths[1]),
        render::justify_left(render::HEADERS[2], widths[2]),
        render::justify_left(render::HEADERS[3], widths[3]),
        render::justify_left(render::HEADERS[4], widths[4]),
        render::justify_left(render::HEADERS[5], widths[5]),
        render::justify_left(render::HEADERS[6], widths[6]),
        render::justify_left(render::HEADERS[7], widths[7]),
    ];
    Line::from(cells.join("  "))
}

fn data_line(row: &crate::model::SessionRow, widths: &[usize; 8], selected: bool) -> Line<'static> {
    let base_style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    let idx_cell = render::justify_right(&row.idx.to_string(), widths[0]);
    let marker_cell = render::justify_left(&row.marker.to_string(), widths[1]);
    let session_cell = render::justify_left(&row.display_name, widths[2]);
    let attn_cell = render::justify_left(&row.attn, widths[3]);
    let wt_cell = render::justify_left(&row.wt, widths[4]);
    let project_cell = render::justify_left(&row.project, widths[5]);
    let branch_cell = render::justify_left(&row.branch, widths[6]);
    let status_cell = render::justify_left(&row.status, widths[7]);

    let branch_style = base_style.patch(Style::default().add_modifier(Modifier::DIM));
    let status_color = match row.status.as_str() {
        "merged" => Some(Color::Green),
        "unmerged" | "detached" => Some(Color::Yellow),
        _ => None,
    };
    let status_style = match status_color {
        Some(c) => base_style.patch(Style::default().fg(c)),
        None => base_style,
    };

    let spans = vec![
        Span::styled(idx_cell, base_style),
        Span::raw("  "),
        Span::styled(marker_cell, base_style),
        Span::raw("  "),
        Span::styled(session_cell, base_style),
        Span::raw("  "),
        Span::styled(attn_cell, base_style),
        Span::raw("  "),
        Span::styled(wt_cell, base_style),
        Span::raw("  "),
        Span::styled(project_cell, base_style),
        Span::raw("  "),
        Span::styled(branch_cell, branch_style),
        Span::raw("  "),
        Span::styled(status_cell, status_style),
    ];

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionRow;
    use ratatui::backend::TestBackend;

    fn row(idx: usize, name: &str, status: &str) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx,
            marker: '-',
            display_name: name.to_string(),
            attn: "-".to_string(),
            wt: "-".to_string(),
            project: "proj".to_string(),
            branch: "main".to_string(),
            status: status.to_string(),
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn normal_mode_renders_help_prompt_and_header() {
        let rows = vec![row(1, "alpha", "merged"), row(2, "beta", "unmerged")];
        let app = App::new(rows);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains(
            "NORMAL — enter:switch | x:kill | g:root | 1-9:jump | i:filter | q/esc:quit | [merged]=safe to close"
        ));
        assert!(text.contains("[N] session >"));
        assert!(text.contains("SESSION"));
        assert!(text.contains("ATTN"));
        assert!(text.contains("PROJECT"));
        assert!(text.contains("BRANCH"));
        assert!(text.contains("STATUS"));
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
    }

    #[test]
    fn insert_mode_renders_help_and_filter_prompt() {
        let rows = vec![row(1, "alpha", "merged")];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('i'));
        app.handle_key(Key::Char('a'));
        app.handle_key(Key::Char('l'));

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("INSERT — type to filter | enter:switch | esc:normal mode"));
        assert!(text.contains("[I] filter > al"));
    }
}
