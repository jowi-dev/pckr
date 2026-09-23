//! Opens a session's pull request via `gh pr view --web`. `gh` is an optional
//! runtime dependency; every failure is reported via `Err`, never fatal.
//! `gh` runs in the session's path so it resolves the PR from the checked-out
//! branch without pckr knowing the PR number.

use std::process::{Command, Stdio};

/// Extracts the first non-empty trimmed line of `stderr` and prefixes it with
/// `pr: `; if `stderr` has no non-empty line, returns `pr: gh pr view failed`.
/// Pure; suitable for unit testing.
pub fn failure_message(stderr: &str) -> String {
    for line in stderr.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return format!("pr: {}", trimmed);
        }
    }
    "pr: gh pr view failed".to_string()
}

/// Opens the pull request for the session at `path` by running
/// `gh pr view --web` with that path as the current directory.
/// Output is captured (the TUI owns the terminal).
/// Returns `Ok(())` on success, or `Err(msg)` with a human-readable error.
pub fn open(path: &str) -> Result<(), String> {
    let output = match Command::new("gh")
        .args(["pr", "view", "--web"])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return Err("pr: gh unavailable".to_string()),
    };

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(failure_message(&stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_message_extracts_first_nonempty_line_and_prefixes() {
        let stderr = "some error\nsecond line";
        assert_eq!(failure_message(stderr), "pr: some error");
    }

    #[test]
    fn failure_message_skips_blank_leading_lines() {
        let stderr = "\n  \nactual message\nfollowing";
        assert_eq!(failure_message(stderr), "pr: actual message");
    }

    #[test]
    fn failure_message_fallback_when_no_nonempty_line() {
        let stderr = "\n\n  \n";
        assert_eq!(failure_message(stderr), "pr: gh pr view failed");
    }

    #[test]
    fn failure_message_empty_stderr() {
        assert_eq!(failure_message(""), "pr: gh pr view failed");
    }
}
