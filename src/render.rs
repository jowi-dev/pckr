//! Plain TSV and colored-table rendering of session rows.

use crate::model::{self, SessionRow};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";

pub(crate) const HEADERS: [&str; 12] = [
    "#", " ", "SESSION", "ATTN", "AGE", "RUNNER", "PHASE", "WT", "PROJECT", "BRANCH", "STATUS",
    "PR",
];

/// Raw 11-field `\t`-delimited rows, one per line, no header. The 11th field
/// (at the end) is the age string or `-`.
pub fn to_plain_tsv(rows: &[SessionRow], now: u64) -> String {
    rows.iter()
        .map(|r| {
            let age = model::age_cell(r.last_active, now);
            let age_field = if age.is_empty() { "-".to_string() } else { age };
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                r.name,
                r.idx,
                r.marker,
                r.display_name,
                r.attn,
                r.wt,
                r.project,
                r.branch,
                r.status,
                r.runner,
                age_field
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Column widths for the 12 columns (`#`, marker, SESSION, ATTN, AGE, RUNNER,
/// PHASE, WT, PROJECT, BRANCH, STATUS, PR), computed from plain (uncolored)
/// text so ANSI escapes never affect alignment. Index 11 (PR column) is 0
/// when every row's pr is empty, else the max of "PR" header width and all
/// pr cell widths. Exposed for slice 2's TUI to reuse for its own layout.
pub fn compute_column_widths(rows: &[SessionRow], now: u64) -> [usize; 12] {
    let mut widths: [usize; 12] = HEADERS.map(|h| h.chars().count());
    for r in rows {
        let age = model::age_cell(r.last_active, now);
        let cells = [
            r.idx.to_string(),
            r.marker.to_string(),
            r.display_name.clone(),
            r.attn.clone(),
            age,
            r.runner.clone(),
            r.phase.clone(),
            r.wt.clone(),
            r.project.clone(),
            r.branch.clone(),
            r.status.clone(),
            r.pr.clone(),
        ];
        for (i, c) in cells.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    if rows.iter().all(|r| r.pr.is_empty()) {
        widths[11] = 0;
    }
    widths
}

/// Left-justifies `text`: pads with trailing spaces on the right.
pub(crate) fn justify_left(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(pad))
}

/// Right-justifies `text`: pads with leading spaces on the left.
pub(crate) fn justify_right(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(text.chars().count());
    format!("{}{text}", " ".repeat(pad))
}

/// Padded, ANSI-colored table: header row first, `#` right-justified, all other columns
/// left-justified, two-space column separators. `status` is green for `merged` only when
/// no phase is set (a phased `merged` is uncolored), yellow for `unmerged`/`detached`;
/// `phase` is red when `blocked`; `branch` is always dim; `age` is red when stale. Padding
/// spaces are appended outside color codes so trailing whitespace stays plain.
pub fn to_table(rows: &[SessionRow], now: u64) -> String {
    let widths = compute_column_widths(rows, now);

    let mut header_cells = vec![
        justify_right(HEADERS[0], widths[0]),
        justify_left(HEADERS[1], widths[1]),
        justify_left(HEADERS[2], widths[2]),
        justify_left(HEADERS[3], widths[3]),
        justify_left(HEADERS[4], widths[4]),
        justify_left(HEADERS[5], widths[5]),
        justify_left(HEADERS[6], widths[6]),
        justify_left(HEADERS[7], widths[7]),
        justify_left(HEADERS[8], widths[8]),
        justify_left(HEADERS[9], widths[9]),
        justify_left(HEADERS[10], widths[10]),
    ];
    if widths[11] > 0 {
        header_cells.push(justify_left(HEADERS[11], widths[11]));
    }
    let mut lines = vec![header_cells.join("  ").trim_end().to_string()];

    for r in rows {
        let idx_cell = justify_right(&r.idx.to_string(), widths[0]);
        let marker_cell = justify_left(&r.marker.to_string(), widths[1]);
        let session_cell = justify_left(&r.display_name, widths[2]);
        let attn_cell = justify_left(&r.attn, widths[3]);

        let age = model::age_cell(r.last_active, now);
        let age_pad = widths[4].saturating_sub(age.chars().count());
        let age_cell = if model::is_stale(r.last_active, now) {
            format!("{RED}{}{RESET}{}", age, " ".repeat(age_pad))
        } else {
            format!("{}{}", age, " ".repeat(age_pad))
        };

        let runner_cell = justify_left(&r.runner, widths[5]);
        let phase_cell = if r.phase == "blocked" {
            let pad = widths[6].saturating_sub(r.phase.chars().count());
            format!("{RED}{}{RESET}{}", r.phase, " ".repeat(pad))
        } else {
            justify_left(&r.phase, widths[6])
        };
        let wt_cell = justify_left(&r.wt, widths[7]);
        let project_cell = justify_left(&r.project, widths[8]);

        let branch_pad = widths[9].saturating_sub(r.branch.chars().count());
        let branch_cell = format!("{DIM}{}{RESET}{}", r.branch, " ".repeat(branch_pad));

        let status_color = match r.status.as_str() {
            "merged" if r.phase == "-" => Some(GREEN),
            "merged" => None,
            "unmerged" | "detached" => Some(YELLOW),
            _ => None,
        };
        let status_pad = widths[10].saturating_sub(r.status.chars().count());
        let status_cell = match status_color {
            Some(color) => format!("{color}{}{RESET}{}", r.status, " ".repeat(status_pad)),
            None => justify_left(&r.status, widths[10]),
        };

        let mut cells = vec![
            idx_cell,
            marker_cell,
            session_cell,
            attn_cell,
            age_cell,
            runner_cell,
            phase_cell,
            wt_cell,
            project_cell,
            branch_cell,
            status_cell,
        ];
        if widths[11] > 0 {
            cells.push(justify_left(&r.pr, widths[11]));
        }
        lines.push(cells.join("  ").trim_end().to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn row(
        idx: usize,
        marker: char,
        name: &str,
        attn: &str,
        runner: &str,
        phase: &str,
        wt: &str,
        project: &str,
        branch: &str,
        status: &str,
    ) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            idx,
            marker,
            display_name: name.to_string(),
            attn: attn.to_string(),
            runner: runner.to_string(),
            phase: phase.to_string(),
            wt: wt.to_string(),
            project: project.to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            pr: String::new(),
            last_active: None,
        }
    }

    #[test]
    fn plain_tsv_has_eleven_fields_and_no_header() {
        let rows = vec![row(1, '*', "s", "-", "-", "-", "-", "p", "m", "-")];
        let out = to_plain_tsv(&rows, 1000);
        assert_eq!(out, "s\t1\t*\ts\t-\t-\tp\tm\t-\t-\t-");
    }

    #[test]
    fn table_renders_padded_colored_header_and_rows() {
        let rows = vec![
            row(1, '*', "s", "-", "claude", "-", "-", "p", "m", "-"),
            row(2, '-', "t", "-", "-", "-", "-", "q", "n", "merged"),
        ];

        let output = to_table(&rows, 1000);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);

        // Header with all columns including AGE, RUNNER and PHASE
        let expected_header =
            "#     SESSION  ATTN  AGE  RUNNER  PHASE  WT  PROJECT  BRANCH  STATUS";
        assert_eq!(lines[0], expected_header);

        // Row 1 with "claude" runner
        let expected_row_1 =
            format!("1  *  s        -          claude  -      -   p        {DIM}m{RESET}       -");
        assert_eq!(lines[1], expected_row_1);

        // Row 2 with "merged" status
        let expected_row_2 = format!(
            "2  -  t        -          -       -      -   q        {DIM}n{RESET}       {GREEN}merged{RESET}"
        );
        assert_eq!(lines[2], expected_row_2);
    }

    #[test]
    fn status_unmerged_and_detached_are_yellow() {
        let rows = vec![row(1, '-', "a", "-", "-", "-", "-", "p", "b", "unmerged")];
        let out = to_table(&rows, 1000);
        let line = out.lines().nth(1).unwrap();
        assert!(line.contains(&format!("{YELLOW}unmerged{RESET}")));

        let rows2 = vec![row(1, '-', "a", "-", "-", "-", "-", "p", "b", "detached")];
        let out2 = to_table(&rows2, 1000);
        let line2 = out2.lines().nth(1).unwrap();
        assert!(line2.contains(&format!("{YELLOW}detached{RESET}")));
    }

    #[test]
    fn runner_column_renders_value_in_table() {
        let rows = vec![row(1, '-', "a", "-", "claude", "-", "-", "p", "b", "-")];
        let out = to_table(&rows, 1000);
        let line = out.lines().nth(1).unwrap();
        assert!(
            line.contains("claude"),
            "runner value should be rendered: {}",
            line
        );
    }

    #[test]
    fn phase_value_renders_in_phase_column() {
        let rows = vec![row(1, '-', "sess", "-", "-", "started", "-", "p", "b", "-")];
        let out = to_table(&rows, 1000);
        let line = out.lines().nth(1).unwrap();
        assert!(line.contains("started"), "phase value must render: {line}");
    }

    #[test]
    fn blocked_phase_renders_red() {
        // Blocked phase should be wrapped in RED color codes
        let rows = vec![row(1, '-', "sess", "-", "-", "blocked", "-", "p", "b", "-")];
        let out = to_table(&rows, 1000);
        let line = out.lines().nth(1).unwrap();
        assert!(
            line.contains(&format!("{RED}blocked{RESET}")),
            "blocked phase must be red: {line}"
        );

        // Started phase should NOT be wrapped in RED
        let rows2 = vec![row(1, '-', "sess", "-", "-", "started", "-", "p", "b", "-")];
        let out2 = to_table(&rows2, 1000);
        let line2 = out2.lines().nth(1).unwrap();
        assert!(
            !line2.contains(&format!("{RED}started{RESET}")),
            "started phase must not be red: {line2}"
        );
    }

    #[test]
    fn merged_status_with_phase_set_is_not_green() {
        let rows = vec![row(
            1, '-', "sess", "-", "-", "started", "-", "p", "b", "merged",
        )];
        let out = to_table(&rows, 1000);
        let line = out.lines().nth(1).unwrap();
        assert!(
            !line.contains(&format!("{GREEN}merged{RESET}")),
            "merged with phase set must not be green: {line}"
        );
        assert!(
            line.ends_with("started  -   p        \x1b[2mb\x1b[0m       merged"),
            "merged with phase set must render plain: {line:?}"
        );
    }

    #[test]
    fn plain_tsv_omits_phase() {
        let rows = vec![row(1, '*', "s", "-", "-", "started", "-", "p", "m", "-")];
        let out = to_plain_tsv(&rows, 1000);
        assert_eq!(out, "s\t1\t*\ts\t-\t-\tp\tm\t-\t-\t-");
    }

    #[test]
    fn table_appends_pr_column_when_any_row_has_pr() {
        let mut rows = vec![
            row(1, '*', "s", "-", "-", "-", "-", "p", "m", "-"),
            row(2, '-', "t", "-", "-", "-", "-", "q", "n", "merged"),
        ];
        rows[0].pr = "ci:pass rev:1/1".to_string();

        let output = to_table(&rows, 1000);
        let lines: Vec<&str> = output.lines().collect();

        // Check header contains PR column
        assert!(
            lines[0].ends_with("STATUS  PR"),
            "header must end with 'STATUS  PR': {}\n{}",
            lines[0],
            output
        );

        // Check first row (with pr) contains the pr value
        assert!(
            lines[1].ends_with("  ci:pass rev:1/1"),
            "row with pr must contain the pr value: {}\n{}",
            lines[1],
            output
        );

        // Check second row (without pr) has no trailing whitespace and doesn't contain "ci:"
        assert_eq!(lines[2], lines[2].trim_end(), "no trailing whitespace");
        assert!(
            !lines[2].contains("ci:"),
            "row without pr must not contain ci:: {}\n{}",
            lines[2],
            output
        );
    }

    #[test]
    fn table_has_no_pr_column_when_no_row_has_pr() {
        let rows = vec![
            row(1, '*', "s", "-", "-", "-", "-", "p", "m", "-"),
            row(2, '-', "t", "-", "-", "-", "-", "q", "n", "merged"),
        ];

        let output = to_table(&rows, 1000);
        let lines: Vec<&str> = output.lines().collect();

        assert!(
            lines[0].ends_with("STATUS"),
            "header must end at STATUS when no row has pr: {}",
            lines[0]
        );
    }

    #[test]
    fn plain_tsv_ignores_pr() {
        let mut row = row(1, '*', "s", "-", "-", "-", "-", "p", "m", "-");
        row.pr = "ci:fail".to_string();

        let out = to_plain_tsv(&[row], 1000);
        let fields: Vec<&str> = out.split('\t').collect();

        assert_eq!(
            fields.len(),
            11,
            "plain TSV must have exactly 11 fields even with pr set: {}",
            out
        );
    }

    #[test]
    fn plain_tsv_appends_age_field_or_dash() {
        let mut rows = vec![row(1, '*', "s", "-", "-", "-", "-", "p", "m", "-")];
        rows[0].last_active = Some(1000 - 180);
        let out = to_plain_tsv(&rows, 1000);
        assert_eq!(out, "s\t1\t*\ts\t-\t-\tp\tm\t-\t-\t3m");

        let mut rows2 = vec![row(1, '*', "s", "-", "-", "-", "-", "p", "m", "-")];
        rows2[0].last_active = None;
        let out2 = to_plain_tsv(&rows2, 1000);
        assert_eq!(out2, "s\t1\t*\ts\t-\t-\tp\tm\t-\t-\t-");
    }

    #[test]
    fn age_column_renders_uncolored_when_fresh() {
        let mut rows = vec![row(1, '-', "a", "-", "-", "-", "-", "p", "b", "-")];
        rows[0].last_active = Some(1000 - 180);
        let out = to_table(&rows, 1000);
        let line = out.lines().nth(1).unwrap();
        // age is 3m, padded to width 3 => "3m "
        let expected_age = "3m ";
        assert!(
            line.contains(expected_age),
            "age should be '3m ' but got: {}",
            line
        );
        assert!(!line.contains(&format!("{RED}3m{RESET}")));
    }
}
