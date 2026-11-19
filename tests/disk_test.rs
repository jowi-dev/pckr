//! Integration tests for `pckr disk-scan` against throwaway git repos.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn pckr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pckr")
}

/// A unique scratch directory, removed on drop (also on panic).
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("pckr-disk-it-{label}-{}-{n}", std::process::id()));
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

/// Runs git with an explicit identity and no signing; panics on failure.
fn git(dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args([
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
        ])
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

fn disk_scan(arg: Option<&Path>) -> Output {
    let mut cmd = Command::new(pckr_bin());
    cmd.arg("disk-scan");
    if let Some(p) = arg {
        cmd.arg(p);
    }
    cmd.output().expect("failed to run pckr disk-scan")
}

#[test]
fn disk_scan_writes_cache_covering_all_worktrees() {
    let root = TempRoot::new("scan");
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    std::fs::write(main.join("a.txt"), "hi").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "init"]);

    let wt = root.join("wt");
    git(
        &main,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    std::fs::write(wt.join("big.bin"), vec![7u8; 200 * 1024]).unwrap();

    let lock = main.join(".git").join("pckr-disk-usage.lock");
    std::fs::write(&lock, "").unwrap();

    let out = disk_scan(Some(&main));
    assert!(out.status.success());

    let cache = std::fs::read_to_string(main.join(".git").join("pckr-disk-usage"))
        .expect("cache file missing");
    let fields: Vec<u64> = cache
        .split_whitespace()
        .map(|f| f.parse().expect("non-integer field"))
        .collect();
    assert_eq!(fields.len(), 2, "cache: {cache:?}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        fields[0] <= now + 5 && fields[0] + 300 >= now,
        "epoch {}",
        fields[0]
    );
    assert!(fields[1] >= 200, "kib {}", fields[1]);
    assert!(!lock.exists(), "lock file should be removed");
}

#[test]
fn disk_scan_on_non_git_path_exits_zero_and_writes_nothing() {
    let root = TempRoot::new("nongit");
    let out = disk_scan(Some(&root.path));
    assert!(out.status.success());
    assert!(!root.join(".git").exists());
}

#[test]
fn disk_scan_without_path_exits_2() {
    let out = disk_scan(None);
    assert_eq!(out.status.code(), Some(2));
}
