//! Interactive ratatui TUI: terminal I/O, event loop, and rendering. The
//! pure state machine lives in `app.rs`; this module wires crossterm events
//! into it and performs the tmux side effects the state machine requests.

use std::io::{self, Stdout};
use std::panic;
use std::time::Duration;

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

/// Redraw interval keeps the AGE column current between keypresses.
const AGE_REDRAW_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum tile card dimensions: width in columns, height in lines
/// (2 border + 4 content lines: title, counts, ready, spend — the floor that
/// keeps each line legible).
const MIN_TILE_WIDTH: u16 = 34;
const MIN_TILE_HEIGHT: u16 = 6;

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

/// One list build: refresh hook, rows, tile info, and usage, in that order so
/// the hook can update `@picker_tile_cmd` inputs and `@picker_usage`.
fn reload(app: &mut App, tmux: &Tmux) {
    model::run_refresh_hook(tmux);
    app.set_rows(model::build_rows(tmux));
    app.set_tile_info(model::build_tile_info(tmux));
    app.set_usage(model::read_usage(tmux));
}

/// Entry point: runs the interactive picker to completion. Never returns an
/// `Err` for tmux-side failures (per parity.md, a failed switch is silent);
/// only terminal setup I/O errors propagate.
pub fn run(tmux: &Tmux) -> io::Result<()> {
    model::run_refresh_hook(tmux);
    let rows = model::build_rows(tmux);
    let mut app = App::new(rows);
    app.set_tile_info(model::build_tile_info(tmux));
    app.set_usage(model::read_usage(tmux));

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

        if !event::poll(AGE_REDRAW_INTERVAL)? {
            continue;
        }
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
                reload(app, tmux);
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
                    reload(app, tmux);
                } else {
                    app.arm_confirm_kill(name, classification.tier, classification.reason);
                }
            }
            Effect::OpenPr(_) => {}
            Effect::JumpRoot => {
                actions::jump_root(tmux, None);
                return Ok(None);
            }
            Effect::Quit => return Ok(None),
            Effect::None => {}
        }
    }
}

/// Renders one frame: help line, optional usage line, table, prompt line, top to bottom.
fn draw(frame: &mut Frame, app: &App) {
    let now = model::now_epoch();
    draw_at(frame, app, now);
}

/// Internal draw function that takes a fixed `now` for testing.
fn draw_at(frame: &mut Frame, app: &App, now: u64) {
    let area = frame.area();
    let constraints = if app.usage().is_some() {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;
    draw_help_line(frame, chunks[idx], app);
    idx += 1;

    if let Some(usage_text) = app.usage() {
        let usage_line = Paragraph::new(Span::styled(
            usage_text.to_string(),
            Style::default().fg(Color::Cyan),
        ));
        frame.render_widget(usage_line, chunks[idx]);
        idx += 1;
    }

    if app.view() == View::Flat {
        draw_table(frame, chunks[idx], app, now);
    } else {
        draw_tiled(frame, chunks[idx], app, now);
    }
    idx += 1;

    draw_prompt_line(frame, chunks[idx], app);
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
fn draw_table(frame: &mut Frame, area: Rect, app: &App, now: u64) {
    let widths = render::compute_column_widths(app.rows(), now);
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
        .map(|(i, row)| data_line(row, &widths, i == app.selected(), now))
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

/// Computes tile grid dimensions given the number of tiles and available area.
/// Returns (cols, visible_rows): the number of columns and the number of visible
/// rows that fit in the area.
fn tile_grid_dims(num_tiles: usize, area: Rect) -> (usize, usize) {
    let cols = ((area.width / MIN_TILE_WIDTH).max(1) as usize).min(num_tiles.max(1));
    let total_rows = num_tiles.div_ceil(cols);
    let visible_rows = total_rows.min(((area.height / MIN_TILE_HEIGHT).max(1)) as usize);
    (cols, visible_rows)
}

/// Renders the tiled view. In `View::Tiles`, the tile grid fills the whole
/// area. In `View::Drilled`, a breadcrumb naming the selected project is
/// rendered on line 0, and the project's session list (header + rows) fills
/// the rest.
fn draw_tiled(frame: &mut Frame, area: Rect, app: &App, now: u64) {
    let tiles = app.tiles();

    match app.view() {
        View::Tiles => {
            let (cols, visible_rows) = tile_grid_dims(tiles.len(), area);
            let selected_row = app.tile_selected() / cols.max(1);
            let row_offset = data_scroll_offset(
                tiles.len().div_ceil(cols.max(1)),
                selected_row,
                visible_rows as u16,
            ) as usize;
            draw_tile_grid(frame, area, app, &tiles, cols, row_offset, visible_rows);
        }
        View::Drilled => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(0)])
                .split(area);
            let project_name = tiles
                .get(app.tile_selected())
                .map(|t| t.project.clone())
                .unwrap_or_default();
            let breadcrumb = format!("tiles › {}", project_name);
            frame.render_widget(
                Paragraph::new(breadcrumb).style(Style::default().add_modifier(Modifier::BOLD)),
                chunks[0],
            );
            draw_drilled_list(frame, chunks[1], app, now);
        }
        View::Flat => {} // Flat view is drawn by draw_table.
    }
}

