//! Session row model: turns raw tmux + git data into the 10-field row shape
//! described in docs/parity.md.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::gitinfo::{self, MergeState};
use crate::tmux::Tmux;

/// One row of the session list. Field order mirrors the table columns; the
/// `--plain` TSV in docs/parity.md uses the same order except `runner`,
/// which is appended last (field 10) so positional consumers are unaffected,
/// and the display-only `phase` (from `@picker_phase`) and `pr` (the
/// `@picker_pr` value verbatim), which `--plain` omits.
/// `name` doubles as both the machine key (field 1) and the display copy
/// (field 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub name: String,
    pub idx: usize,
    pub marker: char,
    pub display_name: String,
    pub attn: String,
    pub runner: String,
    pub phase: String,
    pub wt: String,
    pub project: String,
    pub branch: String,
    pub status: String,
    /// `@picker_pr` value verbatim; empty when unset. Rendered only, not part of the 9-field TSV.
    pub pr: String,
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

/// Returns `@picker_runner` verbatim, or `-` if empty.
pub fn runner_string(runner: &str) -> String {
    if runner.is_empty() {
        "-".to_string()
    } else {
        runner.to_string()
    }
}

/// `@picker_phase` value, or `-` when empty (no phase set).
pub fn phase_string(phase: &str) -> String {
    if phase.is_empty() {
        "-".to_string()
    } else {
        phase.to_string()
    }
}

/// Ages strictly greater than this render in the stale style.
pub const STALE_AFTER_SECS: u64 = 15 * 60;

/// Formats seconds into age string; whole minutes, truncating.
/// Under one hour: `"{m}m"`; one hour or more: `"{h}h{m}m"` (minutes 0-59, no zero-padding).
pub fn format_age(secs: u64) -> String {
    let minutes = secs / 60;
    let hours = minutes / 60;

    if hours == 0 {
        format!("{minutes}m")
    } else {
        let remainder = minutes % 60;
        format!("{hours}h{remainder}m")
    }
}

/// Returns `""` for None, else formats the age from epoch to now.
pub fn age_cell(last_active: Option<u64>, now: u64) -> String {
    match last_active {
        None => "".to_string(),
        Some(epoch) => format_age(now.saturating_sub(epoch)),
    }
}

/// True only when Some and age is strictly greater than `STALE_AFTER_SECS`.
pub fn is_stale(last_active: Option<u64>, now: u64) -> bool {
    match last_active {
        None => false,
        Some(epoch) => now.saturating_sub(epoch) > STALE_AFTER_SECS,
    }
}

/// Trims and parses as u64. Returns None on failure.
pub fn parse_epoch(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

/// Current Unix time in seconds. Returns 0 if the clock is before the epoch.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
            let runner = runner_string(&s.picker_runner);
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

            let phase = phase_string(&s.picker_phase);

            SessionRow {
                name: s.name.clone(),
                idx,
                marker,
                display_name: s.name,
                attn,
                runner,
                phase,
                wt,
                project,
                branch,
                status,
                pr: s.picker_pr,
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

/// Fields parsed from one `@picker_tile_cmd` run. `None` means the field is
/// missing (never printed, or printed with an empty value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileFields {
    pub ready: Option<String>,
    pub spend: Option<String>,
}

/// Parses one tile command's result under the `key=value` contract: each
/// stdout line shaped `key=value` sets a field, splitting at the FIRST `=`
/// with the key and value trimmed; if a key repeats, the last line wins.
/// Unknown keys, blank lines, and later lines with no `=` are ignored. An
/// empty value counts as missing. Recognized keys are `ready` and `spend`.
/// As a legacy form, if the first non-empty line has no `=`, it is used as
/// the `ready` value. A failed command returns `TileFields::default()`.
pub fn parse_tile_output(success: bool, stdout: &str) -> TileFields {
    if !success {
        return TileFields::default();
    }

    let mut fields = TileFields::default();
    let mut lines = stdout.lines().map(str::trim).filter(|l| !l.is_empty());

    let Some(first) = lines.next() else {
        return fields;
    };

    match first.split_once('=') {
        Some((key, value)) => set_tile_field(&mut fields, key, value),
        None => fields.ready = Some(first.to_string()),
    }

    for line in lines {
        if let Some((key, value)) = line.split_once('=') {
            set_tile_field(&mut fields, key, value);
        }
    }

    fields
}

/// Sets the recognized field named by `key` (trimmed) to `value` (trimmed),
/// treating an empty trimmed value as missing. Unknown keys are ignored.
fn set_tile_field(fields: &mut TileFields, key: &str, value: &str) {
    let key = key.trim();
    let value = value.trim();
    let value = if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    };
    match key {
        "ready" => fields.ready = value,
        "spend" => fields.spend = value,
        _ => {}
    }
}

