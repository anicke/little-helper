//! `lh` — the headless half of Lossless Little Helper, as a library.
//!
//! Split out of what used to be `main.rs` so `lh-tui` can parse the exact same subcommands
//! and run them exactly the same way: every command lh-cli knows is callable from lh-tui
//! today, even before it has a screen of its own (`lh-tui/src/main.rs`).
//!
//! Exit codes are `lh-cli/src/main.rs`'s contract, not this crate's: `run` just returns
//! whether every file passed.

use anyhow::Result;
use clap::{Parser, Subcommand};
use lh_core::checksum::ChecksumKind;
use lh_core::job::{CancelToken, Event, Progress, Queue};
use lh_core::model::AudioFile;
use lh_core::scan;
use std::path::PathBuf;

mod commands;
use commands::*;
pub use commands::{print_fixed, print_in_place, print_tail_note, repair_extension, tail_policy};

/// Run one job per file on a bounded worker pool if there is more than one file — a
/// single file just runs directly, since spinning up a pool and a channel for one job is
/// pure overhead with nothing to show for it (docs/job-queue.md §3). `Ctrl-C` cancels: a
/// file already being worked on finishes normally, nothing queued behind it starts.
///
/// `job` gets a `Progress<T>` even outside a real queue (§8's `Progress::detached`) so a
/// job that can check its own cancellation mid-run — `convert`, via `to_flac` / `to_wav`
/// — behaves the same whether it is the only file or one of a batch.
///
/// Results come back paired with their file in submission order, not completion order —
/// a script piping our stdout should see the same thing on every run, even though the
/// work itself now happens in parallel. `None` means the file's job never started because
/// the batch was cancelled first; a job stopped mid-run instead produces its own `T`
/// carrying `Error::Cancelled`, since only the job itself — not `run_batch` — knows how to
/// tell "stopped" apart from any other failure for its own operation.
pub(crate) fn run_batch<T: Send + 'static>(
    files: &[AudioFile],
    job: impl Fn(&AudioFile, &Progress<T>) -> T + Send + Sync + 'static,
) -> Vec<(AudioFile, Option<T>)> {
    if files.len() <= 1 {
        let cancel = CancelToken::new();
        let progress = Progress::detached(cancel.clone());
        // Only one batch runs per process invocation, so this is the only call site.
        let _ = ctrlc::set_handler(move || cancel.cancel());
        return files
            .iter()
            .map(|f| (f.clone(), Some(job(f, &progress))))
            .collect();
    }

    let job = std::sync::Arc::new(job);
    let queue: Queue<T> = Queue::new();
    let cancel = queue.cancel_token();
    // Only one batch runs per process invocation, so this is the only call site.
    let _ = ctrlc::set_handler(move || cancel.cancel());

    for f in files {
        let f = f.clone();
        let job = job.clone();
        queue.submit(f.file_name(), move |progress| job(&f, progress));
    }

    let total = files.len();
    let mut results: Vec<Option<T>> = (0..total).map(|_| None).collect();
    let mut done = 0usize;
    while done < total {
        match queue
            .events()
            .recv()
            .expect("queue closed with jobs still outstanding")
        {
            Event::Finished { id, output, .. } => {
                results[id.index()] = Some(output);
                done += 1;
            }
            Event::Cancelled { .. } => done += 1,
            Event::Started { .. } | Event::Progress { .. } => continue,
        }
        eprint!("\r{done} of {total} done");
    }
    eprintln!();

    files.iter().cloned().zip(results).collect()
}

