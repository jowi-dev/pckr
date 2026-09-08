//! Plain TSV and colored-table rendering of session rows.

use crate::model::SessionRow;

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";

pub(crate) const HEADERS: [&str; 8] = [
    "#", " ", "SESSION", "ATTN", "WT", "PROJECT", "BRANCH", "STATUS",
];

/// Raw 9-field `\t`-delimited rows, one per line, no header.
pub fn to_plain_tsv(rows: &[SessionRow]) -> String {
    rows.iter()
        .map(|r| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                r.name,
                r.idx,
                r.marker,
                r.display_name,
                r.attn,
                r.wt,
                r.project,
                r.branch,
                r.status
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Column widths for the 8 displayed table columns (`#`, marker, SESSION,
/// ATTN, WT, PROJECT, BRANCH, STATUS), computed from plain (uncolored) text
/// so ANSI escapes never affect alignment. Exposed for slice 2's TUI to
/// reuse for its own layout.
pub fn compute_column_widths(rows: &[SessionRow]) -> [usize; 8] {
    let mut widths: [usize; 8] = HEADERS.map(|h| h.chars().count());
    for r in rows {
        let cells = [
            r.idx.to_string(),
            r.marker.to_string(),
            r.display_name.clone(),
            r.attn.clone(),
            r.wt.clone(),
            r.project.clone(),
            r.branch.clone(),
            r.status.clone(),
        ];
        for (i, c) in cells.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
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

/// Padded, ANSI-colored table: header row first, `#` right-justified, all
/// other columns left-justified, two-space column separators. `status` is
/// green for `merged`, yellow for `unmerged`/`detached`; `branch` is always
/// dim. Padding spaces are appended outside the color codes so trailing
/// whitespace stays plain.
pub fn to_table(rows: &[SessionRow]) -> String {
    let widths = compute_column_widths(rows);

    let header_cells = [
        justify_right(HEADERS[0], widths[0]),
        justify_left(HEADERS[1], widths[1]),
        justify_left(HEADERS[2], widths[2]),
        justify_left(HEADERS[3], widths[3]),
        justify_left(HEADERS[4], widths[4]),
        justify_left(HEADERS[5], widths[5]),
        justify_left(HEADERS[6], widths[6]),
        justify_left(HEADERS[7], widths[7]),
    ];
    let mut lines = vec![header_cells.join("  ").trim_end().to_string()];

    for r in rows {
        let idx_cell = justify_right(&r.idx.to_string(), widths[0]);
        let marker_cell = justify_left(&r.marker.to_string(), widths[1]);
        let session_cell = justify_left(&r.display_name, widths[2]);
        let attn_cell = justify_left(&r.attn, widths[3]);
        let wt_cell = justify_left(&r.wt, widths[4]);
        let project_cell = justify_left(&r.project, widths[5]);

        let branch_pad = widths[6].saturating_sub(r.branch.chars().count());
        let branch_cell = format!("{DIM}{}{RESET}{}", r.branch, " ".repeat(branch_pad));

        let status_color = match r.status.as_str() {
            "merged" => Some(GREEN),
            "unmerged" | "detached" => Some(YELLOW),
            _ => None,
        };
        let status_pad = widths[7].saturating_sub(r.status.chars().count());
        let status_cell = match status_color {
            Some(color) => format!("{color}{}{RESET}{}", r.status, " ".repeat(status_pad)),
            None => justify_left(&r.status, widths[7]),
        };

        let cells = [
            idx_cell,
            marker_cell,
            session_cell,
            attn_cell,
            wt_cell,
            project_cell,
            branch_cell,
            status_cell,
        ];
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
            wt: wt.to_string(),
            project: project.to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn plain_tsv_has_nine_fields_and_no_header() {
        let rows = vec![row(1, '*', "s", "-", "-", "p", "m", "-")];
        let out = to_plain_tsv(&rows);
        assert_eq!(out, "s\t1\t*\ts\t-\t-\tp\tm\t-");
    }

    #[test]
    fn table_renders_padded_colored_header_and_rows() {
        let rows = vec![
            row(1, '*', "s", "-", "-", "p", "m", "-"),
            row(2, '-', "t", "-", "-", "q", "n", "merged"),
        ];

        let output = to_table(&rows);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);

        assert_eq!(lines[0], "#     SESSION  ATTN  WT  PROJECT  BRANCH  STATUS");

        let mut expected_row_a = String::new();
        expected_row_a.push_str("1  *  s");
        expected_row_a.push_str(&" ".repeat(8));
        expected_row_a.push('-');
        expected_row_a.push_str(&" ".repeat(5));
        expected_row_a.push('-');
        expected_row_a.push_str(&" ".repeat(3));
        expected_row_a.push('p');
        expected_row_a.push_str(&" ".repeat(8));
        expected_row_a.push_str(DIM);
        expected_row_a.push('m');
        expected_row_a.push_str(RESET);
        expected_row_a.push_str(&" ".repeat(7));
        expected_row_a.push('-');
        assert_eq!(lines[1], expected_row_a);

        let mut expected_row_b = String::new();
        expected_row_b.push_str("2  -  t");
        expected_row_b.push_str(&" ".repeat(8));
        expected_row_b.push('-');
        expected_row_b.push_str(&" ".repeat(5));
        expected_row_b.push('-');
        expected_row_b.push_str(&" ".repeat(3));
        expected_row_b.push('q');
        expected_row_b.push_str(&" ".repeat(8));
        expected_row_b.push_str(DIM);
        expected_row_b.push('n');
        expected_row_b.push_str(RESET);
        expected_row_b.push_str(&" ".repeat(7));
        expected_row_b.push_str(GREEN);
        expected_row_b.push_str("merged");
        expected_row_b.push_str(RESET);
        assert_eq!(lines[2], expected_row_b);
    }

    #[test]
    fn status_unmerged_and_detached_are_yellow() {
        let rows = vec![row(1, '-', "a", "-", "-", "p", "b", "unmerged")];
        let out = to_table(&rows);
        let line = out.lines().nth(1).unwrap();
        assert!(line.contains(&format!("{YELLOW}unmerged{RESET}")));

        let rows2 = vec![row(1, '-', "a", "-", "-", "p", "b", "detached")];
        let out2 = to_table(&rows2);
        let line2 = out2.lines().nth(1).unwrap();
        assert!(line2.contains(&format!("{YELLOW}detached{RESET}")));
    }
}