/// Renders the project-tile cards, `cols` per row, starting at grid row
/// `row_offset`, for `visible_rows` rows. Cards stretch to fill grid cells
/// equally, with the last column and last row expanding to fill remaining space.
fn draw_tile_grid(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    tiles: &[crate::app::ProjectTile],
    cols: usize,
    row_offset: usize,
    visible_rows: usize,
) {
    let card_width = area.width / cols as u16;
    let card_height = area.height / visible_rows as u16;

    for row in 0..visible_rows {
        let tile_row = row_offset + row;
        for col in 0..cols {
            let tile_idx = tile_row * cols + col;
            let Some(tile) = tiles.get(tile_idx) else {
                continue;
            };

            let actual_width = if col == cols - 1 {
                area.width - (col as u16) * card_width
            } else {
                card_width
            };

            let actual_height = if row == visible_rows - 1 {
                area.height - (row as u16) * card_height
            } else {
                card_height
            };

            let card_area = Rect {
                x: area.x + (col as u16) * card_width,
                y: area.y + (row as u16) * card_height,
                width: actual_width,
                height: actual_height,
            };
            draw_tile_card(frame, card_area, app, tile, tile_idx);
        }
    }
}

/// Computes the style for a tile card based on selection. The grid is only
/// drawn in Tiles view, so selected tiles receive REVERSED + BOLD + yellow.
fn tile_card_style(selected: bool) -> Style {
    if !selected {
        return Style::default();
    }
    Style::default()
        .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        .fg(Color::Yellow)
}

