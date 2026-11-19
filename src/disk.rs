//! Per-project disk usage helpers.
//!
//! pckr shows per-project disk usage across all git worktrees in the tiled
//! view. Sizes are measured in the background by `du` and cached in a file
//! inside the project's git common dir, so a list build never waits on a disk
//! walk. Everything here is pure: parsing, formatting, and no I/O.

// Later slices wire these helpers into the tiled view.
#![allow(dead_code)]

use std::path::PathBuf;

/// A cached disk measurement for one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    /// Total size across all worktrees, in KiB.
    pub kib: u64,
    /// Unix epoch seconds when the measurement finished.
    pub measured_at: u64,
}

/// Parses `git worktree list --porcelain` output into worktree paths, in order.
pub fn parse_worktree_list(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// Serializes a cache record as `"<measured_at> <kib>\n"`.
pub fn format_cache(u: &DiskUsage) -> String {
    format!("{} {}\n", u.measured_at, u.kib)
}

/// Parses a cache record; returns `None` unless it is exactly two u64 fields
/// (`measured_at` then `kib`).
pub fn parse_cache(s: &str) -> Option<DiskUsage> {
    let mut it = s.split_whitespace();
    let measured_at = it.next()?.parse().ok()?;
    let kib = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some(DiskUsage { kib, measured_at })
}

/// Sums the first field (KiB) of every non-empty line of `du -sk` output.
/// Returns `None` when there are no lines or any first field is not a u64.
pub fn parse_du_total(stdout: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut any = false;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        total = total.checked_add(line.split_whitespace().next()?.parse::<u64>().ok()?)?;
        any = true;
    }
    any.then_some(total)
}

/// Compact human size from KiB using 1024 steps (K, M, G, T). Below 10 of a
/// unit shows one decimal, otherwise an integer; rounds half up. K is always
/// an integer.
pub fn format_size(kib: u64) -> String {
    const UNITS: [&str; 4] = ["K", "M", "G", "T"];
    let kib = kib as u128;
    let mut idx = 0;
    while idx < 3 && kib >= 1024u128.pow(idx as u32 + 1) {
        idx += 1;
    }
    // Rounding can carry into the next unit (e.g. 1023.6K), so re-check.
    let whole = |idx: usize| {
        let div = 1024u128.pow(idx as u32);
        (kib + div / 2) / div
    };
    if idx < 3 && whole(idx) >= 1024 {
        idx += 1;
    }
    let div = 1024u128.pow(idx as u32);
    let tenths = (kib * 10 + div / 2) / div;
    if idx > 0 && tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[idx])
    } else {
        format!("{}{}", whole(idx), UNITS[idx])
    }
}

/// Text of the tile line: worktree count and approximate disk size with age.
/// The `~` is deliberate: `du` counts APFS clones at full size.
pub fn disk_line(worktrees: Option<usize>, usage: Option<DiskUsage>, now: u64) -> String {
    let wt = match worktrees {
        Some(n) => format!("{n} wt"),
        None => "- wt".to_string(),
    };
    let size = match usage {
        Some(u) => format!(
            "~{} disk {} ago",
            format_size(u.kib),
            crate::model::format_age(now.saturating_sub(u.measured_at))
        ),
        None => "- disk".to_string(),
    };
    format!("{wt}  {size}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn worktree_list_parses_main_and_linked() {
        let fixture = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n\
worktree /repo-wt/a\nHEAD def456\nbranch refs/heads/a\n\n\
worktree /repo-wt/b c\nHEAD 0123\ndetached\nprunable gitdir file points to non-existent location\n\n\
worktree /repo.git\nbare\n";
        assert_eq!(
            parse_worktree_list(fixture),
            vec![
                PathBuf::from("/repo"),
                PathBuf::from("/repo-wt/a"),
                PathBuf::from("/repo-wt/b c"),
                PathBuf::from("/repo.git"),
            ]
        );
    }

    #[test]
    fn worktree_list_empty_input() {
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn cache_round_trip() {
        let u = DiskUsage {
            kib: 42,
            measured_at: 1_700_000_000,
        };
        assert_eq!(format_cache(&u), "1700000000 42\n");
        assert_eq!(parse_cache(&format_cache(&u)), Some(u));
    }

    #[test]
    fn cache_rejects_bad_input() {
        assert_eq!(parse_cache("garbage"), None);
        assert_eq!(parse_cache(""), None);
        assert_eq!(parse_cache("100"), None);
        assert_eq!(parse_cache("1 2 3"), None);
        assert_eq!(parse_cache("100 -5"), None);
        assert_eq!(parse_cache("-1 5"), None);
    }

    #[test]
    fn du_total_sums_first_fields() {
        assert_eq!(parse_du_total("120\t/a\n4\t/b c/d\n"), Some(124));
        assert_eq!(parse_du_total("120\t/a\n\n4\t/b\n"), Some(124));
    }

    #[test]
    fn du_total_none_cases() {
        assert_eq!(parse_du_total(""), None);
        assert_eq!(parse_du_total("\n\n"), None);
        assert_eq!(parse_du_total("120\t/a\nxyz\t/b\n"), None);
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0K");
        assert_eq!(format_size(512), "512K");
        assert_eq!(format_size(1023), "1023K");
        assert_eq!(format_size(1024), "1.0M");
        assert_eq!(format_size(1536), "1.5M");
        assert_eq!(format_size(10 * 1024), "10M");
        assert_eq!(format_size(136 * 1024 * 1024), "136G");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0T");
    }

    #[test]
    fn disk_line_without_usage() {
        assert_eq!(disk_line(Some(3), None, 100), "3 wt  - disk");
        assert_eq!(disk_line(None, None, 0), "- wt  - disk");
    }

    #[test]
    fn disk_line_with_usage() {
        let u = DiskUsage {
            kib: 136 * 1024 * 1024,
            measured_at: 760,
        };
        assert_eq!(
            disk_line(Some(12), Some(u), 1000),
            "12 wt  ~136G disk 4m ago"
        );
    }
}
