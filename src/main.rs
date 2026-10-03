//! taid — a local-first terminal UI for managing which commits the main
//! repository records for its submodules.
//!
//! Step-2 scaffold: argument handling plus a read-only report produced by
//! the git model. The interactive UI lands in step 4.

mod gitx;

#[cfg(test)]
mod fixture;

use std::process::ExitCode;

use anyhow::Context;

const NAME: &str = "taid";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [] => report_current_repo(),
        [arg] if arg == "--version" || arg == "-V" => {
            println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        [arg] if arg == "--help" || arg == "-h" => {
            print!("{}", help_text());
            ExitCode::SUCCESS
        }
        [arg, ..] => {
            eprintln!("{NAME}: unexpected argument `{arg}`");
            eprintln!("Try `{NAME} --help` for more information.");
            ExitCode::from(2)
        }
    }
}

/// Step-2 stub: print what the read-only git model sees in the current
/// directory, so the model is exercised end to end. The interactive UI
/// replaces this in step 4.
fn report_current_repo() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{NAME}: cannot determine the current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    let repo = match gitx::discover(&cwd).context("looking for a git repository") {
        Ok(Some(repo)) => repo,
        Ok(None) => {
            println!("{NAME}: not a git repository");
            println!("The interactive UI will show a popup for this case (step 4).");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{NAME}: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    match gitx::overview(&repo) {
        Ok(ov) => {
            println!("{NAME} — repository at {}", ov.root.display());
            println!("  HEAD: {}", ov.head.describe());
            println!("  worktree: {}", if ov.dirty { "dirty" } else { "clean" });
            if ov.submodules.is_empty() {
                println!("  submodules: none");
            } else {
                let names: Vec<&str> = ov.submodules.iter().map(|s| s.name.as_str()).collect();
                println!("  submodules: {}", names.join(", "));
            }

            let history = gitx::CommitLog::load(&repo, ov.head_oid).and_then(|mut log| {
                log.ensure_loaded(&repo, 1)?;
                Ok(log)
            });
            match history {
                Ok(log) if log.is_empty() => println!("  history: no commits yet"),
                Ok(log) => {
                    if let Some(c) = log.get(0) {
                        println!(
                            "  history: {} commits; newest: {} {} {} {} — {}",
                            log.len(),
                            c.oid,
                            c.short_sha,
                            c.time_str(),
                            c.author_name,
                            c.title
                        );
                    }
                }
                Err(e) => println!("  history: unavailable ({e})"),
            }
            println!("The interactive UI lands in step 4.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{NAME}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn help_text() -> String {
    format!(
        "\
{NAME} {} — manage which commits your repository records for its submodules

USAGE:
    taid               Launch the TUI in the current directory
    taid --version     Print version information
    taid --help        Print this help

taid is strictly local: it never fetches, never pushes, never rewrites history.
",
        env!("CARGO_PKG_VERSION")
    )
}
