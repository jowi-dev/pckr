//! End-to-end TUI integration tests: drives the real `pckr` binary running
//! inside a pane of an isolated, throwaway tmux server (`tmux -L <socket>`,
//! `-f /dev/null` so no user `~/.tmux.conf` leaks in), asserting on
//! `capture-pane -p` output and on `tmux list-sessions` / git worktree
//! state.
//!
//! SAFETY: every session created here gets `-c <fresh temp dir>` as its
//! start directory, NEVER the repo/checkout that this test binary itself
//! lives in. pckr's kill flow removes worktrees at the session path, and a
//! test session rooted in a real checkout could delete it — this exact
//! accident happened once in the predecessor project. `fresh_dir` always
//! allocates a brand-new directory under `std::env::temp_dir()`.
//!
//! Each test uses its own uniquely-named tmux socket (so its own private
//! server), but a process-wide mutex still serializes test bodies: spawning
//! several throwaway tmux servers at once is more prone to flakiness (CI
//! resource contention, tmux server startup races) than running them one at
//! a time, and these tests are not performance-sensitive.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

fn pckr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pckr")
}

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A brand-new, empty directory under `std::env::temp_dir()`. Never the
/// current directory, never any ancestor of this repo — see module-level
/// SAFETY note.
fn fresh_dir(label: &str) -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "pckr-tui-it-dir-{label}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("failed to create fresh temp dir");
    path
}

/// Runs a git command against `dir`, panicking with stderr on failure. Test
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

