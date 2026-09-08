//! Kill and root-session/jump-root flows.

use std::path::Path;
use std::process::Command;

use crate::gitinfo;
use crate::tmux::Tmux;

/// Kill flow (docs/parity.md "Kill flow"):
/// 1. Refuse to kill the current session (silent no-op).
/// 2. Capture the session path BEFORE killing.
/// 3. `kill-session` (best-effort).
/// 4. If the path exists and is a linked worktree, `worktree remove
///    --force`, falling back to `rm -rf`, then `worktree prune`
///    (best-effort).
/// 5. Always succeeds.
pub fn kill_session(tmux: &Tmux, name: &str) {
    if let Some(current) = tmux.current_session_name() {
        if current == name {
            return;
        }
    }

    let path = tmux.session_path(name);
    let _ = tmux.kill_session(name);

    let Some(path_str) = path else { return };
    let path = Path::new(&path_str);
    if !path.exists() || !gitinfo::is_worktree(path) {
        return;
    }

    let Some(main_repo) = gitinfo::main_repo_of(path) else {
        return;
    };

    let removed = Command::new("git")
        .arg("-C")
        .arg(&main_repo)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !removed {
        let _ = std::fs::remove_dir_all(path);
    }

    let _ = Command::new("git")
        .arg("-C")
        .arg(&main_repo)
        .args(["worktree", "prune"])
        .output();
}

/// Root-session resolution (docs/parity.md "root-session / jump-root"):
/// (1) the session's `@root_session` tmux option if non-empty; (2)
/// fallback: if the session path is a linked worktree, derive the root
/// session name from the main checkout's dirname basename (dots -> dashes).
/// `session` defaults to the current session when `None`.
pub fn root_session(tmux: &Tmux, session: Option<&str>) -> Option<String> {
    let session_name = match session {
        Some(s) => s.to_string(),
        None => tmux.current_session_name()?,
    };

    if let Some(opt) = tmux.show_option(&session_name, "@root_session") {
        if !opt.is_empty() {
            return Some(opt);
        }
    }

    let path = tmux.session_path(&session_name)?;
    let path = Path::new(&path);
    if gitinfo::is_worktree(path) {
        gitinfo::root_session_name(path)
    } else {
        None
    }
}

/// Switches the client to the resolved root session. Never fails the
/// caller: an unresolvable root, root == self, or a missing root session
/// prints a message to stderr and returns without switching.
pub fn jump_root(tmux: &Tmux, session: Option<&str>) {
    let root = match root_session(tmux, session) {
        Some(r) => r,
        None => {
            eprintln!("pckr: no root session found");
            return;
        }
    };

    if tmux.current_session_name().as_deref() == Some(root.as_str()) {
        eprintln!("pckr: already at root session '{root}'");
        return;
    }

    let exists = tmux.list_sessions().iter().any(|s| s.name == root);
    if !exists {
        eprintln!("pckr: root session '{root}' not found");
        return;
    }

    tmux.switch_client(&root);
}
