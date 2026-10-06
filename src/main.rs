//! taid's binary: CLI argument handling + TUI bootstrap.

use std::process::ExitCode;

use taid::app::App;
use taid::gitx::{discover, overview};
use taid::{event, term};

const NAME: &str = "taid";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [] => run_tui(),
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

fn run_tui() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("{NAME}: cannot determine the current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build the initial state first: even discovery failures become a
    // popup inside the TUI rather than a bare error on the console.
    let mut app = match discover(&cwd) {
        Ok(Some(repo)) => match overview(&repo) {
            Ok(ov) => App::normal(ov, repo),
            Err(e) => App::error(&cwd, format!("{e:#}")),
        },
        Ok(None) => App::not_repo(&cwd),
        Err(e) => App::error(&cwd, format!("{e:#}")),
    };

    term::install_panic_hook();
    let mut terminal = match term::init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{NAME}: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    let result = event::run(&mut app, &mut terminal);
    term::restore(&mut terminal);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn help_text() -> String {
    format!(
        "\
{NAME} {} — manage driverless repositories and module configurations

USAGE:
    taid               Launch the TUI in the current directory
    taid --version     Print version information
    taid --help        Print this help

Ctrl+R,R repositories · Ctrl+C,C configurations · Ctrl+P,P pull all
YAML forms: Enter edits a parameter; Ctrl+S saves. Quit: q / Esc / Ctrl+Q.
No remote operations run at startup. Ctrl+P,P asks for confirmation;
confirmed pulls are fast-forward only. System Git is required
for remote operations. taid never pushes, creates commits, or rewrites history.
",
        env!("CARGO_PKG_VERSION")
    )
}
