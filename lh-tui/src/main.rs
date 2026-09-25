//! `lh-tui` — a terminal UI for Lossless Little Helper. It parses the exact same subcommands as
//! `lh` (`lh-cli/src/lib.rs`, shared as a library so both binaries stay in lockstep), so
//! every command `lh` knows is callable here too — `lh-tui convert --to flac .` works the
//! same as `lh convert --to flac .`.
//!
//! Given a folder instead of a command, `lh-tui <folder>` opens the workspace: a menu of
//! every screen, grouped by what it is for, each opening on that one folder and returning
//! to the menu when quit (`screens/workspace.rs`, `docs/tui.md` §1).
//!
//! Most commands have an actual screen by now (`docs/tui.md` tracks which); a command with
//! none yet runs exactly as `lh` would — printing to the terminal rather than drawing one —
//! via `run_headless`, which is the permanent fallback for commands that never earn a screen
//! of their own, not a placeholder to delete.
//!
//! `lh_core::analysis::verify` decodes the whole file and never calls `Progress::report`
//! mid-file, so every job here goes straight from Running to a terminal status with no
//! visible sub-progress — the queue's own `Started`/`Finished` events are enough to drive
//! the table and the overall gauge.

mod batch;
mod common;
mod fields;
mod screens;
mod terminal;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use batch::*;
use clap::Parser;
use common::*;
use fields::*;
use lh_cli::{Cli, Command, SbeSub, TorrentCommand};
use lh_core::checksum::ChecksumKind;
use screens::*;
use terminal::*;
use theme::*;

/// `lh-tui`'s own top-level args: the same subcommands `lh_cli::Cli` parses, plus a
/// `--theme` flag that only makes sense for a screen-drawing binary, so it lives here
/// rather than on the `Cli` shared with the headless `lh` binary — and, instead of a
/// subcommand, a bare folder, which opens the workspace (`screens/workspace.rs`).
#[derive(Parser)]
#[command(
    name = "lh-tui",
    version,
    about = "Lossless Little Helper — verify, checksum and convert audio for traders, from a terminal UI"
)]
struct Args {
    /// Color theme for the verify/checksum/torrent screens.
    #[arg(long, value_enum, default_value = "default")]
    theme: ThemeName,
    /// A show folder to open the workspace on: a menu of every screen, each run on that
    /// folder, without restarting for each one. Defaults to the current directory when no
    /// command is given either.
    dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

fn main() -> ExitCode {
    let cli = Args::parse();
    let theme = cli.theme;
    let command = match (cli.command, cli.dir) {
        (Some(command), None) => command,
        (None, dir) => return run_workspace(dir.unwrap_or_else(|| PathBuf::from(".")), theme),
        (Some(_), Some(dir)) => {
            eprintln!(
                "lh-tui: give either a folder or a command, not both ({} came first)",
                dir.display()
            );
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Verify(paths) => run_verify(paths, theme),
        Command::Sbe(a) => match a.command {
            Some(SbeSub::Fix(fix_args)) => run_sbe_fix(fix_args, theme),
            None => run_sbe(a.paths, theme),
        },
        Command::Ffp(args) => run_checksum(ChecksumKind::Ffp, args, theme),
        Command::Md5(args) => run_checksum(ChecksumKind::Md5, args, theme),
        Command::St5(args) => run_checksum(ChecksumKind::St5, args, theme),
        Command::Check { file } => run_check(file, theme),
        Command::Convert(args) => run_convert(args, theme),
        Command::Tag(args) => run_tag(args, theme),
        Command::Rename(args) => run_rename(args, theme),
        Command::Setlist(args) => run_setlist(args, theme),
        Command::Sample(args) => run_sample(args, theme),
        Command::Torrent {
            command: TorrentCommand::Info { file, no_files },
        } => run_torrent_info(file, !no_files, theme),
        Command::Torrent {
            command: TorrentCommand::Create(args),
        } => run_torrent_create(args, theme),
        Command::Torrent {
            command: TorrentCommand::Check { file, path, quick },
        } => run_torrent_check(file, path, quick, theme),
        other => run_headless(Cli { command: other }),
    }
}

/// Every command besides `verify` doesn't have a screen yet, so it runs exactly the way
/// `lh` itself would — same output, same exit-code contract — just from this binary.
fn run_headless(cli: Cli) -> ExitCode {
    match lh_cli::run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            ExitCode::from(2)
        }
    }
}
