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

    /// `tmux list-sessions -F '#{session_name}|#{session_path}|#{@picker_status}|#{@picker_server}|#{@picker_runner}|#{@picker_phase}|#{@picker_pr}'`
    pub fn list_sessions(&self) -> Vec<SessionEntry> {
        let output = self
            .command()
            .args([
                "list-sessions",
                "-F",
                "#{session_name}|#{session_path}|#{@picker_status}|#{@picker_server}|#{@picker_runner}|#{@picker_phase}|#{@picker_pr}",
            ])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };

        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .filter_map(|line| {
                let mut parts = line.splitn(7, '|');
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
                })
            })
            .collect()
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
