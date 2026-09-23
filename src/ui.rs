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
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crate::actions;
use crate::app::{App, Effect, Key, Mode, View};
use crate::kill_safety::{self, KillTier};
use crate::model;
use crate::render;
use crate::tmux::Tmux;

const NORMAL_HELP: &str = "NORMAL — enter:switch | x:kill | g:root | t:tiles | 1-9:jump | i:filter | q/esc:quit | [merged]=safe to close";
const TILES_HELP: &str = "TILES — h/l:project | enter:open | t:flat | g:root | q/esc:quit";
const DRILLED_HELP: &str =
    "SESSIONS — j/k:move | enter:switch | x:kill | h/esc:back | t:flat | q:quit";
const INSERT_HELP: &str = "INSERT — type to filter | enter:switch | esc:normal mode";
const CONFIRM_HELP: &str = "y=kill  any other key=cancel";

/// Fixed tile card size: width in columns, height in lines (2 border + 2
/// content lines).
const TILE_WIDTH: u16 = 28;
const TILE_HEIGHT: u16 = 4;
/// Minimum lines reserved for the detail session list below the tile grid.
const MIN_DETAIL_HEIGHT: u16 = 6;

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
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
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
            Effect::RequestKill(name) => {
                // The CURRENT session is a silent no-op, checked BEFORE
                // classification so we never shell out to `tm` for it.
                if tmux.current_session_name().as_deref() == Some(name.as_str()) {
                    continue;
                }
                let classification = kill_safety::classify(&name);
                if classification.tier == KillTier::Safe {
                    actions::kill_session(tmux, &name);
                    model::run_refresh_hook(tmux);
                    let rows = model::build_rows(tmux);
                    app.set_rows(rows);
                } else {
                    app.arm_confirm_kill(name, classification.tier, classification.reason);
                }
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

    draw_help_line(frame, chunks[0], app);
    if app.view() == View::Flat {
        draw_table(frame, chunks[1], app);
    } else {
        draw_tiled(frame, chunks[1], app);
    }
    draw_prompt_line(frame, chunks[2], app);
}

