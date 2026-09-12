//! pckr — tmux session picker CLI dispatch.
//!
//! This slice implements everything except the ratatui TUI (slice 2): the
//! non-interactive subcommands used both by the eventual TUI and directly
//! by tests / tmux key bindings. See docs/parity.md, section "Invocation".

mod actions;
mod app;
mod gitinfo;
mod kill_safety;
mod model;
mod render;
mod tmux;
mod ui;

use std::env;
use std::path::Path;
use std::process::ExitCode;

use tmux::Tmux;

fn usage() -> &'static str {
    "usage: pckr <list [--plain] | branch-status <path> | project-name <path> | kill <session> | root-session [<session>] | jump-root [<session>]>"
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        let tmux = Tmux::new();
        return match ui::run(&tmux) {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                eprintln!("pckr: {e}");
                ExitCode::from(1)
            }
        };
    }

    let tmux = Tmux::new();

    match args[0].as_str() {
        "list" => cmd_list(&tmux, &args[1..]),
        "branch-status" => cmd_branch_status(&args[1..]),
        "project-name" => cmd_project_name(&args[1..]),
        "kill" => cmd_kill(&tmux, &args[1..]),
        "root-session" => cmd_root_session(&tmux, &args[1..]),
        "jump-root" => cmd_jump_root(&tmux, &args[1..]),
        _ => {
            eprintln!("{}", usage());
            ExitCode::from(2)
        }
    }
}

fn cmd_list(tmux: &Tmux, args: &[String]) -> ExitCode {
    model::run_refresh_hook(tmux);
    let rows = model::build_rows(tmux);
    let plain = args.iter().any(|a| a == "--plain");
    if plain {
        println!("{}", render::to_plain_tsv(&rows));
    } else {
        println!("{}", render::to_table(&rows));
    }
    ExitCode::from(0)
}

fn cmd_branch_status(args: &[String]) -> ExitCode {
    let path = match args.first() {
        Some(p) => p,
        None => {
            eprintln!("usage: pckr branch-status <path>");
            return ExitCode::from(2);
        }
    };
    if let Some(bs) = gitinfo::branch_status(Path::new(path)) {
        match bs.state {
            gitinfo::MergeState::Detached => println!("[detached]"),
            gitinfo::MergeState::Merged => println!("{} [merged]", bs.branch),
            gitinfo::MergeState::Unmerged => println!("{} [unmerged]", bs.branch),
        }
    }
    ExitCode::from(0)
}

fn cmd_project_name(args: &[String]) -> ExitCode {
    let path = match args.first() {
        Some(p) => p,
        None => {
            eprintln!("usage: pckr project-name <path>");
            return ExitCode::from(2);
        }
    };
    match gitinfo::project_name(Path::new(path)) {
        Some(name) => println!("{name}"),
        None => println!("-"),
    }
    ExitCode::from(0)
}

fn cmd_kill(tmux: &Tmux, args: &[String]) -> ExitCode {
    if let Some(session) = args.first() {
        actions::kill_session(tmux, session);
    }
    ExitCode::from(0)
}

fn cmd_root_session(tmux: &Tmux, args: &[String]) -> ExitCode {
    let session = args.first().map(|s| s.as_str());
    match actions::root_session(tmux, session) {
        Some(root) => {
            println!("{root}");
            ExitCode::from(0)
        }
        None => ExitCode::from(1),
    }
}

fn cmd_jump_root(tmux: &Tmux, args: &[String]) -> ExitCode {
    let session = args.first().map(|s| s.as_str());
    actions::jump_root(tmux, session);
    ExitCode::from(0)
}
