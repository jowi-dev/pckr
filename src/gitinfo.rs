//! Git branch/merge status and project-name resolution, ported verbatim
//! (per docs/parity.md) from the bash picker's git logic. All git calls are
//! local-only (read-only, never fetch).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    Merged,
    Unmerged,
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchStatus {
    /// Empty when `state` is `Detached`.
    pub branch: String,
    pub state: MergeState,
}

fn run_git(path: &Path, args: &[&str]) -> Option<Output> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()
}

/// Runs a git command and returns trimmed stdout, only on success.
fn run_git_ok(path: &Path, args: &[&str]) -> Option<String> {
    let output = run_git(path, args)?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Runs a git command and reports only success/failure of the exit status
/// (used for boolean queries like `--is-ancestor` / `--verify --quiet`).
fn run_git_status(path: &Path, args: &[&str]) -> bool {
    run_git(path, args)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `<path>/.git` is a regular file: true for linked worktrees, false for
/// regular checkouts (where `.git` is a directory) and non-git paths.
pub fn is_worktree(path: &Path) -> bool {
    path.join(".git").is_file()
}

/// `git -C <path> rev-parse --path-format=absolute --git-common-dir`,
/// stripped of a trailing `/.git`. Yields the main checkout path for linked
/// worktrees and the repo root for regular checkouts.
pub fn main_repo_of(path: &Path) -> Option<PathBuf> {
    let common_dir = run_git_ok(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if common_dir.is_empty() {
        return None;
    }
    let common_dir = PathBuf::from(common_dir);
    if common_dir.ends_with(".git") {
        common_dir.parent().map(|p| p.to_path_buf())
    } else {
        Some(common_dir)
    }
}

/// Basename of `main_repo_of(path)`; `None` for non-git/missing paths.
pub fn project_name(path: &Path) -> Option<String> {
    let main_repo = main_repo_of(path)?;
    main_repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
}

/// Root-session-name derivation: main checkout dirname basename, with dots
/// mapped to dashes (matching tm's session naming).
pub fn root_session_name(path: &Path) -> Option<String> {
    let main_repo = main_repo_of(path)?;
    let basename = main_repo.file_name()?.to_string_lossy().to_string();
    Some(basename.replace('.', "-"))
}

/// `resolve_base` order: `origin/HEAD` short name if non-empty ->
/// `origin/main` -> `origin/master` -> local `main` -> local `master` ->
/// give up (`None`).
fn resolve_base(path: &Path) -> Option<String> {
    if let Some(s) = run_git_ok(
        path,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if run_git_status(path, &["rev-parse", "--verify", "--quiet", candidate]) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn is_ancestor(path: &Path, rev: &str, base: &str) -> bool {
    run_git_status(path, &["merge-base", "--is-ancestor", rev, base])
}

/// git-delete-squashed trick: build a synthetic commit with the same tree as
/// HEAD but parented on the merge-base, then check whether `git cherry`
/// considers it already upstream (a `-`-prefixed line). Any git failure
/// along the way means "not detectably squash-merged" (caller falls back to
/// `Unmerged`).
fn is_squash_merged(path: &Path, base: &str) -> bool {
    let mb = match run_git_ok(path, &["merge-base", base, "HEAD"]) {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let tree = match run_git_ok(path, &["rev-parse", "HEAD^{tree}"]) {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let synthetic = match run_git_ok(path, &["commit-tree", &tree, "-p", &mb, "-m", "_"]) {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let cherry = match run_git_ok(path, &["cherry", base, &synthetic]) {
        Some(s) => s,
        None => return false,
    };
    cherry.lines().next().unwrap_or("").starts_with('-')
}

/// Branch + merge status for the worktree at `path`.
///
/// - Not a dir / not a git worktree -> `None`.
/// - Detached HEAD (`git branch --show-current` empty) -> `Detached` with
///   an empty branch name.
/// - No resolvable base, or base == current branch -> `None` (matches the
///   bash script's "empty output" behavior).
/// - Otherwise `Merged` (fast-forward ancestor or squash-merged) or
///   `Unmerged`.
pub fn branch_status(path: &Path) -> Option<BranchStatus> {
    if !path.is_dir() {
        return None;
    }
    let inside = run_git_ok(path, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }

    // NOTE: intentionally `git branch --show-current`, not
    // `git rev-parse --abbrev-ref HEAD` — a tag named `main` makes ref
    // shortening return `heads/main` instead of the branch name.
    let branch = run_git_ok(path, &["branch", "--show-current"]).unwrap_or_default();
    if branch.is_empty() {
        return Some(BranchStatus {
            branch: String::new(),
            state: MergeState::Detached,
        });
    }

    let base = resolve_base(path)?;
    if base == branch {
        return None;
    }

    let state = if is_ancestor(path, "HEAD", &base) || is_squash_merged(path, &base) {
        MergeState::Merged
    } else {
        MergeState::Unmerged
    };

    Some(BranchStatus { branch, state })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_worktree_true_for_regular_file_git() {
        let dir = std::env::temp_dir().join(format!("pckr-gitinfo-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        assert!(is_worktree(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn is_worktree_false_for_directory_git() {
        let dir =
            std::env::temp_dir().join(format!("pckr-gitinfo-unit-dir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(!is_worktree(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn root_session_name_maps_dots_to_dashes() {
        let dirname = format!("pckr-gitinfo-unit-dots.v1.2-{}", std::process::id());
        let dir = std::env::temp_dir().join(&dirname);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());

        let name = root_session_name(&dir).unwrap();
        assert_eq!(name, dirname.replace('.', "-"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