#[derive(Parser)]
#[command(
    name = "lh",
    version,
    about = "Lossless Little Helper — verify, checksum and convert audio for traders"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show what each file is, including the encoder that produced it.
    Info(Paths),
    /// Decode each file and check it against the MD5 it carries.
    Verify(Paths),
    /// Report sector boundary errors, or repair them (`sbe fix`).
    Sbe(SbeArgs),
    /// Write or print FFP checksums (audio MD5 from the FLAC header).
    Ffp(ChecksumArgs),
    /// Write or print MD5 checksums of the file bytes.
    Md5(ChecksumArgs),
    /// Write or print ST5 checksums (audio data only).
    St5(ChecksumArgs),
    /// Decode FLAC to WAV, or encode WAV to FLAC with the reference encoder.
    Convert(ConvertArgs),
    /// Show the reference binaries we found, with versions and hashes.
    Tools,
    /// Work with .torrent files.
    Torrent {
        #[command(subcommand)]
        command: TorrentCommand,
    },
    /// Check files against an existing .ffp, .md5 or .st5 file.
    Check {
        /// The checksum file. Its kind is taken from the extension.
        file: PathBuf,
    },
    /// Write the etree Vorbis-comment tags for one show (docs/tagging.md §5).
    Tag(TagArgs),
    /// Rename one show's files to the etree track-name standard (docs/tagging.md §5).
    Rename(RenameArgs),
    /// Write the show's info .txt: header, setlist and track times, from the tags
    /// (docs/info-file.md).
    Setlist(SetlistArgs),
    /// Cut a short MP3 sample from one track, under a size limit (docs/sample.md).
    Sample(SampleArgs),
}

#[derive(Subcommand)]
pub enum TorrentCommand {
    /// Show what a .torrent contains: infohash, trackers, pieces and file list.
    Info {
        /// The .torrent file.
        file: PathBuf,
        /// Suppress the file listing.
        #[arg(long)]
        no_files: bool,
    },
    /// Make a .torrent for a show.
    Create(TorrentCreateArgs),
    /// List the trackers we know about, with the date each was last checked.
    Trackers,
    /// Check local files against a .torrent.
    Check {
        /// The .torrent file.
        file: PathBuf,
        /// Where the files are. Either the folder containing the show or the show
        /// folder itself; both work.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Compare sizes only, without reading file contents.
        #[arg(long)]
        quick: bool,
    },
}

#[derive(clap::Args)]
pub struct TorrentCreateArgs {
    /// The folder to make a torrent for, or a single file.
    pub path: PathBuf,
    /// Where to write the .torrent. Defaults to beside the source folder — writing it
    /// inside the folder would add a file to what the torrent describes.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// A tracker: an id from `lh torrent trackers`, or an announce URL, which is used
    /// verbatim. Repeat it for more; each one becomes its own tier.
    #[arg(long = "tracker", value_name = "ID|URL")]
    pub trackers: Vec<String>,
    /// Piece length in bytes: a power of two from 16384 to 16777216. Chosen from the
    /// payload size when omitted.
    #[arg(long, value_name = "BYTES")]
    pub piece_length: Option<u64>,
    /// Mark the torrent private (BEP 27). This is part of the infohash, so it cannot be
    /// added or removed afterwards — it makes a different torrent.
    #[arg(long)]
    pub private: bool,
    /// The source tag some private trackers require. Also part of the infohash.
    #[arg(long, value_name = "TAG")]
    pub source: Option<String>,
    #[arg(long, value_name = "TEXT")]
    pub comment: Option<String>,
    /// Include files normally left out: Thumbs.db, .DS_Store, other .torrent files.
    #[arg(long)]
    pub include_all: bool,
    /// Overwrite an existing .torrent. The payload is never touched either way.
    #[arg(long)]
    pub force: bool,
}

#[derive(clap::Args)]
pub struct Paths {
    /// Files or folders. Defaults to the current directory.
    #[arg(default_value = ".")]
    pub paths: Vec<PathBuf>,
    /// Descend into subdirectories.
    #[arg(short, long)]
    pub recursive: bool,
}

/// `lh sbe` with no subcommand reports (the existing behaviour); `lh sbe fix` plans a
/// repair. Clap resolves `fix` as the subcommand before it would be swallowed by `paths`
/// below, so both forms coexist under one command.
#[derive(clap::Args)]
pub struct SbeArgs {
    #[command(subcommand)]
    pub command: Option<SbeSub>,
    #[command(flatten)]
    pub paths: Paths,
}

#[derive(Subcommand)]
pub enum SbeSub {
    /// Repair sector boundary errors across one directory's files, taken in filename order
    /// (docs/sbe-repair.md). Use `--dry-run` to only print the plan.
    Fix(SbeFixArgs),
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    Backward,
    Forward,
    Nearest,
}