/// Runs `cmd` once per project as `sh -c <cmd> sh <project> <root>` (so the
/// command line sees `$1` = project name, `$2` = project root) with the root
/// as working directory and `PICKER_PROJECT` / `PICKER_ROOT` exported, so a
/// script named directly as the command still receives both. All commands run concurrently under one shared
/// `timeout`; any still running at the deadline are killed. Returns project
/// name -> fields (see `parse_tile_output`); failed, empty, or timed-out
/// projects get no entry, and a project only gets an entry when at least one
/// field was parsed.
pub fn run_tile_cmds(
    cmd: &str,
    projects: &[(String, PathBuf)],
    timeout: Duration,
) -> HashMap<String, TileFields> {
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
            .env("PICKER_PROJECT", project)
            .env("PICKER_ROOT", root)
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
        let fields = parse_tile_output(status.success(), &stdout);
        if fields.ready.is_some() || fields.spend.is_some() {
            results.insert(project.to_string(), fields);
        }
    }
    results
}

/// Reads the global `@picker_tile_cmd` option and, if set, runs it once per
/// project (first-appearance order in `list-sessions`, keyed by
/// `gitinfo::project_name`, rooted at `gitinfo::main_repo_of`). Returns an
/// empty map without listing sessions when the option is unset. pckr has no
/// built-in knowledge of what the command invokes.
pub fn build_tile_info(tmux: &Tmux) -> HashMap<String, TileFields> {
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

/// Reads the global `@picker_usage` tmux option verbatim. Returns None if
/// unset or empty; the value is rendered on its own line directly under the
/// top help line of the TUI.
pub fn read_usage(tmux: &Tmux) -> Option<String> {
    tmux.show_global_option("@picker_usage")
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
        assert_eq!(
            parse_tile_output(true, "3\n"),
            TileFields {
                ready: Some("3".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_uses_only_first_line() {
        assert_eq!(
            parse_tile_output(true, "3\nextra"),
            TileFields {
                ready: Some("3".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_trims_whitespace() {
        assert_eq!(
            parse_tile_output(true, "  3  \n"),
            TileFields {
                ready: Some("3".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_empty_returns_none() {
        assert_eq!(parse_tile_output(true, ""), TileFields::default());
    }

    #[test]
    fn parse_tile_output_whitespace_only_returns_none() {
        assert_eq!(parse_tile_output(true, "   \n"), TileFields::default());
    }

    #[test]
    fn parse_tile_output_failure_returns_none() {
        assert_eq!(parse_tile_output(false, "3\n"), TileFields::default());
    }

    #[test]
    fn parse_tile_output_ready_and_spend_key_value_lines() {
        assert_eq!(
            parse_tile_output(true, "ready=3\nspend=$4.20/24h"),
            TileFields {
                ready: Some("3".to_string()),
                spend: Some("$4.20/24h".to_string()),
            }
        );
    }

    #[test]
    fn parse_tile_output_unknown_keys_ignored() {
        assert_eq!(
            parse_tile_output(true, "ready=3\nbogus=zzz"),
            TileFields {
                ready: Some("3".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_last_duplicate_key_wins() {
        assert_eq!(
            parse_tile_output(true, "ready=3\nready=7"),
            TileFields {
                ready: Some("7".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_value_may_contain_equals() {
        assert_eq!(
            parse_tile_output(true, "spend=a=b"),
            TileFields {
                ready: None,
                spend: Some("a=b".to_string()),
            }
        );
    }

    #[test]
    fn parse_tile_output_empty_value_is_missing() {
        assert_eq!(
            parse_tile_output(true, "ready=3\nspend="),
            TileFields {
                ready: Some("3".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_later_line_without_equals_ignored() {
        assert_eq!(
            parse_tile_output(true, "ready=3\njunk"),
            TileFields {
                ready: Some("3".to_string()),
                spend: None,
            }
        );
    }

    #[test]
    fn parse_tile_output_trims_key_and_value() {
        assert_eq!(
            parse_tile_output(true, " spend = 12k tok "),
            TileFields {
                ready: None,
                spend: Some("12k tok".to_string()),
            }
        );
    }

    #[test]
    fn run_tile_cmds_echo_returns_value() {
        let root = std::env::temp_dir();
        let projects = vec![("proj1".to_string(), root.clone())];
        let results = run_tile_cmds("echo 3", &projects, Duration::from_secs(5));
        assert_eq!(
            results.get("proj1"),
            Some(&TileFields {
                ready: Some("3".to_string()),
                spend: None,
            })
        );
    }

    #[test]
    fn run_tile_cmds_dollar_one_returns_project_name() {
        let root = std::env::temp_dir();
        let projects = vec![("myproject".to_string(), root.clone())];
        let results = run_tile_cmds("echo $1", &projects, Duration::from_secs(5));
        assert_eq!(
            results.get("myproject"),
            Some(&TileFields {
                ready: Some("myproject".to_string()),
                spend: None,
            })
        );
    }

    #[test]
    fn run_tile_cmds_dollar_two_returns_root() {
        let root = std::env::temp_dir();
        let root_str = root.to_string_lossy().to_string();
        let projects = vec![("proj".to_string(), root.clone())];
        let results = run_tile_cmds("printf '%s' \"$2\"", &projects, Duration::from_secs(5));
        assert_eq!(
            results.get("proj"),
            Some(&TileFields {
                ready: Some(root_str),
                spend: None,
            })
        );
    }

    #[test]
    fn run_tile_cmds_exports_project_and_root_env() {
        let root = std::env::temp_dir();
        let projects = vec![("proj".to_string(), root.clone())];
        let results = run_tile_cmds(
            "printf '%s|%s' \"$PICKER_PROJECT\" \"$PICKER_ROOT\"",
            &projects,
            Duration::from_secs(5),
        );
        let expected = format!("proj|{}", root.to_string_lossy());
        assert_eq!(
            results.get("proj"),
            Some(&TileFields {
                ready: Some(expected),
                spend: None,
            })
        );
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

        assert_eq!(
            results.get("proj1"),
            Some(&TileFields {
                ready: Some("proj1".to_string()),
                spend: None,
            })
        );
        assert_eq!(
            results.get("proj2"),
            Some(&TileFields {
                ready: Some("proj2".to_string()),
                spend: None,
            })
        );
        assert_eq!(
            results.get("proj3"),
            Some(&TileFields {
                ready: Some("proj3".to_string()),
                spend: None,
            })
        );
    }

    #[test]
    fn runner_placeholder_when_empty() {
        assert_eq!(runner_string(""), "-");
    }

    #[test]
    fn runner_passes_through_verbatim_claude() {
        assert_eq!(runner_string("claude"), "claude");
    }

    #[test]
    fn runner_passes_through_verbatim_token() {
        assert_eq!(runner_string("my-agent"), "my-agent");
    }

    #[test]
    fn phase_returns_dash_when_empty() {
        assert_eq!(phase_string(""), "-");
    }

    #[test]
    fn phase_returns_value_verbatim() {
        assert_eq!(phase_string("started"), "started");
        assert_eq!(phase_string("working"), "working");
        assert_eq!(phase_string("review"), "review");
    }

    #[test]
    fn format_age_minutes_under_hour() {
        assert_eq!(format_age(0), "0m");
        assert_eq!(format_age(59), "0m");
        assert_eq!(format_age(180), "3m");
        assert_eq!(format_age(3599), "59m");
    }

    #[test]
    fn format_age_hours_and_minutes() {
        assert_eq!(format_age(3600), "1h0m");
        assert_eq!(format_age(4320), "1h12m");
        assert_eq!(format_age(90000), "25h0m");
    }

    #[test]
    fn age_cell_none_renders_empty() {
        assert_eq!(age_cell(None, 1_000), "");
    }

    #[test]
    fn age_cell_computes_from_epoch() {
        assert_eq!(age_cell(Some(1_000 - 180), 1_000), "3m");
    }

    #[test]
    fn age_cell_clamps_future_epoch() {
        assert_eq!(age_cell(Some(2_000), 1_000), "0m");
    }

    #[test]
    fn is_stale_none_is_not_stale() {
        assert!(!is_stale(None, 10_000));
    }

    #[test]
    fn is_stale_at_threshold_is_not_stale() {
        assert!(!is_stale(Some(10_000 - STALE_AFTER_SECS), 10_000));
    }

    #[test]
    fn is_stale_past_threshold_is_stale() {
        assert!(is_stale(Some(10_000 - STALE_AFTER_SECS - 1), 10_000));
    }

    #[test]
    fn is_stale_future_epoch_is_not_stale() {
        assert!(!is_stale(Some(20_000), 10_000));
    }

    #[test]
    fn parse_epoch_empty_is_none() {
        assert_eq!(parse_epoch(""), None);
    }

    #[test]
    fn parse_epoch_invalid_is_none() {
        assert_eq!(parse_epoch("abc"), None);
        assert_eq!(parse_epoch("-5"), None);
    }

    #[test]
    fn parse_epoch_parses_valid_with_whitespace() {
        assert_eq!(parse_epoch(" 1700000000 "), Some(1_700_000_000));
    }
}
