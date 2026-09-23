//! Pure TUI state machine: mode, filter, selection, and the key-event ->
//! effect mapping described in docs/parity.md, "TUI behavior (modal)".
//!
//! This module knows nothing about tmux or the terminal; it operates on
//! `SessionRow`s already built by `model::build_rows` and reports intent via
//! `Effect`, so it's unit-testable without a real tmux server or terminal.

use crate::kill_safety::KillTier;
use crate::model::SessionRow;

/// Editing mode. Mirrors the two-mode contract from parity.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Tiered kill confirmation is armed; see `App::pending_kill`.
    ConfirmKill,
}

/// A kill request awaiting confirmation (armed by `App::arm_confirm_kill`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingKill {
    pub name: String,
    pub tier: KillTier,
    pub reason: String,
}

/// Which list layout the TUI is showing. `Flat` is the parity-contract
/// default; `Tiles`/`Drilled` are the two focus states of the tiled view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Flat,
    /// Tiled view with focus on the project-tile grid.
    Tiles,
    /// Tiled view drilled into the selected tile's session list.
    Drilled,
}

/// Per-project roll-up computed locally from the rows (no tm involvement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTile {
    pub project: String,
    pub session_count: usize,
    /// Rows whose status is exactly "unmerged".
    pub unmerged_count: usize,
    /// Concatenation (row order, no separator) of every row attn value that
    /// isn't the "-" placeholder; "-" when no session has attention flags.
    pub attn: String,
    /// Value printed by `@picker_tile_cmd` for this project, or "-" when the
    /// option is unset or the command produced nothing.
    pub ready: String,
}

/// Side effect requested by a key event. `None` means "handled internally,
/// nothing further to do" (e.g. moved selection, edited filter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Switch the tmux client to this session name and exit.
    Switch(String),
    /// Kill this session, then the caller must refresh rows.
    Kill(String),
    /// Classify this session's kill-safety tier before killing it.
    RequestKill(String),
    /// Jump to the root session of the CURRENT session, then exit.
    JumpRoot,
    /// Quit without switching.
    Quit,
    /// No externally visible effect; redraw only.
    None,
}

/// The full TUI state: all rows, the filter query, mode, and selection index
/// into the FILTERED view.
pub struct App {
    rows: Vec<SessionRow>,
    filter: String,
    mode: Mode,
    selected: usize,
    pending_kill: Option<PendingKill>,
    view: View,
    tile_selected: usize,
    drill_selected: usize,
    tile_info: std::collections::HashMap<String, String>,
}

/// Case-insensitive, non-contiguous subsequence match: every char of
/// `needle` (lowercased) must appear in `haystack` (lowercased) in order,
/// though not necessarily adjacently. Empty needle always matches.
pub fn subsequence_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay_lower = haystack.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let mut needle_chars = needle_lower.chars().peekable();
    for c in hay_lower.chars() {
        if let Some(&n) = needle_chars.peek() {
            if c == n {
                needle_chars.next();
            }
        } else {
            break;
        }
    }
    needle_chars.peek().is_none()
}

/// Concatenation of a row's visible cells used as the filter haystack.
fn row_haystack(row: &SessionRow) -> String {
    format!(
        "{}{}{}{}{}{}{}{}",
        row.display_name,
        row.attn,
        row.runner,
        row.phase,
        row.wt,
        row.project,
        row.branch,
        row.status
    )
}

