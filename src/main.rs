//! taid — a local-first terminal UI for managing which commits the main
//! repository records for its submodules.
//!
//! Step 1 scaffold: CLI argument handling only. The git plumbing lands in
//! steps 2–3 and the TUI itself in step 4.

use std::process::ExitCode;

const NAME: &str = "taid";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [] => {
            println!("taid: the TUI is not implemented yet (planned for step 4)");
            ExitCode::SUCCESS
        }
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
