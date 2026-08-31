//! `lh` — the headless half of Little Helper. The actual commands live in `lib.rs`, so
//! that `lh-tui` can parse and run the exact same ones (`lh-tui/src/main.rs`).
//!
//! Exit codes: 0 everything passed, 1 at least one file failed, 2 the command itself failed.

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = lh_cli::Cli::parse();
    match lh_cli::run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lh: {e:#}");
            ExitCode::from(2)
        }
    }
}