impl App {
    pub fn new(rows: Vec<SessionRow>) -> Self {
        App {
            rows,
            filter: String::new(),
            mode: Mode::Normal,
            selected: 0,
            pending_kill: None,
            view: View::Flat,
            tile_selected: 0,
            drill_selected: 0,
            tile_info: std::collections::HashMap::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn rows(&self) -> &[SessionRow] {
        &self.rows
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn pending_kill(&self) -> Option<&PendingKill> {
        self.pending_kill.as_ref()
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn tile_selected(&self) -> usize {
        self.tile_selected
    }

    pub fn drill_selected(&self) -> usize {
        self.drill_selected
    }

    /// Groups ALL rows (not the filtered view) by project, in first-
    /// appearance order, with per-project roll-ups.
    pub fn tiles(&self) -> Vec<ProjectTile> {
        let mut tiles: Vec<ProjectTile> = Vec::new();
        let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for row in &self.rows {
            let idx = *index.entry(row.project.as_str()).or_insert_with(|| {
                tiles.push(ProjectTile {
                    project: row.project.clone(),
                    session_count: 0,
                    unmerged_count: 0,
                    attn: String::new(),
                    ready: String::new(),
                });
                tiles.len() - 1
            });
            let tile = &mut tiles[idx];
            tile.session_count += 1;
            if row.status == "unmerged" {
                tile.unmerged_count += 1;
            }
            if row.attn != "-" {
                tile.attn.push_str(&row.attn);
            }
        }
        for tile in &mut tiles {
            if tile.attn.is_empty() {
                tile.attn = "-".to_string();
            }
            tile.ready = self
                .tile_info
                .get(&tile.project)
                .cloned()
                .unwrap_or_else(|| "-".to_string());
        }
        tiles
    }

    /// Rows (original order) belonging to the currently tile-selected
    /// project; empty if there are no tiles.
    pub fn drilled_rows(&self) -> Vec<&SessionRow> {
        match self.tiles().get(self.tile_selected) {
            Some(tile) => self
                .rows
                .iter()
                .filter(|r| r.project == tile.project)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Arms tiered kill confirmation: records the pending kill and switches
    /// to `Mode::ConfirmKill`.
    pub fn arm_confirm_kill(&mut self, name: String, tier: KillTier, reason: String) {
        self.pending_kill = Some(PendingKill { name, tier, reason });
        self.mode = Mode::ConfirmKill;
    }

    /// Rows matching the current filter, in original order.
    pub fn filtered_rows(&self) -> Vec<&SessionRow> {
        self.rows
            .iter()
            .filter(|r| subsequence_match(&row_haystack(r), &self.filter))
            .collect()
    }

    /// Replaces the row list (e.g. after a refresh) and clamps selection,
    /// tile selection, and drill selection, falling back out of `Drilled`
    /// if the drilled project's rows have vanished.
    pub fn set_rows(&mut self, rows: Vec<SessionRow>) {
        let drilled_project = if self.view == View::Drilled {
            self.tiles()
                .get(self.tile_selected)
                .map(|t| t.project.clone())
        } else {
            None
        };
        self.rows = rows;
        self.clamp_selection();
        self.clamp_tile_selection();
        self.clamp_drill_selection();
        if let Some(project) = drilled_project {
            if !self.rows.iter().any(|r| r.project == project) {
                self.view = View::Tiles;
            }
        }
    }

    /// Sets the tile info (project -> ready value) used to fill ProjectTile.ready.
    pub fn set_tile_info(&mut self, info: std::collections::HashMap<String, String>) {
        self.tile_info = info;
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered_rows().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn clamp_tile_selection(&mut self) {
        let len = self.tiles().len();
        if len == 0 {
            self.tile_selected = 0;
        } else if self.tile_selected >= len {
            self.tile_selected = len - 1;
        }
    }

    fn clamp_drill_selection(&mut self) {
        let len = self.drilled_rows().len();
        if len == 0 {
            self.drill_selected = 0;
        } else if self.drill_selected >= len {
            self.drill_selected = len - 1;
        }
    }

    fn move_tile_selection(&mut self, delta: isize) {
        let len = self.tiles().len();
        if len == 0 {
            self.tile_selected = 0;
            return;
        }
        let cur = self.tile_selected as isize;
        let next = (cur + delta).clamp(0, len as isize - 1);
        self.tile_selected = next as usize;
        self.drill_selected = 0;
    }

    fn move_drill_selection(&mut self, delta: isize) {
        let len = self.drilled_rows().len();
        if len == 0 {
            self.drill_selected = 0;
            return;
        }
        let cur = self.drill_selected as isize;
        let next = (cur + delta).clamp(0, len as isize - 1);
        self.drill_selected = next as usize;
    }

    fn drilled_selected_name(&self) -> Option<String> {
        self.drilled_rows()
            .get(self.drill_selected)
            .map(|r| r.name.clone())
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered_rows().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected as isize;
        let next = (cur + delta).clamp(0, len as isize - 1);
        self.selected = next as usize;
    }

    fn selected_name(&self) -> Option<String> {
        self.filtered_rows()
            .get(self.selected)
            .map(|r| r.name.clone())
    }

    /// Selects filtered-row N (1-based); no-op if N exceeds visible rows.
    fn jump_to(&mut self, n: usize) {
        let len = self.filtered_rows().len();
        if n >= 1 && n <= len {
            self.selected = n - 1;
        }
    }

    fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.clamp_selection();
    }

    fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.clamp_selection();
    }

    /// Handles one key event (already decoded to a portable `Key`) and
    /// returns the effect the caller should perform, if any.
    pub fn handle_key(&mut self, key: Key) -> Effect {
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Insert => self.handle_insert_key(key),
            Mode::ConfirmKill => self.handle_confirm_kill_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: Key) -> Effect {
        match self.view {
            View::Flat => self.handle_normal_flat_key(key),
            View::Tiles => self.handle_tiles_key(key),
            View::Drilled => self.handle_drilled_key(key),
        }
    }

    fn handle_normal_flat_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Char('t') => {
                self.view = View::Tiles;
                self.clamp_tile_selection();
                Effect::None
            }
            Key::Char('j') | Key::Down => {
                self.move_selection(1);
                Effect::None
            }
            Key::Char('k') | Key::Up => {
                self.move_selection(-1);
                Effect::None
            }
            Key::Enter => match self.selected_name() {
                Some(name) => Effect::Switch(name),
                None => Effect::None,
            },
            Key::Char('x') => match self.selected_name() {
                Some(name) => Effect::RequestKill(name),
                None => Effect::None,
            },
            Key::Char('g') => Effect::JumpRoot,
            Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                let n = c.to_digit(10).unwrap() as usize;
                self.jump_to(n);
                match self.selected_name() {
                    Some(name) if self.filtered_rows().len() >= n => Effect::Switch(name),
                    _ => Effect::None,
                }
            }
            Key::Char('i') => {
                self.mode = Mode::Insert;
                Effect::None
            }
            Key::Char('q') | Key::Esc => Effect::Quit,
            _ => Effect::None,
        }
    }

    fn handle_tiles_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Char('t') => {
                self.view = View::Flat;
                Effect::None
            }
            Key::Char('h') | Key::Left => {
                self.move_tile_selection(-1);
                Effect::None
            }
            Key::Char('l') | Key::Right => {
                self.move_tile_selection(1);
                Effect::None
            }
            Key::Enter | Key::Char('j') | Key::Down => {
                if !self.drilled_rows().is_empty() {
                    self.view = View::Drilled;
                    self.drill_selected = 0;
                }
                Effect::None
            }
            Key::Char('q') | Key::Esc => Effect::Quit,
            Key::Char('g') => Effect::JumpRoot,
            _ => Effect::None,
        }
    }

