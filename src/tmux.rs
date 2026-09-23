//! Thin wrapper around the `tmux` CLI.
//!
//! All methods are best-effort: if tmux is absent, the server isn't
//! running, or a call fails for any reason, methods return `None`/`false`/
//! empty results rather than panicking.

use std::env;
use std::process::Command;

/// One row parsed from `tmux list-sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    pub path: String,
    pub picker_status: String,
    pub picker_server: String,
    pub picker_runner: String,
    pub picker_phase: String,
    pub picker_pr: String,
    pub picker_last_active: String,
}

/// Wraps `std::process::Command` invocations of `tmux`, transparently
/// targeting an alternate socket when `TMUX_PICKER_SOCKET` is set (used by
/// integration tests to run against an isolated tmux server).
pub struct Tmux {
    socket: Option<String>,
}

impl Default for Tmux {
    fn default() -> Self {
        Self::new()
    }
}

impl Tmux {
    pub fn new() -> Self {
        let socket = env::var("TMUX_PICKER_SOCKET")
            .ok()
            .filter(|s| !s.is_empty());
        Tmux { socket }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("tmux");
        if let Some(socket) = &self.socket {
            cmd.arg("-L").arg(socket);
        }
        cmd
    }

    /// `tmux list-sessions -F '#{session_name}|#{session_path}|#{@picker_status}|#{@picker_server}|#{@picker_runner}|#{@picker_phase}|#{@picker_pr}|#{@picker_last_active}'`
    pub fn list_sessions(&self) -> Vec<SessionEntry> {
        let output = self
            .command()
            .args([
                "list-sessions",
                "-F",
                "#{session_name}|#{session_path}|#{@picker_status}|#{@picker_server}|#{@picker_runner}|#{@picker_phase}|#{@picker_pr}|#{@picker_last_active}",
            ])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };

        let text = String::from_utf8_lossy(&output.stdout);
        text.lines().filter_map(parse_session_line).collect()
    }

    /// `tmux display-message -p '#S'`
    pub fn current_session_name(&self) -> Option<String> {
        let output = self
            .command()
            .args(["display-message", "-p", "#S"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// `tmux display-message -p -t <session> '#{session_path}'`
    pub fn session_path(&self, session: &str) -> Option<String> {
        let output = self
            .command()
            .args(["display-message", "-p", "-t", session, "#{session_path}"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// `tmux kill-session -t <session>` — best-effort.
    pub fn kill_session(&self, session: &str) -> bool {
        self.command()
            .args(["kill-session", "-t", session])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// `tmux switch-client -t <session>` — best-effort.
    pub fn switch_client(&self, session: &str) -> bool {
        self.command()
            .args(["switch-client", "-t", session])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Session-scoped user option, e.g. `@root_session`.
    pub fn show_option(&self, session: &str, option: &str) -> Option<String> {
        let output = self
            .command()
            .args(["show-options", "-t", session, "-v", option])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// Global user option, e.g. `@picker_refresh_cmd`.
    pub fn show_global_option(&self, option: &str) -> Option<String> {
        let output = self
            .command()
            .args(["show-options", "-g", "-v", option])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Parses a single session line from `tmux list-sessions` output.
/// Format: name|path|status|server|runner|phase|pr|last_active
/// The pr field may contain `|`, so we peel off the last field first,
/// then parse the remaining prefix with splitn(7, '|').
fn parse_session_line(line: &str) -> Option<SessionEntry> {
    // Split off the last field (epoch timestamp)
    let (prefix, picker_last_active) = line.rsplit_once('|')?;

    // Parse the remaining 7 fields
    let mut parts = prefix.splitn(7, '|');
    let name = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let picker_status = parts.next().unwrap_or("").to_string();
    let picker_server = parts.next().unwrap_or("").to_string();
    let picker_runner = parts.next().unwrap_or("").to_string();
    let picker_phase = parts.next().unwrap_or("").to_string();
    let picker_pr = parts.next().unwrap_or("").to_string();

    Some(SessionEntry {
        name,
        path,
        picker_status,
        picker_server,
        picker_runner,
        picker_phase,
        picker_pr,
        picker_last_active: picker_last_active.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normal_eight_field_line() {
        let line = "myses|/home/user|active|unix|claude|started|ci:pass|1700000000";
        let entry = parse_session_line(line).expect("failed to parse");
        assert_eq!(entry.name, "myses");
        assert_eq!(entry.path, "/home/user");
        assert_eq!(entry.picker_status, "active");
        assert_eq!(entry.picker_server, "unix");
        assert_eq!(entry.picker_runner, "claude");
        assert_eq!(entry.picker_phase, "started");
        assert_eq!(entry.picker_pr, "ci:pass");
        assert_eq!(entry.picker_last_active, "1700000000");
    }

    #[test]
    fn parse_pr_field_with_pipe() {
        let line = "s|/p|?|srv|-|-|a|b|1700000000";
        let entry = parse_session_line(line).expect("failed to parse");
        assert_eq!(entry.name, "s");
        assert_eq!(entry.path, "/p");
        assert_eq!(entry.picker_status, "?");
        assert_eq!(entry.picker_server, "srv");
        assert_eq!(entry.picker_pr, "a|b");
        assert_eq!(entry.picker_last_active, "1700000000");
    }

    #[test]
    fn parse_empty_trailing_options() {
        let line = "s|/p||||||1700000000";
        let entry = parse_session_line(line).expect("failed to parse");
        assert_eq!(entry.name, "s");
        assert_eq!(entry.path, "/p");
        assert_eq!(entry.picker_status, "");
        assert_eq!(entry.picker_server, "");
        assert_eq!(entry.picker_runner, "");
        assert_eq!(entry.picker_phase, "");
        assert_eq!(entry.picker_pr, "");
        assert_eq!(entry.picker_last_active, "1700000000");
    }

    #[test]
    fn parse_no_pipe_returns_none() {
        let line = "just_a_name";
        let entry = parse_session_line(line);
        assert!(entry.is_none());
    }
}
