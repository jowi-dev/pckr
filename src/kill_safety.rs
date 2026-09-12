//! Tiered kill-confirmation classification via `tm runs kill-safety`, per
//! tskmstr ADR-0005 and GitHub issue pckr#2.
//!
//! `tm` prints exactly two lines: a machine-readable tier token on line 1,
//! and a human-readable reason on line 2. Any failure to classify (spawn
//! failure, non-zero exit, unrecognized token) collapses to `Unknown` so the
//! TUI always falls back to the safest (confirm) path.

use std::process::Command;

/// Kill-safety tier reported by `tm runs kill-safety <session>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillTier {
    LiveRun,
    RootSession,
    Safe,
    Unknown,
}

/// A classification result: the tier plus a human-readable reason suitable
/// for display in a confirmation prompt. Never branch on `reason`; it's
/// display-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub tier: KillTier,
    pub reason: String,
}

/// Parses the raw `tm runs kill-safety` output into a `Classification`.
/// Pure and terminal-independent so it's unit-testable without shelling out.
pub fn parse(success: bool, stdout: &str) -> Classification {
    if !success {
        return Classification {
            tier: KillTier::Unknown,
            reason: String::new(),
        };
    }

    let mut lines = stdout.lines();
    let tier = match lines.next().map(str::trim) {
        Some("live-run") => KillTier::LiveRun,
        Some("root-session") => KillTier::RootSession,
        Some("safe") => KillTier::Safe,
        Some("unknown") => KillTier::Unknown,
        _ => KillTier::Unknown,
    };
    let reason = lines.next().map(str::trim).unwrap_or("").to_string();

    Classification { tier, reason }
}

/// Runs `tm runs kill-safety <session_name>` and classifies the result.
/// Best-effort: a spawn failure (e.g. `tm` not installed) is treated as
/// `Unknown`, matching the contract's "any non-zero exit, spawn failure ...
/// must be treated as unknown".
pub fn classify(session_name: &str) -> Classification {
    let output = Command::new("tm")
        .args(["runs", "kill-safety", session_name])
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            parse(o.status.success(), &stdout)
        }
        Err(_) => Classification {
            tier: KillTier::Unknown,
            reason: "tm unavailable".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_run() {
        let c = parse(true, "live-run\nsession is running a live task\n");
        assert_eq!(c.tier, KillTier::LiveRun);
        assert_eq!(c.reason, "session is running a live task");
    }

    #[test]
    fn parses_root_session() {
        let c = parse(true, "root-session\nthis is the root session\n");
        assert_eq!(c.tier, KillTier::RootSession);
        assert_eq!(c.reason, "this is the root session");
    }

    #[test]
    fn parses_safe() {
        let c = parse(true, "safe\nnothing to worry about\n");
        assert_eq!(c.tier, KillTier::Safe);
        assert_eq!(c.reason, "nothing to worry about");
    }

    #[test]
    fn parses_unknown_token() {
        let c = parse(true, "unknown\ncould not classify\n");
        assert_eq!(c.tier, KillTier::Unknown);
        assert_eq!(c.reason, "could not classify");
    }

    #[test]
    fn unrecognized_first_line_becomes_unknown() {
        let c = parse(true, "bogus-token\nsome reason\n");
        assert_eq!(c.tier, KillTier::Unknown);
        assert_eq!(c.reason, "some reason");
    }

    #[test]
    fn empty_output_becomes_unknown_with_empty_reason() {
        let c = parse(true, "");
        assert_eq!(c.tier, KillTier::Unknown);
        assert_eq!(c.reason, "");
    }

    #[test]
    fn non_success_becomes_unknown_regardless_of_stdout() {
        let c = parse(false, "safe\nirrelevant\n");
        assert_eq!(c.tier, KillTier::Unknown);
        assert_eq!(c.reason, "");
    }

    #[test]
    fn missing_reason_line_yields_empty_reason() {
        let c = parse(true, "safe\n");
        assert_eq!(c.tier, KillTier::Safe);
        assert_eq!(c.reason, "");

        let c2 = parse(true, "safe");
        assert_eq!(c2.tier, KillTier::Safe);
        assert_eq!(c2.reason, "");
    }

    #[test]
    fn reason_is_trimmed() {
        let c = parse(true, "live-run\n   spaced reason   \n");
        assert_eq!(c.reason, "spaced reason");
    }
}
