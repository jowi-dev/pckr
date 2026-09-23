//! Session row model: turns raw tmux + git data into the 9-field row shape
//! described in docs/parity.md.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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

/// Shared wall-clock budget for one batch of `@picker_tile_cmd` runs, so a
/// slow tracker call can never freeze the popup.
pub const TILE_CMD_TIMEOUT: Duration = Duration::from_secs(1);

/// Parses one tile command's result: the first stdout line, trimmed, only
/// when the command succeeded and that line is non-empty.
pub fn parse_tile_output(success: bool, stdout: &str) -> Option<String> {
    if !success {
        return None;
    }
    stdout
        .lines()
        .next()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
}

/// Runs `cmd` once per project as `sh -c <cmd> sh <project> <root>` (so the
/// script sees `$1` = project name, `$2` = project root) with the root as
/// working directory. All commands run concurrently under one shared
/// `timeout`; any still running at the deadline are killed. Returns project
/// name -> value (see `parse_tile_output`); failed, empty, or timed-out
/// projects get no entry.
pub fn run_tile_cmds(
    cmd: &str,
    projects: &[(String, PathBuf)],
    timeout: Duration,
) -> HashMap<String, String> {
    let deadline = Instant::now() + timeout;

    // Each child's stdout is drained on its own thread so a pipe held open
    // (e.g. by a backgrounded grandchild) never blocks this thread.
    let mut running: Vec<(&str, Child, mpsc::Receiver<String>)> = Vec::new();
    for (project, root) in projects {
        let spawned = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .arg("sh")
            .arg(project)
            .arg(root)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else {
            continue;
        };
        let (tx, rx) = mpsc::channel();
        if let Some(mut stdout) = child.stdout.take() {
            thread::spawn(move || {
                let mut buf = String::new();
                let _ = stdout.read_to_string(&mut buf);
                let _ = tx.send(buf);
            });
        }
        running.push((project.as_str(), child, rx));
    }

    while Instant::now() < deadline
        && running
            .iter_mut()
            .any(|(_, child, _)| matches!(child.try_wait(), Ok(None)))
    {
        thread::sleep(Duration::from_millis(10));
    }

    let mut results = HashMap::new();
    for (project, mut child, rx) in running {
        let Ok(Some(status)) = child.try_wait() else {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(stdout) = rx.recv_timeout(remaining) else {
            continue;
        };
        if let Some(value) = parse_tile_output(status.success(), &stdout) {
            results.insert(project.to_string(), value);
        }
    }
    results
}

/// Reads the global `@picker_tile_cmd` option and, if set, runs it once per
/// project (first-appearance order in `list-sessions`, keyed by
/// `gitinfo::project_name`, rooted at `gitinfo::main_repo_of`). Returns an
/// empty map without listing sessions when the option is unset. pckr has no
/// built-in knowledge of what the command invokes.
pub fn build_tile_info(tmux: &Tmux) -> HashMap<String, String> {
    let Some(cmd) = tmux.show_global_option("@picker_tile_cmd") else {
        return HashMap::new();
    };

    let mut projects: Vec<(String, PathBuf)> = Vec::new();
    for session in tmux.list_sessions() {
        let path = Path::new(&session.path);
        let (Some(project), Some(root)) =
            (gitinfo::project_name(path), gitinfo::main_repo_of(path))
        else {
            continue;
        };
        if !projects.iter().any(|(p, _)| *p == project) {
            projects.push((project, root));
        }
    }

    run_tile_cmds(&cmd, &projects, TILE_CMD_TIMEOUT)
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

    #[test]
    fn parse_tile_output_success_with_content() {
        assert_eq!(parse_tile_output(true, "3\n"), Some("3".to_string()));
    }

    #[test]
    fn parse_tile_output_uses_only_first_line() {
        assert_eq!(parse_tile_output(true, "3\nextra"), Some("3".to_string()));
    }

    #[test]
    fn parse_tile_output_trims_whitespace() {
        assert_eq!(parse_tile_output(true, "  3  \n"), Some("3".to_string()));
    }

    #[test]
    fn parse_tile_output_empty_returns_none() {
        assert_eq!(parse_tile_output(true, ""), None);
    }

    #[test]
    fn parse_tile_output_whitespace_only_returns_none() {
        assert_eq!(parse_tile_output(true, "   \n"), None);
    }

    #[test]
    fn parse_tile_output_failure_returns_none() {
        assert_eq!(parse_tile_output(false, "3\n"), None);
    }

    #[test]
    fn run_tile_cmds_echo_returns_value() {
        let root = std::env::temp_dir();
        let projects = vec![("proj1".to_string(), root.clone())];
        let results = run_tile_cmds("echo 3", &projects, Duration::from_secs(5));
        assert_eq!(results.get("proj1"), Some(&"3".to_string()));
    }

    #[test]
    fn run_tile_cmds_dollar_one_returns_project_name() {
        let root = std::env::temp_dir();
        let projects = vec![("myproject".to_string(), root.clone())];
        let results = run_tile_cmds("echo $1", &projects, Duration::from_secs(5));
        assert_eq!(results.get("myproject"), Some(&"myproject".to_string()));
    }

    #[test]
    fn run_tile_cmds_dollar_two_returns_root() {
        let root = std::env::temp_dir();
        let root_str = root.to_string_lossy().to_string();
        let projects = vec![("proj".to_string(), root.clone())];
        let results = run_tile_cmds("printf '%s' \"$2\"", &projects, Duration::from_secs(5));
        assert_eq!(results.get("proj"), Some(&root_str));
    }

    #[test]
    fn run_tile_cmds_nonzero_exit_gives_no_entry() {
        let root = std::env::temp_dir();
        let projects = vec![("proj".to_string(), root)];
        let results = run_tile_cmds("exit 1", &projects, Duration::from_secs(5));
        assert!(!results.contains_key("proj"));
    }

    #[test]
    fn run_tile_cmds_timeout_kills_child_and_returns_no_entry() {
        let root = std::env::temp_dir();
        let projects = vec![("proj".to_string(), root)];
        let start = Instant::now();
        let results = run_tile_cmds("sleep 5", &projects, Duration::from_millis(200));
        let elapsed = start.elapsed();

        // Should complete quickly (well under 2 seconds)
        assert!(elapsed < Duration::from_secs(2));
        // Should not have an entry for the sleeping process
        assert!(!results.contains_key("proj"));
    }

    #[test]
    fn run_tile_cmds_timeout_applies_after_stdout_closes() {
        let root = std::env::temp_dir();
        let projects = vec![("proj".to_string(), root)];
        let start = Instant::now();
        let results = run_tile_cmds(
            "echo 3; exec >&-; sleep 5",
            &projects,
            Duration::from_millis(200),
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!results.contains_key("proj"));
    }

    #[test]
    fn run_tile_cmds_multiple_projects_concurrent() {
        let root = std::env::temp_dir();
        let projects = vec![
            ("proj1".to_string(), root.clone()),
            ("proj2".to_string(), root.clone()),
            ("proj3".to_string(), root),
        ];
        let results = run_tile_cmds("echo $1", &projects, Duration::from_secs(5));

        assert_eq!(results.get("proj1"), Some(&"proj1".to_string()));
        assert_eq!(results.get("proj2"), Some(&"proj2".to_string()));
        assert_eq!(results.get("proj3"), Some(&"proj3".to_string()));
    }
}