fn draw_help_line(frame: &mut Frame, area: Rect, app: &App) {
    let text = match app.mode() {
        Mode::ConfirmKill => CONFIRM_HELP,
        Mode::Insert => INSERT_HELP,
        Mode::Normal => match app.view() {
            View::Flat => NORMAL_HELP,
            View::Tiles => TILES_HELP,
            View::Drilled => DRILLED_HELP,
        },
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_prompt_line(frame: &mut Frame, area: Rect, app: &App) {
    let text = match app.mode() {
        Mode::ConfirmKill => confirm_kill_prompt(app),
        Mode::Insert => format!("[I] filter > {}", app.filter()),
        Mode::Normal => match app.view() {
            View::Flat => "[N] session > ".to_string(),
            View::Tiles => "[T] project > ".to_string(),
            View::Drilled => "[T] session > ".to_string(),
        },
    };
    frame.render_widget(Paragraph::new(text), area);
    if app.mode() == Mode::Insert {
        let cursor_x = area.x + "[I] filter > ".len() as u16 + app.filter().chars().count() as u16;
        frame.set_cursor_position((cursor_x, area.y));
    }
}

/// Builds the confirmation question for the armed pending kill, e.g.
/// `Kill 'alpha' + worktree? [live run] session is running a live task`.
/// Omits the trailing reason cleanly when it's empty.
fn confirm_kill_prompt(app: &App) -> String {
    let Some(pending) = app.pending_kill() else {
        return String::new();
    };
    let label = match pending.tier {
        KillTier::LiveRun => "live run",
        KillTier::RootSession => "root session",
        // Safe never arms a confirmation; grouped with Unknown only so the
        // match stays exhaustive.
        KillTier::Unknown | KillTier::Safe => "unclassified",
    };
    if pending.reason.is_empty() {
        format!("Kill '{}' + worktree? [{}]", pending.name, label)
    } else {
        format!(
            "Kill '{}' + worktree? [{}] {}",
            pending.name, label, pending.reason
        )
    }
}

/// Renders the header (pinned to the first line of `area`) and the data
/// rows (scrolled, in the remaining lines) so that the header is always
/// visible and the selected row is always within the viewport, even when
/// the filtered row count exceeds the available height.
fn draw_table(frame: &mut Frame, area: Rect, app: &App) {
    let widths = render::compute_column_widths(app.rows());
    let filtered = app.filtered_rows();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let header_area = chunks[0];
    let data_area = chunks[1];

    frame.render_widget(Paragraph::new(header_line(&widths)), header_area);

    let data_lines: Vec<Line> = filtered
        .iter()
        .enumerate()
        .map(|(i, row)| data_line(row, &widths, i == app.selected()))
        .collect();

    let scroll = data_scroll_offset(data_lines.len(), app.selected(), data_area.height);
    frame.render_widget(Paragraph::new(data_lines).scroll((scroll, 0)), data_area);
}

/// Computes the vertical scroll offset (in data-row lines) needed to keep
/// the selected row within a viewport of `viewport_height` data rows. The
/// header is rendered separately and is therefore always visible
/// regardless of this offset.
fn data_scroll_offset(num_rows: usize, selected: usize, viewport_height: u16) -> u16 {
    if num_rows == 0 || viewport_height == 0 {
        return 0;
    }
    let num_rows = num_rows as u16;
    if num_rows <= viewport_height {
        return 0;
    }
    let selected = selected as u16;
    let max_offset = num_rows - viewport_height;
    // Keep `selected` within [offset, offset + viewport_height).
    let min_offset_for_visibility = selected.saturating_sub(viewport_height - 1);
    min_offset_for_visibility.min(max_offset)
}

/// Renders the tiled master-detail layout: a project-tile grid on top and
/// the tile-selected project's session list below. Used for `View::Tiles`
/// and `View::Drilled`; the two differ only in which half is highlighted.
fn draw_tiled(frame: &mut Frame, area: Rect, app: &App) {
    let tiles = app.tiles();
    let cols = (area.width / TILE_WIDTH).max(1) as usize;
    let total_rows = tiles.len().div_ceil(cols.max(1));

    let max_grid_height = area.height.saturating_sub(MIN_DETAIL_HEIGHT);
    let max_visible_rows = (max_grid_height / TILE_HEIGHT) as usize;
    let visible_rows = total_rows.min(max_visible_rows);

    let selected_row = app.tile_selected() / cols.max(1);
    let row_offset = data_scroll_offset(total_rows, selected_row, visible_rows as u16) as usize;

    let grid_height = (visible_rows as u16) * TILE_HEIGHT;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(grid_height), Constraint::Min(0)])
        .split(area);
    let grid_area = chunks[0];
    let detail_area = chunks[1];

    draw_tile_grid(
        frame,
        grid_area,
        app,
        &tiles,
        cols,
        row_offset,
        visible_rows,
    );
    draw_drilled_list(frame, detail_area, app);
}

/// Renders the project-tile cards, `cols` per row, starting at grid row
/// `row_offset`, for `visible_rows` rows.
fn draw_tile_grid(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    tiles: &[crate::app::ProjectTile],
    cols: usize,
    row_offset: usize,
    visible_rows: usize,
) {
    for row in 0..visible_rows {
        let tile_row = row_offset + row;
        for col in 0..cols {
            let tile_idx = tile_row * cols + col;
            let Some(tile) = tiles.get(tile_idx) else {
                continue;
            };
            let x = area.x + (col as u16) * TILE_WIDTH;
            if x >= area.x + area.width {
                continue;
            }
            let width = TILE_WIDTH.min(area.x + area.width - x);
            let card_area = Rect {
                x,
                y: area.y + (row as u16) * TILE_HEIGHT,
                width,
                height: TILE_HEIGHT,
            };
            draw_tile_card(frame, card_area, app, tile, tile_idx);
        }
    }
}