impl From<Direction> for lh_core::repair::BoundaryDirection {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Backward => Self::Backward,
            Direction::Forward => Self::Forward,
            Direction::Nearest => Self::Nearest,
        }
    }
}

#[derive(clap::Args)]
pub struct SbeFixArgs {
    /// The directory whose files are one ordered set, in filename order.
    pub dir: PathBuf,
    /// Which way a misaligned boundary's remainder frames move (shntool's -b/-f/-u).
    #[arg(long, value_enum, default_value = "backward")]
    pub direction: Direction,
    /// Close a misaligned last file by adding silence, instead of leaving it reported.
    #[arg(long)]
    pub pad_tail: bool,
    /// Print the plan and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Write repaired files here. To execute, give this or `--in-place` (never the source
    /// directory's own files — Principle 1, repair never overwrites originals).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Replace each file the fix changes under its own name, moving the file it replaces
    /// into `_original/` inside the folder. Files the fix leaves alone are not touched.
    #[arg(long, conflicts_with_all = ["output", "overwrite", "dry_run"])]
    pub in_place: bool,
    /// Overwrite outputs that already exist at the destination. Sources are never touched
    /// either way.
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    Wav,
    Flac,
}

#[derive(clap::Args)]
pub struct ConvertArgs {
    #[command(flatten)]
    pub paths: Paths,
    /// What to produce. Files already in that format are left alone.
    #[arg(long, value_enum)]
    pub to: Target,
    /// Write outputs here instead of beside their sources.
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
    /// flac's compression level, 0 to 8. Only used when encoding.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(0..=8))]
    pub level: u8,
    /// Overwrite outputs that already exist. Sources are never touched either way.
    #[arg(long)]
    pub force: bool,
    /// Print the full provenance record for every file written.
    #[arg(long)]
    pub provenance: bool,
    /// Once a WAV's FLAC checks out against it, move the WAV into `_original/` beside it.
    /// Only with `--to flac`; a FLAC source is the archival copy and stays put.
    #[arg(long)]
    pub move_sources: bool,
}

#[derive(clap::Args)]
pub struct ChecksumArgs {
    #[command(flatten)]
    pub paths: Paths,
    /// Write to this file instead of printing to stdout.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct TagArgs {
    /// The show's folder. A show is the unit: a track number only means something
    /// relative to its siblings.
    pub dir: PathBuf,
    #[arg(long)]
    pub artist: Option<String>,
    #[arg(long)]
    pub album: Option<String>,
    #[arg(long)]
    pub date: Option<String>,
    #[arg(long)]
    pub genre: Option<String>,
    #[arg(long)]
    pub comment: Option<String>,
    #[arg(long)]
    pub location: Option<String>,
    /// One title per line, in file order (the same order the diff and the write use).
    /// `-` reads stdin.
    #[arg(long)]
    pub titles: Option<PathBuf>,
    /// Write the changes. Without it, the full diff is printed and nothing is touched —
    /// there is deliberately no separate `--dry-run`, since the preview already is one.
    #[arg(long)]
    pub yes: bool,
}

#[derive(clap::Args)]
pub struct RenameArgs {
    /// The show's folder. Renaming touches the files in it, never the folder itself.
    pub dir: PathBuf,
    /// Defaults from the folder's own name when it is an etree show name.
    #[arg(long)]
    pub band: Option<String>,
    /// `YYYY-MM-DD` or `YY-MM-DD`. Defaults from the folder's own name when it is an
    /// etree show name.
    #[arg(long)]
    pub date: Option<String>,
    /// Render `77-05-08` instead of `1977-05-08`.
    #[arg(long)]
    pub short_year: bool,
    #[arg(long)]
    pub disc: Option<u32>,
    /// Write the renames. Without it, the full diff is printed and nothing is touched.
    #[arg(long)]
    pub yes: bool,
}

#[derive(clap::Args)]
pub struct SetlistArgs {
    /// The show's folder. The file is written inside it, named `bbyyyy-mm-dd.txt`.
    pub dir: PathBuf,
    /// Write the file. Without it, the text is printed and nothing is touched.
    #[arg(long)]
    pub yes: bool,
    /// Replace an existing file of the same name. It is usually hand-written, so this is
    /// never the default.
    #[arg(long)]
    pub force: bool,
}

