//! Session row model: turns raw tmux + git data into the 9-field row shape
//! described in docs/parity.md.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::gitinfo::{self, MergeState};
use crate::tmux::Tmux;

/// One row of the session list. Field order mirrors the 9-field TSV from
/// docs/parity.md; `name` doubles as both the machine key (field 1) and the
/// display copy (field 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub name: String,
    pub idx: usize,
    pub marker: char,
    pub display_name: String,
    pub attn: String,
    pub wt: String,
    pub project: String,
    pub branch: String,
    pub status: String,
}

/// `@picker_status` and `@picker_server` concatenated with no separator;
/// `-` if both are empty.
pub fn attn_string(status: &str, server: &str) -> String {
    if status.is_empty() && server.is_empty() {
        "-".to_string()
    } else {
        format!("{status}{server}")
    }
}

/// Builds the full row list from the current tmux session list, resolving
/// git branch/project info for each session's path.
pub fn build_rows(tmux: &Tmux) -> Vec<SessionRow> {
    let current = tmux.current_session_name();
    let sessions = tmux.list_sessions();

    sessions
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let idx = i + 1;
            let marker = if current.as_deref() == Some(s.name.as_str()) {
                '*'
            } else {
                '-'
            };
            let attn = attn_string(&s.picker_status, &s.picker_server);
            let path = Path::new(&s.path);
            let wt = if gitinfo::is_worktree(path) {
                "wt"
            } else {
                "-"
            }
            .to_string();
            let project = gitinfo::project_name(path).unwrap_or_else(|| "-".to_string());

            let (branch, status) = match gitinfo::branch_status(path) {
                None => ("-".to_string(), "-".to_string()),
                Some(bs) => {
                    let branch = if bs.state == MergeState::Detached {
                        "-".to_string()
                    } else {
                        bs.branch
                    };
                    let status = match bs.state {
                        MergeState::Merged => "merged",
                        MergeState::Unmerged => "unmerged",
                        MergeState::Detached => "detached",
                    }
                    .to_string();
                    (branch, status)
                }
            };

            SessionRow {
                name: s.name.clone(),
                idx,
                marker,
                display_name: s.name,
                attn,
                wt,
                project,
                branch,
                status,
            }
        })
        .collect()
}

/// Reads the global `@picker_refresh_cmd` tmux option and, if non-empty,
/// runs it via `sh -c`, swallowing all output and errors. This is the
/// generic plugin-refresh hook; pckr has no built-in knowledge of what it
/// invokes.
pub fn run_refresh_hook(tmux: &Tmux) {
    if let Some(cmd) = tmux.show_global_option("@picker_refresh_cmd") {
        if !cmd.is_empty() {
            let _ = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attn_concatenates_with_no_separator() {
        assert_eq!(attn_string("\u{2753}", "srv"), "\u{2753}srv");
    }

    #[test]
    fn attn_placeholder_when_both_empty() {
        assert_eq!(attn_string("", ""), "-");
    }

    #[test]
    fn attn_one_sided_values_pass_through() {
        assert_eq!(attn_string("\u{23f8}", ""), "\u{23f8}");
        assert_eq!(attn_string("", "srv"), "srv");
    }
}