    fn handle_drilled_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Char('j') | Key::Down => {
                self.move_drill_selection(1);
                Effect::None
            }
            Key::Char('k') | Key::Up => {
                self.move_drill_selection(-1);
                Effect::None
            }
            Key::Enter => match self.drilled_selected_name() {
                Some(name) => Effect::Switch(name),
                None => Effect::None,
            },
            Key::Char('x') => match self.drilled_selected_name() {
                Some(name) => Effect::RequestKill(name),
                None => Effect::None,
            },
            Key::Char('h') | Key::Left | Key::Esc => {
                self.view = View::Tiles;
                Effect::None
            }
            Key::Char('t') => {
                self.view = View::Flat;
                Effect::None
            }
            Key::Char('q') => Effect::Quit,
            Key::Char('g') => Effect::JumpRoot,
            _ => Effect::None,
        }
    }

    fn handle_insert_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Char(c) => {
                self.push_filter_char(c);
                Effect::None
            }
            Key::Backspace => {
                self.pop_filter_char();
                Effect::None
            }
            Key::Down => {
                self.move_selection(1);
                Effect::None
            }
            Key::Up => {
                self.move_selection(-1);
                Effect::None
            }
            Key::Enter => match self.selected_name() {
                Some(name) => Effect::Switch(name),
                None => Effect::None,
            },
            Key::Esc => {
                self.mode = Mode::Normal;
                Effect::None
            }
            Key::Left | Key::Right => Effect::None,
        }
    }

    /// `y`/`Y` confirms the pending kill; any other key cancels. Either way
    /// the pending kill is cleared and mode returns to `Normal`.
    fn handle_confirm_kill_key(&mut self, key: Key) -> Effect {
        let pending = self.pending_kill.take();
        self.mode = Mode::Normal;
        match key {
            Key::Char('y') | Key::Char('Y') => match pending {
                Some(p) => Effect::Kill(p.name),
                None => Effect::None,
            },
            _ => Effect::None,
        }
    }
}