fn git_commit(dir: &Path, message: &str) {
    git(
        dir,
        &[
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// An isolated throwaway tmux server on its own `-L` socket. Every method
/// targets this socket only; `Drop` kills the server (best-effort) so a
/// failing assertion never leaks a server process.
struct TestServer {
    socket: String,
}

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TestServer {
    fn new(label: &str) -> Self {
        let n = SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
        let socket = format!("pckr-tui-it-{label}-{}-{n}", std::process::id());
        TestServer { socket }
    }

    fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.args(["-L", &self.socket, "-f", "/dev/null"]);
        cmd
    }

    fn tmux_ok(&self, args: &[&str]) -> Output {
        let out = self
            .tmux()
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn tmux {args:?}: {e}"));
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// Starts (or, after the first call, adds to) a session on this server,
    /// rooted at `dir` (which MUST be a fresh temp directory — see the
    /// module-level SAFETY note), running `argv` as the pane's command
    /// instead of the default shell. Passed as separate argv elements (not
    /// a single shell string) so tmux execs it directly with no shell
    /// quoting to worry about.
    fn new_session(&self, name: &str, dir: &Path, argv: &[&str]) {
        let mut args = vec![
            "new-session",
            "-d",
            "-x",
            "180",
            "-y",
            "40",
            "-s",
            name,
            "-c",
            dir.to_str().expect("non-utf8 temp dir path"),
        ];
        args.extend_from_slice(argv);
        self.tmux_ok(&args);
    }

    fn capture_pane(&self, target: &str) -> String {
        let out = self
            .tmux()
            .args(["capture-pane", "-p", "-t", target])
            .output()
            .expect("capture-pane failed to spawn");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Sends `text` as literal characters (via `send-keys -l`), i.e. typed
    /// input rather than a key-name lookup.
    fn send_literal(&self, target: &str, text: &str) {
        self.tmux_ok(&["send-keys", "-t", target, "-l", text]);
    }

    /// Sends a named key (e.g. `"Escape"`, `"Enter"`), NOT run through the
    /// literal (`-l`) path.
    fn send_key(&self, target: &str, key: &str) {
        self.tmux_ok(&["send-keys", "-t", target, key]);
    }

    fn set_option(&self, target: &str, option: &str, value: &str) {
        self.tmux_ok(&["set-option", "-t", target, option, value]);
    }

    fn session_names(&self) -> Vec<String> {
        let out = self
            .tmux()
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .expect("list-sessions failed to spawn");
        if !out.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
    }
}

/// Polls `capture()` every 100ms until `predicate` holds, or panics
/// (including the last captured text) after `timeout`.
fn wait_for(
    timeout: Duration,
    mut capture: impl FnMut() -> String,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let start = Instant::now();
    loop {
        let last = capture();
        if predicate(&last) {
            return last;
        }
        if start.elapsed() >= timeout {
            panic!(
                "condition not met within {:?}; last capture-pane output:\n{}",
                timeout, last
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Shell argv that runs pckr inside a pane against `socket`, staying alive
/// (pckr itself blocks in its event loop until quit/switch/kill-and-exit).
fn pckr_argv(socket: &str) -> [String; 3] {
    [
        "sh".to_string(),
        "-c".to_string(),
        format!("TMUX_PICKER_SOCKET={socket} {}", pckr_bin()),
    ]
}

/// Writes a fresh, executable stub `tm` shell script (mode 0o755) into a new
/// `fresh_dir` and returns that directory. The script body is `#!/bin/sh`
/// followed verbatim by `script_body` (so it can print classification lines
/// and/or `exit <n>`). Callers put this directory FIRST on `PATH` when
/// launching the pckr pane (see `pckr_argv_with_tm_stub`) so the pckr
/// process's `tm runs kill-safety <name>` subprocess call resolves to this
/// stub rather than any real `tm` on the host.
fn stub_tm_dir(label: &str, script_body: &str) -> PathBuf {
    let dir = fresh_dir(label);
    let script_path = dir.join("tm");
    std::fs::write(&script_path, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();
    dir
}

/// Like `pckr_argv`, but prepends `stub_dir` to `PATH` so the pckr process
/// (and anything it shells out to, e.g. `tm runs kill-safety`) sees the stub
/// `tm` first. The current process's own `PATH` (which includes the nix dev
/// shell's tmux/git) is preserved after it, via `env PATH=<stub>:<PATH> ...`.
fn pckr_argv_with_tm_stub(socket: &str, stub_dir: &Path) -> [String; 3] {
    let current_path = std::env::var("PATH").unwrap_or_default();
    [
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "TMUX_PICKER_SOCKET={socket} PATH={}:{current_path} {}",
            stub_dir.display(),
            pckr_bin()
        ),
    ]
}

/// Like `pckr_argv`, but writes pckr's exit code to `marker_path` afterward
/// so tests can observe clean process exit by reading a file directly.
///
/// This deliberately does NOT rely on `capture-pane` to observe the exit:
/// on the tmux build this suite was developed against (3.7b), a pane whose
/// process has exited — even with `remain-on-exit on` set well before the
/// process exits — reliably renders as a blank screen plus the "Pane is
/// dead" placeholder, with none of the pane's actual last output visible.
/// Writing straight to a file sidesteps that entirely.
fn pckr_argv_with_exit_marker(socket: &str, marker_path: &Path) -> [String; 3] {
    [
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "TMUX_PICKER_SOCKET={socket} {}; echo $? > {}",
            pckr_bin(),
            marker_path.display()
        ),
    ]
}

// --- (a) list rendering ------------------------------------------------

#[test]
fn list_rendering_shows_header_help_sessions_and_current_marker() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("list");

    let dir_a = fresh_dir("list-a");
    let dir_b = fresh_dir("list-b");
    let dir_host = fresh_dir("list-host");

    server.new_session("session-a", &dir_a, &["sh"]);
    server.new_session("session-b", &dir_b, &["sh"]);
    let argv = pckr_argv(&server.socket);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("session-a") && t.contains("session-b") && t.contains("pckr-host"),
    );

    assert!(text.contains(
        "NORMAL — enter:switch | x:kill | g:root | 1-9:jump | i:filter | q/esc:quit | [merged]=safe to close"
    ));
    assert!(text.contains("SESSION"), "header must be rendered:\n{text}");
    assert!(text.contains("[N] session >"));
    assert!(
        text.contains("* "),
        "current session (pckr-host) must show the marker:\n{text}"
    );

    server.send_key("pckr-host", "q");
}

// --- (b) ATTN contract ---------------------------------------------------

#[test]
fn attn_column_renders_picker_status_and_picker_server() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("attn");

    let dir_a = fresh_dir("attn-a");
    let dir_b = fresh_dir("attn-b");
    let dir_host = fresh_dir("attn-host");

    server.new_session("session-a", &dir_a, &["sh"]);
    server.new_session("session-b", &dir_b, &["sh"]);
    server.set_option("session-a", "@picker_status", "\u{2753}");
    server.set_option("session-b", "@picker_server", "\u{1f525}");

    let argv = pckr_argv(&server.socket);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("session-a") && t.contains("session-b"),
    );

    assert!(
        text.contains('\u{2753}'),
        "@picker_status symbol must render:\n{text}"
    );
    assert!(
        text.contains('\u{1f525}'),
        "@picker_server symbol must render:\n{text}"
    );

    server.send_key("pckr-host", "q");
}

// --- (c) modal safety -----------------------------------------------------

#[test]
fn insert_mode_edits_filter_and_never_kills_a_session() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("modal");

    let dir_a = fresh_dir("modal-a");
    let dir_host = fresh_dir("modal-host");

    server.new_session("session-a", &dir_a, &["sh"]);
    let argv = pckr_argv(&server.socket);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("[N] session >"),
    );

    server.send_literal("pckr-host", "i");
    server.send_literal("pckr-host", "x");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("[I] filter > x"),
    );
    assert!(text.contains("[I] filter > x"));
    assert!(text.contains("INSERT — type to filter | enter:switch | esc:normal mode"));

    let sessions = server.session_names();
    assert!(
        sessions.contains(&"session-a".to_string()),
        "typing 'x' in insert mode must not kill a session; sessions: {sessions:?}"
    );
    assert!(
        sessions.contains(&"pckr-host".to_string()),
        "typing 'x' in insert mode must not kill the host session; sessions: {sessions:?}"
    );

    // tmux `capture-pane` trims trailing whitespace from each line, so the
    // trailing space in the literal prompt text (`"[N] session > "`) never
    // survives capture — match without it.
    server.send_key("pckr-host", "Escape");
    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("[N] session >"),
    );
    assert!(text.contains("[N] session >"));

    server.send_key("pckr-host", "q");
}