fn draw_tile_card(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    tile: &crate::app::ProjectTile,
    tile_idx: usize,
) {
    let selected = tile_idx == app.tile_selected();
    let card_style = tile_card_style(selected);

    let mut title_spans = vec![Span::styled(
        tile.project.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if tile.blocked_count > 0 {
        title_spans.push(Span::styled(
            format!("  [{} blocked]", tile.blocked_count),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    let title_line = Line::from(title_spans);
    let rollup_line = Line::from(format!(
        "{} sess  {} unmerged  {} active  {}",
        tile.session_count, tile.unmerged_count, tile.active_count, tile.attn
    ));
    let ready_line = Line::from(format!("{} ready", tile.ready));
    let spend_line = Line::from(format!("{} spend", tile.spend));

    let block = Block::default().borders(Borders::ALL).style(card_style);
    let paragraph =
        Paragraph::new(vec![title_line, rollup_line, ready_line, spend_line]).block(block);
    frame.render_widget(paragraph, area);
}

/// Renders the tile-selected project's session list (header + rows), with
/// the row-selected styling applied only when focus is on `View::Drilled`.
fn draw_drilled_list(frame: &mut Frame, area: Rect, app: &App, now: u64) {
    let drilled: Vec<crate::model::SessionRow> = app.drilled_rows().into_iter().cloned().collect();
    let widths = render::compute_column_widths(&drilled, now);

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
        .map(|(i, row)| {
            data_line(
                row,
                &widths,
                drilled_focus && i == app.drill_selected(),
                now,
            )
        })
        .collect();

    let scroll = data_scroll_offset(data_lines.len(), app.drill_selected(), data_area.height);
    frame.render_widget(Paragraph::new(data_lines).scroll((scroll, 0)), data_area);
}

fn header_line(widths: &[usize; 12]) -> Line<'static> {
    let mut cells = vec![
        render::justify_right(render::HEADERS[0], widths[0]),
        render::justify_left(render::HEADERS[1], widths[1]),
        render::justify_left(render::HEADERS[2], widths[2]),
        render::justify_left(render::HEADERS[3], widths[3]),
        render::justify_left(render::HEADERS[4], widths[4]),
        render::justify_left(render::HEADERS[5], widths[5]),
        render::justify_left(render::HEADERS[6], widths[6]),
        render::justify_left(render::HEADERS[7], widths[7]),
        render::justify_left(render::HEADERS[8], widths[8]),
        render::justify_left(render::HEADERS[9], widths[9]),
        render::justify_left(render::HEADERS[10], widths[10]),
    ];
    if widths[11] > 0 {
        cells.push(render::justify_left(render::HEADERS[11], widths[11]));
    }
    Line::from(cells.join("  "))
}

fn data_line(
    row: &crate::model::SessionRow,
    widths: &[usize; 12],
    selected: bool,
    now: u64,
) -> Line<'static> {
    let base_style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    let idx_cell = render::justify_right(&row.idx.to_string(), widths[0]);
    let marker_cell = render::justify_left(&row.marker.to_string(), widths[1]);
    let session_cell = render::justify_left(&row.display_name, widths[2]);
    let attn_cell = render::justify_left(&row.attn, widths[3]);

    let age = model::age_cell(row.last_active, now);
    let age_style = if model::is_stale(row.last_active, now) {
        base_style.patch(Style::default().fg(Color::Red))
    } else {
        base_style
    };
    let age_cell = render::justify_left(&age, widths[4]);

    let runner_cell = render::justify_left(&row.runner, widths[5]);
    let phase_cell = render::justify_left(&row.phase, widths[6]);
    let wt_cell = render::justify_left(&row.wt, widths[7]);
    let project_cell = render::justify_left(&row.project, widths[8]);
    let branch_cell = render::justify_left(&row.branch, widths[9]);
    let status_cell = render::justify_left(&row.status, widths[10]);

    let branch_style = base_style.patch(Style::default().add_modifier(Modifier::DIM));
    let status_color = match row.status.as_str() {
        "merged" if row.phase == "-" => Some(Color::Green),
        "merged" => None,
        "unmerged" | "detached" => Some(Color::Yellow),
        _ => None,
    };
    let status_style = match status_color {
        Some(c) => base_style.patch(Style::default().fg(c)),
        None => base_style,
    };

    let phase_style = if row.phase == "blocked" {
        base_style.patch(Style::default().fg(Color::Red))
    } else {
        base_style
    };

    let mut spans = vec![
        Span::styled(idx_cell, base_style),
        Span::raw("  "),
        Span::styled(marker_cell, base_style),
        Span::raw("  "),
        Span::styled(session_cell, base_style),
        Span::raw("  "),
        Span::styled(attn_cell, base_style),
        Span::raw("  "),
        Span::styled(age_cell, age_style),
        Span::raw("  "),
        Span::styled(runner_cell, base_style),
        Span::raw("  "),
        Span::styled(phase_cell, phase_style),
        Span::raw("  "),
        Span::styled(wt_cell, base_style),
        Span::raw("  "),
        Span::styled(project_cell, base_style),
        Span::raw("  "),
        Span::styled(branch_cell, branch_style),
        Span::raw("  "),
        Span::styled(status_cell, status_style),
    ];
    if widths[11] > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            render::justify_left(&row.pr, widths[11]),
            base_style,
        ));
    }

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
            runner: "-".to_string(),
            phase: "-".to_string(),
            wt: "-".to_string(),
            project: "proj".to_string(),
            branch: "main".to_string(),
            status: status.to_string(),
            pr: String::new(),
            last_active: None,
        }
    }

    fn row_with(idx: usize, name: &str, project: &str, status: &str, attn: &str) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx,
            marker: '-',
            display_name: name.to_string(),
            attn: attn.to_string(),
            runner: "-".to_string(),
            phase: "-".to_string(),
            wt: "-".to_string(),
            project: project.to_string(),
            branch: "main".to_string(),
            status: status.to_string(),
            pr: String::new(),
            last_active: None,
        }
    }

    fn row_with_pr(
        idx: usize,
        name: &str,
        project: &str,
        status: &str,
        attn: &str,
        pr: &str,
    ) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx,
            marker: '-',
            display_name: name.to_string(),
            attn: attn.to_string(),
            runner: "-".to_string(),
            phase: "-".to_string(),
            wt: "-".to_string(),
            project: project.to_string(),
            branch: "main".to_string(),
            status: status.to_string(),
            pr: pr.to_string(),
            last_active: None,
        }
    }

    /// Starts an `App` and toggles it into `View::Flat` via `t`, the way a
    /// real session would after launching into `View::Tiles`. Use this for
    /// tests that exercise flat-view-only rendering.
    fn flat_app(rows: Vec<SessionRow>) -> App {
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t'));
        assert_eq!(app.view(), View::Flat);
        app
    }

    /// The buffer cell where the first data row's AGE value starts, located
    /// by the `AGE` header text so the test survives column-width changes.
    fn first_row_age_cell(terminal: &Terminal<TestBackend>) -> ratatui::buffer::Cell {
        let text = buffer_text(terminal);
        let header = text.lines().nth(1).expect("missing header line");
        let age_x = header.find("AGE").expect("AGE column not found in header") as u16;
        terminal.backend().buffer()[(age_x, 2)].clone()
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

    fn buffer_line(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut out = String::new();
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.trim_end().to_string()
    }

    #[test]
    fn normal_mode_renders_help_prompt_and_header() {
        let rows = vec![row(1, "alpha", "merged"), row(2, "beta", "unmerged")];
        let app = flat_app(rows);

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
        assert!(text.contains("RUNNER"));
        assert!(text.contains("PROJECT"));
        assert!(text.contains("BRANCH"));
        assert!(text.contains("STATUS"));
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
    }

    #[test]
    fn flat_view_runner_column_renders_value() {
        let mut row_with_runner = row(1, "sess-runner", "merged");
        row_with_runner.runner = "opencode".to_string();
        let app = flat_app(vec![row_with_runner]);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("RUNNER"), "header must contain RUNNER");
        assert!(text.contains("opencode"), "runner value must render");
    }

    #[test]
    fn insert_mode_renders_help_and_filter_prompt() {
        let rows = vec![row(1, "alpha", "merged")];
        let mut app = flat_app(rows);
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
        let mut app = flat_app(rows);
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
        let mut app = flat_app(rows);
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
        let mut app = flat_app(rows);
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
        let app = App::new(rows);

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
        let app = App::new(rows);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        // With 2 tiles at 120 wide: each card is 60 wide, starting at (0,1) and (60,1).
        // Interior cell of first (selected) tile: one column in, two rows down from top-left.
        let interior_cell = &buffer[(1, 2)];
        assert!(
            interior_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "selected tile interior should have REVERSED modifier in TILES view"
        );
        // Blank cell near the right edge of the first card (59 is the border).
        let blank_cell = &buffer[(58, 2)];
        assert_eq!(blank_cell.symbol(), " ");
        assert!(
            blank_cell.style().add_modifier.contains(Modifier::REVERSED),
            "fill should cover blank interior cells, not just text"
        );

        // Neighbor tile (second card starts at x=60) should not have REVERSED.
        let neighbor_cell = &buffer[(61, 2)];
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
    fn drilled_view_renders_project_scoped_session_list() {
        let rows = vec![
            row_with(1, "sess-aaa-1", "projx", "merged", "-"),
            row_with(2, "sess-bbb-1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
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
    fn drilled_view_runner_column_renders_value() {
        let mut row_with_runner = row_with(1, "sess-aaa-1", "projx", "merged", "-");
        row_with_runner.runner = "opencode".to_string();
        let mut app = App::new(vec![row_with_runner]);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("RUNNER"), "header must contain RUNNER");
        assert!(text.contains("opencode"), "runner value must render");
    }

    #[test]
    fn confirm_kill_prompt_renders_in_drilled_view() {
        let rows = vec![row_with(1, "sess-aaa-1", "projx", "unmerged", "-")];
        let mut app = App::new(rows);
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
            "breadcrumb should name the project:\n{text}"
        );
    }

    #[test]
    fn tile_grid_dims_unit_test() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(tile_grid_dims(2, area), (2, 1));
        assert_eq!(tile_grid_dims(5, area), (2, 3));
        assert_eq!(tile_grid_dims(12, area), (2, 4));
        assert_eq!(tile_grid_dims(1, area), (1, 1));
        assert_eq!(tile_grid_dims(0, area), (1, 0));
    }

    #[test]
    fn tile_grid_fills_area_for_2_5_and_12_tiles() {
        for num_tiles in [2, 5, 12] {
            let mut rows = Vec::new();
            for i in 0..num_tiles {
                rows.push(row_with(
                    i + 1,
                    &format!("sess-{i}"),
                    &format!("p{i}"),
                    "merged",
                    "-",
                ));
            }
            let app = App::new(rows);

            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();

            let text = buffer_text(&terminal);
            let (cols, visible_rows) = tile_grid_dims(num_tiles, Rect::new(0, 1, 80, 22));
            let max_visible = cols * visible_rows;

            let check_count = num_tiles.min(max_visible);
            for i in 0..check_count {
                assert!(
                    text.contains(&format!("p{i}")),
                    "project p{i} not found for {num_tiles} tiles (expected {check_count} visible)"
                );
            }

            let buffer = terminal.backend().buffer();
            let mut found_top_right = false;
            let mut found_bottom_right = false;

            // Check for top-right corner at x=79 (right edge)
            for y in 1..=22 {
                if buffer[(79, y as u16)].symbol() == "┐" {
                    found_top_right = true;
                    break;
                }
            }

            // Check for bottom-right corner at y=22 (bottom edge)
            for x in 0..80 {
                if buffer[(x as u16, 22)].symbol() == "┘" {
                    found_bottom_right = true;
                    break;
                }
            }

            assert!(
                found_top_right,
                "no ┐ at right edge (x=79) for {num_tiles} tiles"
            );
            assert!(
                found_bottom_right,
                "no ┘ at bottom edge (y=22) for {num_tiles} tiles"
            );
        }
    }

    #[test]
    fn tiles_view_renders_no_session_header() {
        let rows = vec![
            row_with(1, "a1", "projx", "merged", "-"),
            row_with(2, "b1", "projy", "merged", "-"),
        ];
        let app = App::new(rows);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            !text.contains("SESSION"),
            "SESSION header should not appear in Tiles view"
        );
        assert!(
            !text.contains("STATUS"),
            "STATUS header should not appear in Tiles view"
        );
    }

    #[test]
    fn drilled_view_replaces_grid_with_breadcrumb_list() {
        let rows = vec![
            row_with(1, "sess-aaa-1", "projx", "merged", "-"),
            row_with(2, "sess-bbb-1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.handle_key(Key::Enter); // Enter drilled view

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("tiles › projx"),
            "breadcrumb should show drilled project: {text}"
        );
        assert!(
            text.contains("SESSION"),
            "SESSION header should appear in drilled view"
        );
        assert!(
            text.contains("sess-aaa-1"),
            "drilled project's session should appear"
        );
        assert!(
            !text.contains("sess-bbb-1"),
            "other project's session should not appear"
        );
        assert!(
            !text.contains("projy"),
            "tile grid should be hidden in drilled view"
        );

        // Press 'h' to go back to Tiles view
        app.handle_key(Key::Char('h'));
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(
            text.contains("projy"),
            "back in Tiles view, should see other projects"
        );
        assert!(
            !text.contains("SESSION"),
            "SESSION header should not appear after returning to Tiles view"
        );
    }

    #[test]
    fn tiles_view_with_tile_info_renders_ready_values() {
        let rows = vec![
            row_with(1, "a1", "projx", "unmerged", "-"),
            row_with(2, "b1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        let mut info = std::collections::HashMap::new();
        info.insert(
            "projx".to_string(),
            crate::model::TileFields {
                ready: Some("3".to_string()),
                spend: Some("$4.20/24h".to_string()),
            },
        );
        app.set_tile_info(info);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("3 ready"),
            "projx tile should show '3 ready':\n{text}"
        );
        assert!(
            text.contains("- ready"),
            "projy tile should show '- ready':\n{text}"
        );
        assert!(
            text.contains("$4.20/24h spend"),
            "projx tile should show '$4.20/24h spend':\n{text}"
        );
        assert!(
            text.contains("- spend"),
            "projy tile should show '- spend':\n{text}"
        );
    }

    #[test]
    fn tiles_view_renders_active_count() {
        let mut a1 = row_with(1, "a1", "projx", "-", "-");
        a1.phase = "working".to_string();
        let mut a2 = row_with(2, "a2", "projx", "-", "-");
        a2.phase = "working".to_string();
        let a3 = row_with(3, "a3", "projx", "-", "-");
        let b1 = row_with(4, "b1", "projy", "-", "-");

        let app = App::new(vec![a1, a2, a3, b1]);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("2 active"),
            "projx tile should show '2 active':\n{text}"
        );
        assert!(
            text.contains("0 active"),
            "projy tile should show '0 active':\n{text}"
        );
    }

    #[test]
    fn tiles_view_marks_blocked_projects() {
        let mut a1 = row_with(1, "a1", "projx", "-", "-");
        a1.phase = "blocked".to_string();
        let a2 = row_with(2, "a2", "projx", "-", "-");
        let b1 = row_with(3, "b1", "projy", "-", "-");

        let app = App::new(vec![a1, a2, b1]);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("[1 blocked]"),
            "projx tile should show '[1 blocked]':\n{text}"
        );
        assert_eq!(
            text.matches("blocked").count(),
            1,
            "only the projx tile should carry a blocked marker:\n{text}"
        );
    }

    /// Foreground color of the STATUS cell on the frame line showing `name`.
    fn merged_fg(row: SessionRow) -> Option<Color> {
        let name = row.display_name.clone();
        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal.draw(|f| draw(f, &flat_app(vec![row]))).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height).find_map(|y| {
            let line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if !line.contains(&name) {
                return None;
            }
            let x = line.find("merged")?;
            buffer[(x as u16, y)].fg.into()
        })
    }

    /// Foreground color of the PHASE cell on the frame line showing `name`.
    fn phase_fg(row: SessionRow) -> Option<Color> {
        let name = row.display_name.clone();
        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal.draw(|f| draw(f, &flat_app(vec![row]))).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height).find_map(|y| {
            let line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if !line.contains(&name) {
                return None;
            }
            let phase_word = if line.contains("blocked") {
                "blocked"
            } else {
                "started"
            };
            let x = line.find(phase_word)?;
            buffer[(x as u16, y)].fg.into()
        })
    }

    #[test]
    fn merged_status_is_green_only_without_phase() {
        let unphased = row(1, "s1", "merged");
        let mut phased = row(1, "s1", "merged");
        phased.phase = "started".to_string();

        assert_eq!(merged_fg(unphased), Some(Color::Green));
        assert_ne!(merged_fg(phased), Some(Color::Green));
    }

    // --- usage header ---

    #[test]
    fn flat_view_renders_usage_line_under_help_when_set() {
        let rows = vec![row(1, "alpha", "merged")];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t')); // Tiles is the default; switch to flat
        app.set_usage(Some("claude 62% | opencode 3.1M tok".into()));

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let line_0 = buffer_line(&terminal, 0);
        let line_1 = buffer_line(&terminal, 1);
        let line_2 = buffer_line(&terminal, 2);

        assert!(
            line_0.contains("NORMAL —"),
            "line 0 should contain help: {}",
            line_0
        );
        assert!(
            line_1.contains("claude 62% | opencode 3.1M tok"),
            "line 1 should contain usage: {}",
            line_1
        );
        assert!(
            line_2.contains("SESSION"),
            "line 2 should contain header: {}",
            line_2
        );
    }

    #[test]
    fn tiles_view_renders_usage_line_under_help_when_set() {
        let rows = vec![
            row_with(1, "a1", "projx", "merged", "-"),
            row_with(2, "b1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.set_usage(Some("claude 62%".into()));

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let line_0 = buffer_line(&terminal, 0);
        let line_1 = buffer_line(&terminal, 1);

        assert!(
            line_0.contains("TILES —"),
            "line 0 should contain tiles help: {}",
            line_0
        );
        assert!(
            line_1.contains("claude 62%"),
            "line 1 should contain usage: {}",
            line_1
        );
    }

    #[test]
    fn header_unchanged_when_usage_unset() {
        let rows = vec![row(1, "alpha", "merged")];
        let mut app = App::new(rows);
        app.handle_key(Key::Char('t')); // Tiles is the default; switch to flat, do not set usage

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let line_0 = buffer_line(&terminal, 0);
        let line_1 = buffer_line(&terminal, 1);

        assert!(
            line_0.contains("NORMAL —"),
            "line 0 should contain help: {}",
            line_0
        );
        assert!(
            line_1.contains("SESSION"),
            "line 1 should contain header directly: {}",
            line_1
        );

        let text = buffer_text(&terminal);
        assert!(
            !text.contains("claude"),
            "usage text should not appear when unset"
        );
    }

    #[test]
    fn flat_view_renders_pr_column_when_any_row_has_pr() {
        let rows = vec![
            row_with_pr(1, "session-a", "proj", "merged", "-", "ci:fail"),
            row_with_pr(2, "session-b", "proj", "merged", "-", ""),
        ];
        let app = flat_app(rows);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("ci:fail"),
            "PR column must render pr value:\n{text}"
        );
        assert!(
            text.contains("STATUS  PR"),
            "header must contain PR:\n{text}"
        );
    }

    #[test]
    fn flat_view_omits_pr_column_when_no_row_has_pr() {
        let rows = vec![
            row_with(1, "session-a", "proj", "merged", "-"),
            row_with(2, "session-b", "proj", "merged", "-"),
        ];
        let app = flat_app(rows);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            !text.contains("STATUS  PR"),
            "header must not contain PR when no pr is set:\n{text}"
        );
    }

    #[test]
    fn drilled_view_renders_pr_column_in_session_list() {
        let rows = vec![
            row_with_pr(1, "sess-aaa-1", "projx", "merged", "-", "ci:pass rev:1/1"),
            row_with(2, "sess-bbb-1", "projy", "merged", "-"),
        ];
        let mut app = App::new(rows);
        app.handle_key(Key::Enter); // Enter drilled view

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(&terminal);
        assert!(
            text.contains("ci:pass rev:1/1"),
            "drilled view must show pr value in session list:\n{text}"
        );
        assert!(
            text.contains("sess-aaa-1"),
            "drilled view must show the session:\n{text}"
        );
    }

    #[test]
    fn flat_view_renders_age_column() {
        let mut rows = vec![row(1, "alpha", "merged")];
        rows[0].last_active = Some(1000 - 180);
        let app = flat_app(rows);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_at(f, &app, 1000)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("AGE"), "AGE header must be present:\n{text}");
        assert!(text.contains("3m"), "age value must be present:\n{text}");
    }

    #[test]
    fn age_column_renders_red_when_stale() {
        let mut rows = vec![row(1, "alpha", "merged")];
        rows[0].last_active = Some(10_000 - 3600);
        let app = flat_app(rows);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_at(f, &app, 10_000)).unwrap();

        let age_cell = first_row_age_cell(&terminal);
        assert_eq!(
            age_cell.symbol(),
            "1",
            "stale age (1h0m) should start with '1'"
        );
        assert_eq!(
            age_cell.style().fg,
            Some(Color::Red),
            "stale age should be red"
        );
    }

    #[test]
    fn age_column_renders_uncolored_when_fresh() {
        let mut rows = vec![row(1, "alpha", "merged")];
        rows[0].last_active = Some(1000 - 180);
        let app = flat_app(rows);

        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_at(f, &app, 1000)).unwrap();

        let age_cell = first_row_age_cell(&terminal);
        assert_eq!(
            age_cell.symbol(),
            "3",
            "fresh age (3m) should start with '3'"
        );
        assert_ne!(
            age_cell.style().fg,
            Some(Color::Red),
            "fresh age should not be red"
        );
    }

    #[test]
    fn blocked_phase_cell_is_red() {
        let mut blocked_row = row_with(1, "s1", "p1", "-", "-");
        blocked_row.phase = "blocked".to_string();

        let mut started_row = row_with(1, "s1", "p1", "-", "-");
        started_row.phase = "started".to_string();

        assert_eq!(phase_fg(blocked_row), Some(Color::Red));
        assert_ne!(phase_fg(started_row), Some(Color::Red));
    }
}