/// A portable, terminal-library-agnostic key event. `ui.rs` translates
/// crossterm events into these so the state machine stays unit-testable
/// without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx: 1,
            marker: '-',
            display_name: name.to_string(),
            attn: "-".to_string(),
            runner: "-".to_string(),
            phase: "-".to_string(),
            wt: "-".to_string(),
            project: "-".to_string(),
            branch: "-".to_string(),
            status: "-".to_string(),
        }
    }

    fn rows(names: &[&str]) -> Vec<SessionRow> {
        names.iter().map(|n| row(n)).collect()
    }

    fn row_with(name: &str, project: &str, status: &str, attn: &str) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx: 1,
            marker: '-',
            display_name: name.to_string(),
            attn: attn.to_string(),
            runner: "-".to_string(),
            phase: "-".to_string(),
            wt: "-".to_string(),
            project: project.to_string(),
            branch: "-".to_string(),
            status: status.to_string(),
        }
    }

    // --- subsequence filter ---

    #[test]
    fn subsequence_matches_case_insensitively_and_non_contiguously() {
        assert!(subsequence_match("Alpha-Beta", "abt"));
        assert!(subsequence_match("Alpha-Beta", "ALPHABETA"));
        assert!(!subsequence_match("Alpha-Beta", "z"));
    }

    #[test]
    fn subsequence_empty_needle_always_matches() {
        assert!(subsequence_match("anything", ""));
        assert!(subsequence_match("", ""));
    }

    #[test]
    fn filtered_rows_use_subsequence_match_on_row_text() {
        let mut app = App::new(rows(&["alpha", "beta", "gamma"]));
        app.filter = "al".to_string();
        let names: Vec<&str> = app
            .filtered_rows()
            .into_iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn filter_matches_runner_token() {
        let mut row1 = row_with("aa", "bb", "-", "-");
        row1.runner = "opencode".to_string();
        let row2 = row_with("cc", "dd", "-", "-");

        let mut app = App::new(vec![row1, row2]);
        app.filter = "opencode".to_string();
        let filtered = app.filtered_rows();
        let names: Vec<&str> = filtered.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["aa"]);
    }

    // --- mode transitions ---

    #[test]
    fn i_enters_insert_mode() {
        let mut app = App::new(rows(&["alpha"]));
        assert_eq!(app.mode(), Mode::Normal);
        app.handle_key(Key::Char('i'));
        assert_eq!(app.mode(), Mode::Insert);
    }

    #[test]
    fn esc_in_insert_returns_to_normal_keeping_filter() {
        let mut app = App::new(rows(&["alpha", "beta"]));
        app.handle_key(Key::Char('i'));
        app.handle_key(Key::Char('a'));
        app.handle_key(Key::Char('l'));
        assert_eq!(app.filter(), "al");
        app.handle_key(Key::Esc);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.filter(), "al");
    }

    #[test]
    fn q_quits_only_in_normal_mode() {
        let mut app = App::new(rows(&["alpha"]));
        let effect = app.handle_key(Key::Char('q'));
        assert_eq!(effect, Effect::Quit);

        let mut app2 = App::new(rows(&["alpha"]));
        app2.handle_key(Key::Char('i'));
        let effect2 = app2.handle_key(Key::Char('q'));
        assert_eq!(effect2, Effect::None);
        assert_eq!(app2.filter(), "q");
    }

    // --- selection clamping ---

    #[test]
    fn selection_clamps_after_filter_shrinks_visible_rows() {
        let mut app = App::new(rows(&["alpha", "beta", "gamma"]));
        app.move_selection(2);
        assert_eq!(app.selected(), 2);
        app.filter = "beta".to_string();
        app.clamp_selection();
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn selection_clamps_after_row_removal() {
        let mut app = App::new(rows(&["alpha", "beta", "gamma"]));
        app.move_selection(2);
        assert_eq!(app.selected(), 2);
        app.set_rows(rows(&["alpha", "beta"]));
        assert_eq!(app.selected(), 1);
    }

    // --- digit jump ---

    #[test]
    fn digit_jump_resolves_nth_filtered_row() {
        // "ap" is a subsequence of "apple" and "grape" but not "banana".
        let mut app = App::new(rows(&["apple", "banana", "grape"]));
        app.filter = "ap".to_string();
        let filtered_names: Vec<&str> = app
            .filtered_rows()
            .into_iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(filtered_names, vec!["apple", "grape"]);

        let effect = app.handle_key(Key::Char('2'));
        assert_eq!(effect, Effect::Switch("grape".to_string()));
    }

    #[test]
    fn digit_jump_is_noop_when_n_exceeds_match_count() {
        let mut app = App::new(rows(&["alpha"]));
        let effect = app.handle_key(Key::Char('9'));
        assert_eq!(effect, Effect::None);
        assert_eq!(app.selected(), 0);
    }

    // --- x in insert mode ---

    #[test]
    fn x_in_insert_mode_edits_filter_instead_of_killing() {
        let mut app = App::new(rows(&["alpha", "xray"]));
        app.handle_key(Key::Char('i'));
        let effect = app.handle_key(Key::Char('x'));
        assert_eq!(effect, Effect::None);
        assert_eq!(app.filter(), "x");
    }

    // --- effects mapping in normal mode ---

    #[test]
    fn enter_in_normal_switches_to_selected() {
        let mut app = App::new(rows(&["alpha", "beta"]));
        app.move_selection(1);
        let effect = app.handle_key(Key::Enter);
        assert_eq!(effect, Effect::Switch("beta".to_string()));
    }

    #[test]
    fn x_in_normal_requests_kill_of_selected() {
        let mut app = App::new(rows(&["alpha", "beta"]));
        let effect = app.handle_key(Key::Char('x'));
        assert_eq!(effect, Effect::RequestKill("alpha".to_string()));
    }

    #[test]
    fn x_with_no_selection_is_noop() {
        let mut app = App::new(rows(&[]));
        let effect = app.handle_key(Key::Char('x'));
        assert_eq!(effect, Effect::None);
    }

    // --- tiered kill confirmation ---

    #[test]
    fn confirm_kill_y_kills_and_returns_to_normal() {
        let mut app = App::new(rows(&["alpha"]));
        app.arm_confirm_kill("alpha".to_string(), KillTier::LiveRun, "reason".to_string());
        assert_eq!(app.mode(), Mode::ConfirmKill);

        let effect = app.handle_key(Key::Char('y'));
        assert_eq!(effect, Effect::Kill("alpha".to_string()));
        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.pending_kill().is_none());
    }

    #[test]
    fn confirm_kill_uppercase_y_kills() {
        let mut app = App::new(rows(&["alpha"]));
        app.arm_confirm_kill("alpha".to_string(), KillTier::RootSession, String::new());

        let effect = app.handle_key(Key::Char('Y'));
        assert_eq!(effect, Effect::Kill("alpha".to_string()));
        assert_eq!(app.mode(), Mode::Normal);
    }

    #[test]
    fn confirm_kill_cancels_on_n_esc_q_or_digit() {
        for key in [Key::Char('n'), Key::Esc, Key::Char('q'), Key::Char('1')] {
            let mut app = App::new(rows(&["alpha"]));
            app.arm_confirm_kill("alpha".to_string(), KillTier::Unknown, "why".to_string());

            let effect = app.handle_key(key);
            assert_eq!(
                effect,
                Effect::None,
                "key {key:?} should cancel with no effect"
            );
            assert_eq!(
                app.mode(),
                Mode::Normal,
                "key {key:?} should return to Normal"
            );
            assert!(
                app.pending_kill().is_none(),
                "key {key:?} should clear pending kill"
            );
        }
    }

    #[test]
    fn g_in_normal_requests_jump_root() {
        let mut app = App::new(rows(&["alpha"]));
        let effect = app.handle_key(Key::Char('g'));
        assert_eq!(effect, Effect::JumpRoot);
    }

    #[test]
    fn esc_in_normal_quits() {
        let mut app = App::new(rows(&["alpha"]));
        let effect = app.handle_key(Key::Esc);
        assert_eq!(effect, Effect::Quit);
    }

    #[test]
    fn movement_keys_clamp_at_bounds() {
        let mut app = App::new(rows(&["alpha", "beta"]));
        app.handle_key(Key::Char('k'));
        assert_eq!(app.selected(), 0);
        app.handle_key(Key::Char('j'));
        app.handle_key(Key::Char('j'));
        app.handle_key(Key::Char('j'));
        assert_eq!(app.selected(), 1);
    }

    // --- tiled view ---

    #[test]
    fn tiles_group_by_project_in_first_appearance_order_with_rollups() {
        let app = App::new(vec![
            row_with("a1", "proj-a", "unmerged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
            row_with("a2", "proj-a", "merged", "!"),
        ]);
        let tiles = app.tiles();
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].project, "proj-a");
        assert_eq!(tiles[0].session_count, 2);
        assert_eq!(tiles[0].unmerged_count, 1);
        assert_eq!(tiles[0].attn, "!");
        assert_eq!(tiles[1].project, "proj-b");
        assert_eq!(tiles[1].session_count, 1);
        assert_eq!(tiles[1].unmerged_count, 0);
        assert_eq!(tiles[1].attn, "-");
    }

    #[test]
    fn t_in_normal_flat_enters_tiles_and_t_returns_to_flat() {
        let mut app = App::new(rows(&["alpha"]));
        assert_eq!(app.view(), View::Flat);
        let effect = app.handle_key(Key::Char('t'));
        assert_eq!(effect, Effect::None);
        assert_eq!(app.view(), View::Tiles);
        let effect2 = app.handle_key(Key::Char('t'));
        assert_eq!(effect2, Effect::None);
        assert_eq!(app.view(), View::Flat);
    }

    #[test]
    fn t_in_insert_mode_edits_filter_not_view() {
        let mut app = App::new(rows(&["alpha"]));
        app.handle_key(Key::Char('i'));
        let effect = app.handle_key(Key::Char('t'));
        assert_eq!(effect, Effect::None);
        assert_eq!(app.filter(), "t");
        assert_eq!(app.view(), View::Flat);
    }

    #[test]
    fn h_l_move_tile_selection_and_clamp() {
        let mut app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
        ]);
        app.handle_key(Key::Char('t'));
        assert_eq!(app.tile_selected(), 0);

        app.handle_key(Key::Char('h'));
        assert_eq!(app.tile_selected(), 0, "clamped at 0");

        app.drill_selected = 3;
        app.handle_key(Key::Char('l'));
        assert_eq!(app.tile_selected(), 1);
        assert_eq!(
            app.drill_selected(),
            0,
            "changing tile resets drill_selected"
        );

        app.handle_key(Key::Right);
        assert_eq!(app.tile_selected(), 1, "clamped at last tile");
    }

    #[test]
    fn enter_on_tile_drills_into_project_sessions() {
        let mut app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
            row_with("a2", "proj-a", "merged", "-"),
        ]);
        app.handle_key(Key::Char('t'));
        let effect = app.handle_key(Key::Enter);
        assert_eq!(effect, Effect::None);
        assert_eq!(app.view(), View::Drilled);
        let names: Vec<&str> = app
            .drilled_rows()
            .into_iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, vec!["a1", "a2"]);
    }

    #[test]
    fn drilled_j_k_move_and_enter_switches_to_selected_session() {
        let mut a1 = row_with("a1-name", "proj-a", "merged", "-");
        a1.display_name = "A1 Display".to_string();
        let mut a2 = row_with("a2-name", "proj-a", "merged", "-");
        a2.display_name = "A2 Display".to_string();
        let mut app = App::new(vec![a1, a2]);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);
        assert_eq!(app.view(), View::Drilled);
        assert_eq!(app.drill_selected(), 0);

        app.handle_key(Key::Char('j'));
        assert_eq!(app.drill_selected(), 1);
        app.handle_key(Key::Char('j'));
        assert_eq!(app.drill_selected(), 1, "clamp at last");

        app.handle_key(Key::Char('k'));
        assert_eq!(app.drill_selected(), 0);

        app.handle_key(Key::Char('j'));
        let effect = app.handle_key(Key::Enter);
        assert_eq!(effect, Effect::Switch("a2-name".to_string()));
    }

    #[test]
    fn drilled_x_requests_kill_of_selected_session() {
        let mut app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("a2", "proj-a", "merged", "-"),
        ]);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);
        app.handle_key(Key::Char('j'));
        let effect = app.handle_key(Key::Char('x'));
        assert_eq!(effect, Effect::RequestKill("a2".to_string()));
    }

    #[test]
    fn drilled_h_or_esc_returns_to_tiles_without_quitting() {
        for key in [Key::Char('h'), Key::Esc, Key::Left] {
            let mut app = App::new(vec![row_with("a1", "proj-a", "merged", "-")]);
            app.handle_key(Key::Char('t'));
            app.handle_key(Key::Enter);
            assert_eq!(app.view(), View::Drilled);
            let effect = app.handle_key(key);
            assert_eq!(effect, Effect::None);
            assert_eq!(
                app.view(),
                View::Tiles,
                "key {key:?} should return to Tiles"
            );
        }
    }

    #[test]
    fn esc_in_tiles_quits() {
        let mut app = App::new(rows(&["alpha"]));
        app.handle_key(Key::Char('t'));
        let effect = app.handle_key(Key::Esc);
        assert_eq!(effect, Effect::Quit);
    }

    #[test]
    fn q_in_drilled_quits() {
        let mut app = App::new(vec![row_with("a1", "proj-a", "merged", "-")]);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);
        let effect = app.handle_key(Key::Char('q'));
        assert_eq!(effect, Effect::Quit);
    }

    #[test]
    fn set_rows_falls_back_to_tiles_when_drilled_project_vanishes() {
        let mut app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
        ]);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);
        assert_eq!(app.view(), View::Drilled);

        app.set_rows(vec![row_with("b1", "proj-b", "merged", "-")]);
        assert_eq!(app.view(), View::Tiles);
        assert_eq!(app.tile_selected(), 0);
    }

    #[test]
    fn confirm_kill_flow_preserves_tiled_view() {
        let mut app = App::new(vec![row_with("a1", "proj-a", "merged", "-")]);
        app.handle_key(Key::Char('t'));
        app.handle_key(Key::Enter);
        assert_eq!(app.view(), View::Drilled);

        app.arm_confirm_kill("a1".to_string(), KillTier::LiveRun, "reason".to_string());
        assert_eq!(app.mode(), Mode::ConfirmKill);
        let effect = app.handle_key(Key::Char('y'));
        assert_eq!(effect, Effect::Kill("a1".to_string()));
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.view(), View::Drilled);

        app.arm_confirm_kill("a1".to_string(), KillTier::LiveRun, "reason".to_string());
        let effect2 = app.handle_key(Key::Char('n'));
        assert_eq!(effect2, Effect::None);
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.view(), View::Drilled);
    }

    #[test]
    fn digits_and_i_are_noops_in_tiled_views() {
        let mut app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
        ]);
        app.handle_key(Key::Char('t'));
        for key in [Key::Char('1'), Key::Char('i')] {
            let effect = app.handle_key(key);
            assert_eq!(effect, Effect::None);
            assert_eq!(app.mode(), Mode::Normal);
            assert_eq!(app.view(), View::Tiles);
            assert_eq!(app.tile_selected(), 0);
        }

        app.handle_key(Key::Enter);
        assert_eq!(app.view(), View::Drilled);
        for key in [Key::Char('1'), Key::Char('i')] {
            let effect = app.handle_key(key);
            assert_eq!(effect, Effect::None);
            assert_eq!(app.mode(), Mode::Normal);
            assert_eq!(app.view(), View::Drilled);
            assert_eq!(app.drill_selected(), 0);
        }
    }

    #[test]
    fn tiles_default_ready_to_dash_with_no_tile_info() {
        let app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
        ]);
        let tiles = app.tiles();
        assert_eq!(tiles[0].ready, "-");
        assert_eq!(tiles[1].ready, "-");
    }

    #[test]
    fn set_tile_info_fills_ready_values() {
        let mut app = App::new(vec![
            row_with("a1", "proj-a", "merged", "-"),
            row_with("b1", "proj-b", "merged", "-"),
        ]);
        let mut info = std::collections::HashMap::new();
        info.insert("proj-a".to_string(), "3".to_string());
        app.set_tile_info(info);

        let tiles = app.tiles();
        assert_eq!(tiles[0].project, "proj-a");
        assert_eq!(tiles[0].ready, "3");
        assert_eq!(tiles[1].project, "proj-b");
        assert_eq!(tiles[1].ready, "-");
    }
}