// --- (d) kill + worktree cleanup -------------------------------------------

#[test]
fn kill_removes_session_worktree_directory_and_worktree_registration_when_tier_is_safe() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("kill");

    let stub_dir = stub_tm_dir(
        "kill-safe-stub",
        "echo safe\necho classified as safe by stub\nexit 0",
    );

    // A real git repo + linked worktree, both under fresh temp dirs — never
    // this test binary's own checkout (see module-level SAFETY note).
    let main_repo = fresh_dir("kill-main-repo");
    git(&main_repo, &["init", "-q", "-b", "main"]);
    std::fs::write(main_repo.join("f.txt"), "x\n").unwrap();
    git(&main_repo, &["add", "f.txt"]);
    git_commit(&main_repo, "initial");

    let worktree_dir = fresh_dir("kill-worktree");
    // `worktree add` requires the target not already exist as a non-empty
    // dir when created fresh by git itself, so remove the placeholder first.
    std::fs::remove_dir(&worktree_dir).unwrap();
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree_dir.to_str().unwrap(),
            "-b",
            "wt-branch",
        ],
    );
    assert!(worktree_dir.join(".git").is_file());

    let dir_host = fresh_dir("kill-host");
    server.new_session("wt-target", &worktree_dir, &["sh"]);
    let argv = pckr_argv_with_tm_stub(&server.socket, &stub_dir);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("wt-target"),
    );

    // Selection starts on row 0 (pckr-host, the current session). Move down
    // once to select wt-target, then kill it. Killing the current session
    // is a silent no-op in pckr, so we must not select row 0.
    server.send_literal("pckr-host", "j");
    server.send_literal("pckr-host", "x");

    wait_for(
        DEFAULT_TIMEOUT,
        || server.session_names().join(","),
        |names| !names.split(',').any(|n| n == "wt-target"),
    );

    let sessions = server.session_names();
    assert!(
        !sessions.contains(&"wt-target".to_string()),
        "wt-target session must be gone after kill; sessions: {sessions:?}"
    );
    assert!(
        !worktree_dir.exists(),
        "worktree directory must be removed after kill"
    );

    let worktree_list =
        String::from_utf8_lossy(&git(&main_repo, &["worktree", "list"]).stdout).to_string();
    assert!(
        !worktree_list.contains(worktree_dir.to_str().unwrap()),
        "git worktree list must no longer show the removed worktree:\n{worktree_list}"
    );

    // The safe tier must not leave a confirmation prompt on screen after
    // the kill completes (the full capture history isn't observable, so
    // this is the closest available "never prompted" assertion).
    let text = server.capture_pane("pckr-host");
    assert!(
        !text.contains("y=kill"),
        "safe tier must never enter confirmation mode:\n{text}"
    );

    server.send_key("pckr-host", "q");
}