/// Computes the style for a tile card based on selection and view focus.
fn tile_card_style(selected: bool, view: View) -> Style {
    if !selected {
        return Style::default();
    }
    match view {
        View::Tiles => Style::default()
            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
            .fg(Color::Yellow),
        View::Drilled => Style::default().bg(Color::DarkGray).fg(Color::Yellow),
        View::Flat => Style::default().fg(Color::Yellow),
    }
}

fn draw_tile_card(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    tile: &crate::app::ProjectTile,
    tile_idx: usize,
) {
    let selected = tile_idx == app.tile_selected();
    let card_style = tile_card_style(selected, app.view());

    let title_line = Line::from(Span::styled(
        tile.project.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let rollup_line = Line::from(format!(
        "{} sess  {} unmerged  {}",
        tile.session_count, tile.unmerged_count, tile.attn
    ));

    let block = Block::default().borders(Borders::ALL).style(card_style);
    let paragraph = Paragraph::new(vec![title_line, rollup_line]).block(block);
    frame.render_widget(paragraph, area);
}

/// Renders the tile-selected project's session list (header + rows), with
/// the row-selected styling applied only when focus is on `View::Drilled`.
fn draw_drilled_list(frame: &mut Frame, area: Rect, app: &App) {
    let drilled: Vec<crate::model::SessionRow> = app.drilled_rows().into_iter().cloned().collect();
    let widths = render::compute_column_widths(&drilled);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let header_area = chunks[0];
    let data_area = chunks[1];

    frame.render_widget(Paragraph::new(header_line(&widths)), header_area);

    let drilled_focus = app.view() == View::Drilled;
    let data_lines: Vec<Line> = drilled
        .iter()
        .enumerate()
        .map(|(i, row)| data_line(row, &widths, drilled_focus && i == app.drill_selected()))
        .collect();

    let scroll = data_scroll_offset(data_lines.len(), app.drill_selected(), data_area.height);
    frame.render_widget(Paragraph::new(data_lines).scroll((scroll, 0)), data_area);
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

    fn row_with(idx: usize, name: &str, project: &str, status: &str, attn: &str) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx,
            marker: '-',
            display_name: name.to_string(),
            attn: attn.to_string(),
            wt: "-".to_string(),
            project: project.to_string(),
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
            "NORMAL — enter:switch | x:kill | g:root | t:tiles | 1-9:jump | i:filter | q/esc:quit | [merged]=safe to close"
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

    #[test]
    fn confirm_kill_mode_renders_help_and_prompt() {
        let rows = vec![row(1, "alpha", "unmerged")];
        let mut app = App::new(rows);
        app.arm_confirm_kill(
            "alpha".to_string(),
            crate::kill_safety::KillTier::LiveRun,
            "session is running a live task".to_string(),
        );

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("y=kill  any other key=cancel"));
        assert!(text.contains("Kill 'alpha' + worktree? [live run] session is running a live task"));
    }

    #[test]
    fn confirm_kill_mode_omits_trailing_space_when_reason_empty() {
        let rows = vec![row(1, "alpha", "unmerged")];
        let mut app = App::new(rows);
        app.arm_confirm_kill(
            "alpha".to_string(),
            crate::kill_safety::KillTier::RootSession,
            String::new(),
        );

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Kill 'alpha' + worktree? [root session]"));
        assert!(!text.contains("[root session] \n"));
    }

    #[test]
    fn table_scrolls_to_keep_selected_row_visible_and_header_pinned() {
        let rows: Vec<SessionRow> = (0..30)
            .map(|i| row(i + 1, &format!("session-{i}"), "merged"))
            .collect();
        let mut app = App::new(rows);
        for _ in 0..25 {
            app.handle_key(Key::Char('j'));
        }
        assert_eq!(app.selected(), 25);

        // 10-line-high layout: help(1) + table(8) + prompt(1).
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("SESSION"), "header row must stay visible");
        assert!(
            text.contains("session-25"),
            "selected row must be within the viewport:\n{text}"
        );
    }

    #[test]
    fn tiles_view_renders_tile_grid_with_rollups_and_placeholder() {
        let rows = vec![
            row_with(1, "a1", "projx", "unmerged", "-"),
            row_with(2, "b1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t'));

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("projx"), "missing project name:\n{text}");
        assert!(text.contains("projy"), "missing project name:\n{text}");
        assert!(text.contains("1 sess"), "missing roll-up text:\n{text}");
        assert!(text.contains("-"), "missing attn placeholder:\n{text}");
        assert!(text.contains(TILES_HELP));
        assert!(text.contains("[T] project >"));
    }

    #[test]
    fn tiles_view_fills_selected_tile_with_reversed() {
        let rows = vec![
            row_with(1, "a1", "projx", "merged", "-"),
            row_with(2, "b1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t'));

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        // Interior cell of first (selected) tile: one column in, two rows down from top-left.
        // Tile grid starts at buffer row 1, card interior starts at row 2.
        let interior_cell = &buffer[(1, 2)];
        assert!(
            interior_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "selected tile interior should have REVERSED modifier in TILES view"
        );
        // Blank cell past the title text: the fill must cover the whole card.
        let blank_cell = &buffer[(TILE_WIDTH - 2, 2)];
        assert_eq!(blank_cell.symbol(), " ");
        assert!(
            blank_cell.style().add_modifier.contains(Modifier::REVERSED),
            "fill should cover blank interior cells, not just text"
        );

        // Neighbor tile (next column) should not have REVERSED.
        let neighbor_cell = &buffer[(TILE_WIDTH + 1, 2)];
        assert!(
            !neighbor_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "unselected tile should not have REVERSED modifier"
        );
        assert!(
            neighbor_cell.style().bg.is_none() || neighbor_cell.style().bg == Some(Color::Reset),
            "unselected tile interior should have no bg set"
        );
    }

    #[test]
    fn drilled_view_dims_selected_tile_with_darkgray() {
        let rows = vec![
            row_with(1, "a1", "projx", "merged", "-"),
            row_with(2, "b1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter); // Enter drilled view

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        // Interior cell of selected tile in drilled view.
        let interior_cell = &buffer[(1, 2)];
        assert_eq!(
            interior_cell.style().bg,
            Some(Color::DarkGray),
            "selected tile interior should have DarkGray bg in DRILLED view"
        );
        assert!(
            !interior_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "selected tile should not have REVERSED in DRILLED view"
        );

        // Neighbor tile should not have DarkGray bg.
        let neighbor_cell = &buffer[(TILE_WIDTH + 1, 2)];
        assert_ne!(
            neighbor_cell.style().bg,
            Some(Color::DarkGray),
            "unselected tile should not have DarkGray bg"
        );
    }

    #[test]
    fn drilled_view_renders_project_scoped_session_list() {
        let rows = vec![
            row_with(1, "sess-aaa-1", "projx", "merged", "-"),
            row_with(2, "sess-bbb-1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("sess-aaa-1"),
            "missing drilled session:\n{text}"
        );
        assert!(
            !text.contains("sess-bbb-1"),
            "other project's session leaked into drilled view:\n{text}"
        );
        assert!(text.contains(DRILLED_HELP));
        assert!(text.contains("[T] session >"));
        assert!(
            text.contains("SESSION"),
            "detail header must render:\n{text}"
        );
    }

    #[test]
    fn confirm_kill_prompt_renders_in_drilled_view() {
        let rows = vec![row_with(1, "sess-aaa-1", "projx", "unmerged", "-")];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);
        app.arm_confirm_kill(
            "sess-aaa-1".to_string(),
            crate::kill_safety::KillTier::LiveRun,
            "session is running a live task".to_string(),
        );

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains(CONFIRM_HELP));
        assert!(text
            .contains("Kill 'sess-aaa-1' + worktree? [live run] session is running a live task"));
        assert!(
            text.contains("projx"),
            "tile grid should stay visible:\n{text}"
        );
    }
}
