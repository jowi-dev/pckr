//! Integration tests for `pckr branch-status` / `pckr project-name` against
//! real throwaway git repos, covering the scenarios from docs/parity.md's
//! "branch-status logic" and "project-name logic" sections.
//!
//! Each test builds its own repo(s) under a unique subdirectory of
//! `std::env::temp_dir()`, cleaned up by a Drop guard even on panic. A bare
//! repo acts as "origin"; every commit-producing git command passes
//! explicit `-c user.email=test@test -c user.name=test -c
//! commit.gpgsign=false` so the suite never depends on global git config or
//! GPG signing.
//!
//! tmux-dependent scenarios are intentionally out of scope here (slice 3).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn pckr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pckr")
}

fn branch_status_output(path: &Path) -> String {
    let output = Command::new(pckr_bin())
        .arg("branch-status")
        .arg(path)
        .output()
        .expect("failed to run pckr branch-status");
    assert!(output.status.success(), "branch-status exited non-zero");
    String::from_utf8(output.stdout).expect("non-utf8 stdout")
}

fn project_name_output(path: &Path) -> String {
    let output = Command::new(pckr_bin())
        .arg("project-name")
        .arg(path)
        .output()
        .expect("failed to run pckr project-name");
    assert!(output.status.success(), "project-name exited non-zero");
    String::from_utf8(output.stdout)
        .expect("non-utf8 stdout")
        .trim_end()
        .to_string()
}

/// A unique scratch directory under `std::env::temp_dir()`, recursively
/// removed on drop (including on panic, since Drop still runs during
/// unwinding).
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "pckr-gitinfo-it-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("failed to create temp root");
        TempRoot { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Runs a git command, panicking with stderr on failure. Used for test
/// fixture setup only.
fn git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Commit-producing git commands always get an explicit identity + no GPG
/// signing, independent of ambient git config.
fn git_commit(dir: &Path, message: &str, extra: &[&str]) {
    let mut args: Vec<&str> = vec![
        "-c",
        "user.email=test@test",
        "-c",
        "user.name=test",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        message,
    ];
    args.extend_from_slice(extra);
    git(dir, &args);
}

/// Sets up a bare "origin" repo and a "work" clone with a single commit on
/// `main`, pushed and tracked (including a local `origin/HEAD` symref so
/// `resolve_base`'s first lookup has something to find).
fn setup_origin_and_work(root: &TempRoot) -> PathBuf {
    let bare = root.join("origin.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);

    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("base.txt"), "v1\n").unwrap();
    git(&work, &["add", "base.txt"]);
    git_commit(&work, "initial", &["--no-verify"]);

    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    git(&work, &["remote", "set-head", "origin", "main"]);

    work
}

// -- branch-status scenarios ------------------------------------------------

#[test]
fn branch_status_fast_forward_merged() {
    let root = TempRoot::new("ff");
    let work = setup_origin_and_work(&root);

    git(&work, &["checkout", "-q", "-b", "feature-ff"]);

    assert_eq!(branch_status_output(&work), "feature-ff [merged]\n");
}

#[test]
fn branch_status_squash_merged() {
    let root = TempRoot::new("squash");
    let work = setup_origin_and_work(&root);

    git(&work, &["checkout", "-q", "-b", "feature-squash"]);
    std::fs::write(work.join("feature.txt"), "feature content\n").unwrap();
    git(&work, &["add", "feature.txt"]);
    git_commit(&work, "add feature", &["--no-verify"]);

    git(&work, &["checkout", "-q", "main"]);
    git(&work, &["merge", "--squash", "-q", "feature-squash"]);
    git_commit(&work, "squash merge feature-squash", &["--no-verify"]);
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["fetch", "-q", "origin"]);

    git(&work, &["checkout", "-q", "feature-squash"]);

    assert_eq!(branch_status_output(&work), "feature-squash [merged]\n");
}

#[test]
fn branch_status_unmerged() {
    let root = TempRoot::new("unmerged");
    let work = setup_origin_and_work(&root);

    git(&work, &["checkout", "-q", "-b", "feature-unmerged"]);
    std::fs::write(work.join("unmerged.txt"), "wip\n").unwrap();
    git(&work, &["add", "unmerged.txt"]);
    git_commit(&work, "wip", &["--no-verify"]);

    assert_eq!(branch_status_output(&work), "feature-unmerged [unmerged]\n");
}

#[test]
fn branch_status_detached_head() {
    let root = TempRoot::new("detached");
    let work = setup_origin_and_work(&root);

    let sha = git_stdout(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", &sha]);

    assert_eq!(branch_status_output(&work), "[detached]\n");
}

#[test]
fn branch_status_on_default_branch_is_empty() {
    // No remote at all: resolve_base falls through to local `main`, which
    // equals the current branch, so output must be empty.
    let root = TempRoot::new("default-branch");
    let repo = root.join("solo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("f.txt"), "x\n").unwrap();
    git(&repo, &["add", "f.txt"]);
    git_commit(&repo, "initial", &["--no-verify"]);

    assert_eq!(branch_status_output(&repo), "");
}

#[test]
fn branch_status_non_git_path_is_empty() {
    let root = TempRoot::new("non-git");
    let dir = root.join("plain");
    std::fs::create_dir_all(&dir).unwrap();

    assert_eq!(branch_status_output(&dir), "");
}

// -- project-name scenarios --------------------------------------------------

#[test]
fn project_name_regular_checkout_is_repo_name() {
    let root = TempRoot::new("project-regular");
    let work = setup_origin_and_work(&root);

    assert_eq!(project_name_output(&work), "work");
}

#[test]
fn project_name_linked_worktree_reports_main_repo_name() {
    let root = TempRoot::new("project-worktree");
    let work = setup_origin_and_work(&root);

    let wt = root.join("wt");
    git(
        &work,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "wt-branch",
        ],
    );

    // The linked worktree must report the *parent* repo's name ("work"),
    // not its own directory name ("wt").
    assert_eq!(project_name_output(&wt), "work");
    assert_eq!(project_name_output(&work), "work");
}

#[test]
fn project_name_non_git_path_is_dash() {
    let root = TempRoot::new("project-non-git");
    let dir = root.join("plain");
    std::fs::create_dir_all(&dir).unwrap();

    assert_eq!(project_name_output(&dir), "-");
}