#[test]
fn kill_prompts_and_respects_decline_then_confirm_when_tier_is_live_run() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("killliverun");

    let stub_dir = stub_tm_dir(
        "kill-liverun-stub",
        "echo live-run\necho a test is running in this session\nexit 0",
    );

    // A real git repo + linked worktree, both under fresh temp dirs — never
    // this test binary's own checkout (see module-level SAFETY note).
    let main_repo = fresh_dir("killliverun-main-repo");
    git(&main_repo, &["init", "-q", "-b", "main"]);
    std::fs::write(main_repo.join("f.txt"), "x\n").unwrap();
    git(&main_repo, &["add", "f.txt"]);
    git_commit(&main_repo, "initial");

    let worktree_dir = fresh_dir("killliverun-worktree");
    std::fs::remove_dir(&worktree_dir).unwrap();
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree_dir.to_str().unwrap(),
            "-b",
            "wt-branch",
        ],
    );
    assert!(worktree_dir.join(".git").is_file());

    let dir_host = fresh_dir("killliverun-host");
    server.new_session("wt-target", &worktree_dir, &["sh"]);
    let argv = pckr_argv_with_tm_stub(&server.socket, &stub_dir);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("wt-target"),
    );

    // Row 0 is pckr-host (the current session, never selectable for kill);
    // move down once to select wt-target.
    server.send_literal("pckr-host", "j");
    server.send_literal("pckr-host", "x");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("y=kill  any other key=cancel") && t.contains("[live run]"),
    );
    assert!(
        text.contains("Kill 'wt-target' + worktree? [live run]"),
        "confirm prompt must show the live-run label:\n{text}"
    );

    // Decline with 'n' (one of "any other key"): must cancel back to NORMAL,
    // leaving the session and worktree untouched.
    server.send_literal("pckr-host", "n");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("NORMAL — enter:switch"),
    );
    assert!(
        !text.contains("y=kill"),
        "declining must return to NORMAL mode, not linger in confirm mode:\n{text}"
    );

    let sessions = server.session_names();
    assert!(
        sessions.contains(&"wt-target".to_string()),
        "declining the kill must leave wt-target alive; sessions: {sessions:?}"
    );
    assert!(
        worktree_dir.exists(),
        "declining the kill must leave the worktree directory in place"
    );
    let worktree_list =
        String::from_utf8_lossy(&git(&main_repo, &["worktree", "list"]).stdout).to_string();
    assert!(
        worktree_list.contains(worktree_dir.to_str().unwrap()),
        "declining the kill must leave the worktree registered:\n{worktree_list}"
    );

    // Now confirm: press x again, wait for the prompt, then 'y'.
    server.send_literal("pckr-host", "x");
    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("y=kill  any other key=cancel") && t.contains("[live run]"),
    );
    server.send_literal("pckr-host", "y");

    wait_for(
        DEFAULT_TIMEOUT,
        || server.session_names().join(","),
        |names| !names.split(',').any(|n| n == "wt-target"),
    );

    let sessions = server.session_names();
    assert!(
        !sessions.contains(&"wt-target".to_string()),
        "confirming the kill must remove wt-target; sessions: {sessions:?}"
    );
    assert!(
        !worktree_dir.exists(),
        "confirming the kill must remove the worktree directory"
    );
    let worktree_list =
        String::from_utf8_lossy(&git(&main_repo, &["worktree", "list"]).stdout).to_string();
    assert!(
        !worktree_list.contains(worktree_dir.to_str().unwrap()),
        "confirming the kill must deregister the worktree:\n{worktree_list}"
    );

    server.send_key("pckr-host", "q");
}

#[test]
fn kill_prompts_with_root_session_label_and_escape_cancels() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("killroot");

    let stub_dir = stub_tm_dir(
        "kill-root-stub",
        "echo root-session\necho this is the root checkout\nexit 0",
    );

    let dir_target = fresh_dir("killroot-target");
    let dir_host = fresh_dir("killroot-host");

    server.new_session("root-target", &dir_target, &["sh"]);
    let argv = pckr_argv_with_tm_stub(&server.socket, &stub_dir);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("root-target"),
    );

    server.send_literal("pckr-host", "j");
    server.send_literal("pckr-host", "x");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("y=kill  any other key=cancel") && t.contains("[root session]"),
    );
    assert!(
        text.contains("Kill 'root-target' + worktree? [root session]"),
        "confirm prompt must show the root-session label:\n{text}"
    );

    server.send_key("pckr-host", "Escape");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("NORMAL — enter:switch"),
    );
    assert!(
        !text.contains("y=kill"),
        "Escape must return to NORMAL mode:\n{text}"
    );

    let sessions = server.session_names();
    assert!(
        sessions.contains(&"root-target".to_string()),
        "cancelling via Escape must leave root-target alive; sessions: {sessions:?}"
    );

    server.send_key("pckr-host", "q");
}