#[derive(clap::Args)]
pub struct SampleArgs {
    /// The track to take the sample from. `lh-tui` also takes the show's folder, and
    /// lets you pick the track.
    pub path: PathBuf,
    /// Where in the track it starts: `90`, `1:30` or `1:02:03`. Defaults to centring the
    /// clip in the track.
    #[arg(long, value_parser = parse_time)]
    pub start: Option<f64>,
    /// How long it is, in the same form as `--start`. Defaults to the longest clip that
    /// fits `--max-size`, up to 30 seconds.
    #[arg(long, value_parser = parse_time)]
    pub length: Option<f64>,
    /// 320, 256, 192, 160 or 128 for CBR; v0 or v2 for VBR, whose size is only known
    /// once it is encoded.
    #[arg(long, default_value = "256", value_parser = parse_mode)]
    pub mode: lh_core::sample::Mode,
    /// The largest the sample may be: `1M`, `900K`, `1MiB` or bytes. `K` and `M` are
    /// decimal.
    #[arg(long, default_value = "1M", value_parser = parse_size)]
    pub max_size: u64,
    /// Where to write it. Defaults to `<track>.sample.mp3` beside the show folder, so it
    /// does not end up in the folder's checksums or torrent.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Replace an existing sample.
    #[arg(long)]
    pub force: bool,
    /// Print the full provenance record for the sample.
    #[arg(long)]
    pub provenance: bool,
}

fn parse_time(s: &str) -> std::result::Result<f64, String> {
    lh_core::sample::parse_time(s).ok_or_else(|| format!("{s:?} is not a time like 90 or 1:30"))
}

fn parse_mode(s: &str) -> std::result::Result<lh_core::sample::Mode, String> {
    lh_core::sample::Mode::parse(s)
        .ok_or_else(|| format!("{s:?} is not a mode; try 320, 256, 192, 160, 128, v0 or v2"))
}

fn parse_size(s: &str) -> std::result::Result<u64, String> {
    lh_core::sample::parse_size(s).ok_or_else(|| format!("{s:?} is not a size like 1M or 900K"))
}

/// Returns whether every file passed.
pub fn run(cli: Cli) -> Result<bool> {
    match cli.command {
        Command::Info(p) => cmd_info(&p),
        Command::Verify(p) => cmd_verify(&p),
        Command::Sbe(a) => match a.command {
            Some(SbeSub::Fix(fix_args)) => cmd_sbe_fix(&fix_args),
            None => cmd_sbe(&a.paths),
        },
        Command::Ffp(a) => cmd_checksum(ChecksumKind::Ffp, &a),
        Command::Md5(a) => cmd_checksum(ChecksumKind::Md5, &a),
        Command::St5(a) => cmd_checksum(ChecksumKind::St5, &a),
        Command::Check { file } => cmd_check(&file),
        Command::Convert(a) => cmd_convert(&a),
        Command::Tools => cmd_tools(),
        Command::Tag(a) => cmd_tag(&a),
        Command::Rename(a) => cmd_rename(&a),
        Command::Setlist(a) => cmd_setlist(&a),
        Command::Sample(a) => cmd_sample(&a),
        Command::Torrent { command } => match command {
            TorrentCommand::Info { file, no_files } => cmd_torrent_info(&file, !no_files),
            TorrentCommand::Check { file, path, quick } => cmd_torrent_check(&file, &path, quick),
            TorrentCommand::Create(a) => cmd_torrent_create(&a),
            TorrentCommand::Trackers => cmd_torrent_trackers(),
        },
    }
}

/// Expand files and folders into a flat list of audio files, reporting anything skipped
/// rather than dropping it silently.
pub(crate) fn collect(p: &Paths) -> Result<(Vec<AudioFile>, bool)> {
    let set = scan::collect(&p.paths, p.recursive)?;
    let mut clean = true;
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
        clean = false;
    }
    Ok((set.files, clean))
}