#[test]
fn kill_defaults_to_unclassified_label_when_tm_fails() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("killunknown");

    // A `tm` that exits non-zero must be treated as `unknown` regardless of
    // what (if anything) it printed.
    let stub_dir = stub_tm_dir("kill-unknown-stub", "exit 1");

    let dir_target = fresh_dir("killunknown-target");
    let dir_host = fresh_dir("killunknown-host");

    server.new_session("unknown-target", &dir_target, &["sh"]);
    let argv = pckr_argv_with_tm_stub(&server.socket, &stub_dir);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("unknown-target"),
    );

    server.send_literal("pckr-host", "j");
    server.send_literal("pckr-host", "x");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("y=kill  any other key=cancel") && t.contains("[unclassified]"),
    );
    assert!(
        text.contains("Kill 'unknown-target' + worktree? [unclassified]"),
        "a failing tm must default to the unclassified label:\n{text}"
    );

    server.send_key("pckr-host", "Escape");

    let text = wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("NORMAL — enter:switch"),
    );
    assert!(
        !text.contains("y=kill"),
        "cancel must return to NORMAL:\n{text}"
    );

    let sessions = server.session_names();
    assert!(
        sessions.contains(&"unknown-target".to_string()),
        "cancelling must leave the session alive; sessions: {sessions:?}"
    );

    server.send_key("pckr-host", "q");
}

// --- (e) switch -------------------------------------------------------------

#[test]
fn digit_jump_switches_and_pckr_exits_cleanly() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("switch");

    let dir_a = fresh_dir("switch-a");
    let dir_host = fresh_dir("switch-host");
    let marker_dir = fresh_dir("switch-marker");
    let marker_path = marker_dir.join("exit-code");

    server.new_session("session-a", &dir_a, &["sh"]);
    let argv = pckr_argv_with_exit_marker(&server.socket, &marker_path);
    let argv_ref: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    server.new_session("pckr-host", &dir_host, &argv_ref);

    wait_for(
        DEFAULT_TIMEOUT,
        || server.capture_pane("pckr-host"),
        |t| t.contains("session-a"),
    );

    // Row 1 (1-based) is whichever session sorts first; jumping to "1"
    // always resolves to *some* visible row and exits via Effect::Switch.
    server.send_literal("pckr-host", "1");

    // No client is ever attached to this detached test server, so
    // `tmux switch-client` has nothing to switch (best-effort, silently a
    // no-op per tmux.rs) and `tmux list-clients` is always empty here — not
    // a usable observable in this harness. What *is* observable and proves
    // the digit-jump -> Effect::Switch -> switch_client -> exit path ran to
    // completion is that the pckr process itself exits cleanly afterward,
    // which we detect via the exit-code marker file (see
    // `pckr_argv_with_exit_marker` for why not `capture-pane`).
    let exit_code = wait_for(
        DEFAULT_TIMEOUT,
        || std::fs::read_to_string(&marker_path).unwrap_or_default(),
        |t| !t.trim().is_empty(),
    );
    assert_eq!(
        exit_code.trim(),
        "0",
        "pckr must exit 0 after a digit-jump switch"
    );

    let clients = server
        .tmux()
        .args(["list-clients", "-F", "#{client_session}"])
        .output()
        .expect("list-clients failed to spawn");
    assert!(
        String::from_utf8_lossy(&clients.stdout).trim().is_empty(),
        "no client is ever attached in this harness, so list-clients is expected to be empty"
    );
}

// --- (f) jump-root as CLI ---------------------------------------------------

#[test]
fn jump_root_and_root_session_cli_resolve_across_sessions() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let server = TestServer::new("jumproot");

    let dir_root = fresh_dir("jumproot-root");
    let dir_child = fresh_dir("jumproot-child");

    server.new_session("root-repo", &dir_root, &["sh"]);
    server.new_session("child", &dir_child, &["sh"]);
    server.set_option("child", "@root_session", "root-repo");

    let jump_root_output = Command::new(pckr_bin())
        .env("TMUX_PICKER_SOCKET", &server.socket)
        .args(["jump-root", "child"])
        .output()
        .expect("failed to run pckr jump-root");
    assert!(
        jump_root_output.status.success(),
        "pckr jump-root child must exit 0; stderr: {}",
        String::from_utf8_lossy(&jump_root_output.stderr)
    );

    let root_session_output = Command::new(pckr_bin())
        .env("TMUX_PICKER_SOCKET", &server.socket)
        .args(["root-session", "child"])
        .output()
        .expect("failed to run pckr root-session");
    assert!(root_session_output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&root_session_output.stdout)
            .trim_end()
            .to_string(),
        "root-repo"
    );
}
