//! `lh` — the headless half of Little Helper, as a library.
//!
//! Split out of what used to be `main.rs` so `lh-tui` can parse the exact same subcommands
//! and run them exactly the same way: every command lh-cli knows is callable from lh-tui
//! today, even before it has a screen of its own (`lh-tui/src/main.rs`).
//!
//! Exit codes are `lh-cli/src/main.rs`'s contract, not this crate's: `run` just returns
//! whether every file passed.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lh_core::analysis::{
    BoundaryDirection, FixPlan, RepairEncode, Sbe, TailPolicy, Verification, execute_fix, plan_fix,
    sbe, verify,
};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute, ffp};
use lh_core::convert::{
    Conversion, EncodeOpts, destination, to_flac_cancellable, to_wav_with_progress,
};
use lh_core::etree::{ShowDate, ShowName};
use lh_core::job::{CancelToken, Event, Progress, Queue};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::rename::{NameSpec, RenamePlan, RenameStatus, execute_rename, plan_rename};
use lh_core::tag::{self, Tags};
use lh_core::tools::{Discovery, Registry, ToolId};
use lh_core::torrent::{
    Chosen, CreateOpts, Created, FileStatus, Metainfo, Origin, Passkeys, Tracker, TrackerList,
    Verdict, check, check_sizes, create_with_progress, default_output, resolve,
};
use lh_core::{format, scan};
use std::path::{Path, PathBuf};

/// Run one job per file on a bounded worker pool if there is more than one file — a
/// single file just runs directly, since spinning up a pool and a channel for one job is
/// pure overhead with nothing to show for it (docs/job-queue.md §3). `Ctrl-C` cancels: a
/// file already being worked on finishes normally, nothing queued behind it starts.
///
/// `job` gets a `Progress<T>` even outside a real queue (§8's `Progress::detached`) so a
/// job that can check its own cancellation mid-run — `convert`, via
/// `to_flac_cancellable` / `to_wav_with_progress` — behaves the same whether it is the
/// only file or one of a batch.
///
/// Results come back paired with their file in submission order, not completion order —
/// a script piping our stdout should see the same thing on every run, even though the
/// work itself now happens in parallel. `None` means the file's job never started because
/// the batch was cancelled first; a job stopped mid-run instead produces its own `T`
/// carrying `Error::Cancelled`, since only the job itself — not `run_batch` — knows how to
/// tell "stopped" apart from any other failure for its own operation.
fn run_batch<T: Send + 'static>(
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
    about = "Little Helper — lossless audio for traders"
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
    /// Report sector boundary errors, or plan a repair (`sbe fix`).
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
    /// Plan (but do not yet perform) a sector-boundary repair for one directory's files,
    /// taken in filename order (docs/sbe-repair.md).
    Fix(SbeFixArgs),
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    Backward,
    Forward,
    Nearest,
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
    /// Write repaired files here. Required to execute (never the source directory's own
    /// files — Principle 1, repair never overwrites originals).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
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
pub fn collect(p: &Paths) -> Result<(Vec<AudioFile>, bool)> {
    let mut files = Vec::new();
    let mut clean = true;
    for path in &p.paths {
        if path.is_dir() {
            let set = scan::scan(path, p.recursive)
                .with_context(|| format!("scanning {}", path.display()))?;
            for (skipped, why) in &set.skipped {
                eprintln!("skipped {}: {why}", skipped.display());
                clean = false;
            }
            files.extend(set.files);
        } else {
            match format::probe(path) {
                Ok(f) => files.push(f),
                Err(e) => {
                    eprintln!("skipped {}: {e}", path.display());
                    clean = false;
                }
            }
        }
    }
    Ok((files, clean))
}

fn cmd_info(p: &Paths) -> Result<bool> {
    let (files, clean) = collect(p)?;
    for f in &files {
        let si = &f.stream_info;
        let dur = si
            .duration_secs()
            .map(format_duration)
            .unwrap_or_else(|| "?".into());
        println!(
            "{name}\n  {fmt}  {rate} Hz  {bits}-bit  {ch} ch  {dur}  {size} bytes",
            name = f.file_name(),
            fmt = f.format,
            rate = si.sample_rate,
            bits = si.bits_per_sample,
            ch = si.channels,
            size = f.file_size,
        );
        if let Some(enc) = &f.encoder {
            println!("  encoder: {enc}");
        }
        match sbe(si) {
            Sbe::Aligned => println!("  sbe: aligned"),
            Sbe::Misaligned { remainder_frames } => {
                println!("  sbe: MISALIGNED ({remainder_frames} frames past a sector boundary)")
            }
            Sbe::NotApplicable { reason } => println!("  sbe: n/a ({reason})"),
        }
    }
    Ok(clean)
}

fn cmd_verify(p: &Paths) -> Result<bool> {
    let (files, mut ok) = collect(p)?;
    let results = run_batch(&files, |f, _| verify(&f.path));
    for (f, result) in &results {
        match result {
            Some(Ok(Verification::Ok)) => println!("OK        {}", f.file_name()),
            Some(Ok(Verification::Md5Mismatch { stored, computed })) => {
                ok = false;
                println!(
                    "MISMATCH  {}\n            stored   {}\n            computed {}",
                    f.file_name(),
                    hex::encode(stored),
                    hex::encode(computed)
                );
            }
            Some(Ok(Verification::NoStoredMd5 { .. })) => {
                println!(
                    "NO MD5    {} (decoded cleanly, nothing to compare)",
                    f.file_name()
                )
            }
            Some(Err(e)) => {
                ok = false;
                println!("FAILED    {}: {e}", f.file_name());
            }
            None => {
                ok = false;
                println!("CANCELLED {}", f.file_name());
            }
        }
    }
    Ok(ok)
}

fn cmd_sbe(p: &Paths) -> Result<bool> {
    let (files, mut ok) = collect(p)?;
    let results = run_batch(&files, |f, _| sbe(&f.stream_info));
    for (f, result) in &results {
        match result {
            Some(Sbe::Aligned) => println!("ALIGNED   {}", f.file_name()),
            Some(Sbe::Misaligned { remainder_frames }) => {
                ok = false;
                println!("SBE       {} (+{remainder_frames} frames)", f.file_name());
            }
            Some(Sbe::NotApplicable { reason }) => {
                println!("N/A       {} ({reason})", f.file_name())
            }
            None => {
                ok = false;
                println!("CANCELLED {}", f.file_name());
            }
        }
    }
    Ok(ok)
}

/// Plan a sector-boundary repair for one directory's files, in filename order, and — unless
/// `--dry-run` — execute it: every boundary shift chained left to right, and the tail
/// padded with silence when `--pad-tail` is given (docs/sbe-repair.md R1–R3).
fn cmd_sbe_fix(args: &SbeFixArgs) -> Result<bool> {
    if !args.dir.is_dir() {
        anyhow::bail!("{} is not a directory", args.dir.display());
    }
    let set =
        scan::scan(&args.dir, false).with_context(|| format!("scanning {}", args.dir.display()))?;
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        anyhow::bail!("no audio files found in {}", args.dir.display());
    }

    let direction = match args.direction {
        Direction::Backward => BoundaryDirection::Backward,
        Direction::Forward => BoundaryDirection::Forward,
        Direction::Nearest => BoundaryDirection::Nearest,
    };
    let tail = if args.pad_tail {
        TailPolicy::Pad
    } else {
        TailPolicy::Report
    };

    let plan = plan_fix(&set.files, direction, tail)
        .with_context(|| format!("planning a fix for {}", args.dir.display()))?;

    if args.dry_run {
        print_fix_plan(&args.dir, &set.files, &plan, args.pad_tail);
        return Ok(plan.fully_fixed);
    }

    let out_dir = args.output.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "sbe fix needs -o/--output to execute — it never writes over the originals \
             (Principle 1)"
        )
    })?;
    for f in &set.files {
        if f.format != AudioFormat::Flac {
            anyhow::bail!(
                "sbe fix can only execute against FLAC ({} is {}); other formats have no \
                 repair path yet",
                f.file_name(),
                f.format
            );
        }
    }

    let flac = Registry::discover_one(ToolId::Flac)
        .require(ToolId::Flac)
        .cloned()?;
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: &flac,
        opts: &opts,
        overwrite: args.overwrite,
    };

    let dsts = set
        .files
        .iter()
        .map(|f| {
            destination(&f.path, "flac", Some(out_dir))
                .ok_or_else(|| anyhow::anyhow!("{} has no file name", f.path.display()))
        })
        .collect::<Result<Vec<_>>>()?;

    let fixed = execute_fix(&set.files, &plan, &dsts, &encode)
        .with_context(|| format!("repairing {}", args.dir.display()))?;

    for (f, fixed) in set.files.iter().zip(&fixed) {
        println!(
            "FIXED     {} -> {}   audio md5 {}",
            f.file_name(),
            fixed.path.display(),
            hex::encode(fixed.audio_md5)
        );
    }
    if !plan.fully_fixed {
        println!(
            "{}   still misaligned — rerun with --pad-tail to close it with silence",
            set.files
                .last()
                .expect("checked non-empty above")
                .file_name()
        );
    }

    Ok(plan.fully_fixed)
}

fn print_fix_plan(dir: &Path, files: &[AudioFile], plan: &FixPlan, pad_tail: bool) {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    println!("{name}");

    let file_name = |i: usize| files[i].file_name();
    for b in &plan.boundaries {
        let a = file_name(b.index);
        let bn = file_name(b.index + 1);
        match b.shifted_frames.cmp(&0) {
            std::cmp::Ordering::Equal => println!("  {a} → {bn}   already aligned"),
            std::cmp::Ordering::Greater => println!(
                "  {a} → {bn}   shift {n} frames backward   ({a} was +{n} past a sector)",
                n = b.shifted_frames
            ),
            std::cmp::Ordering::Less => println!(
                "  {a} → {bn}   shift {n} frames forward   ({a} borrows {n} frames from {bn})",
                n = -b.shifted_frames
            ),
        }
    }

    let tail_name = file_name(files.len() - 1);
    match plan.tail_padding_frames {
        Some(pad) => println!(
            "  {tail_name}        last file, would be padded with {pad} frames of silence \
             (not written — planning only)"
        ),
        None if plan.fully_fixed => println!("  {tail_name}        last file, already aligned"),
        None => println!(
            "  {tail_name}        last file, still misaligned once every other boundary is \
             fixed — rerun with --pad-tail to close it with silence"
        ),
    }

    if plan.fully_fixed {
        println!("plan only, nothing written — every file would end up aligned");
    } else if pad_tail {
        println!("plan only, nothing written");
    } else {
        println!("plan only, nothing written — pass --pad-tail to fully align the set");
    }
}

fn cmd_checksum(kind: ChecksumKind, args: &ChecksumArgs) -> Result<bool> {
    let (files, mut ok) = collect(&args.paths)?;
    let mut out = ChecksumFile::new(kind);
    let results = run_batch(&files, move |f, _| compute(kind, &f.path));
    for (f, result) in &results {
        match result {
            Some(Ok(digest)) => out.entries.push(Entry {
                file_name: f.file_name(),
                digest: *digest,
            }),
            Some(Err(e)) => {
                ok = false;
                eprintln!("{}: {e}", f.file_name());
            }
            None => {
                ok = false;
                eprintln!("{}: cancelled", f.file_name());
            }
        }
    }
    match &args.output {
        Some(path) => {
            out.write(path)
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "wrote {} {} entries to {}",
                out.entries.len(),
                kind.label(),
                path.display()
            );
        }
        None => print!("{}", out.render()),
    }
    Ok(ok)
}

/// The `ChecksumKind` a `.ffp`/`.md5`/`.st5` path implies, from its extension — shared
/// with `lh-tui`'s `check` screen so both name a file's kind exactly the way `lh check`
/// does.
pub fn checksum_kind_for(file: &Path) -> Result<ChecksumKind> {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ffp") => Ok(ChecksumKind::Ffp),
        Some("md5") => Ok(ChecksumKind::Md5),
        Some("st5") => Ok(ChecksumKind::St5),
        other => anyhow::bail!(
            "cannot tell what kind of checksum file this is from {:?}; expected .ffp, .md5 or .st5",
            other.unwrap_or_default()
        ),
    }
}

fn cmd_check(file: &Path) -> Result<bool> {
    let kind = checksum_kind_for(file)?;

    let list =
        ChecksumFile::read(kind, file).with_context(|| format!("reading {}", file.display()))?;
    let dir = file.parent().unwrap_or(Path::new("."));

    let mut ok = true;
    for entry in &list.entries {
        let target = dir.join(&entry.file_name);
        if !target.exists() {
            ok = false;
            println!("MISSING   {}", entry.file_name);
            continue;
        }
        match compute(kind, &target) {
            Ok(actual) if actual == entry.digest => println!("OK        {}", entry.file_name),
            Ok(actual) => {
                ok = false;
                println!(
                    "MISMATCH  {}\n            expected {}\n            actual   {}",
                    entry.file_name,
                    hex::encode(entry.digest),
                    hex::encode(actual)
                );
            }
            Err(e) => {
                ok = false;
                println!("FAILED    {}: {e}", entry.file_name);
            }
        }
    }
    println!("{} {} entries checked", list.entries.len(), kind.label());
    Ok(ok)
}

/// Write the etree Vorbis-comment tags for one show (docs/tagging.md §1, §4, §5).
///
/// `TRACKNUMBER` always comes from position in the scan, never typed. Every other field
/// is show-level — one value applied to every taggable file — except `TITLE`, which comes
/// from `--titles` in file order when given. The full diff is always printed; nothing is
/// written unless `--yes` is given, and a write is followed immediately by the
/// audio-MD5 recheck docs/tagging.md §1's contract requires — a mismatch aborts the rest
/// of the run rather than being treated as one file's failure, because it means a bug in
/// us, not in the input.
fn cmd_tag(args: &TagArgs) -> Result<bool> {
    if !args.dir.is_dir() {
        anyhow::bail!("{} is not a directory", args.dir.display());
    }
    let set =
        scan::scan(&args.dir, false).with_context(|| format!("scanning {}", args.dir.display()))?;
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        anyhow::bail!("no audio files found in {}", args.dir.display());
    }

    let titles = match &args.titles {
        Some(path) if path == Path::new("-") => Some(read_titles(
            std::io::read_to_string(std::io::stdin()).context("reading titles from stdin")?,
        )),
        Some(path) => Some(read_titles(
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        )),
        None => None,
    };
    if let Some(titles) = &titles {
        if titles.len() != set.files.len() {
            anyhow::bail!(
                "{} titles given but {} files in {}",
                titles.len(),
                set.files.len(),
                args.dir.display()
            );
        }
    }

    let show_edit = Tags {
        artist: args.artist.clone(),
        album: args.album.clone(),
        date: args.date.clone(),
        genre: args.genre.clone(),
        comment: args.comment.clone(),
        location: args.location.clone(),
        title: None,
        track_number: None,
    };

    let mut any_change = false;
    let mut wrote = 0usize;
    for (i, f) in set.files.iter().enumerate() {
        if !tag::is_taggable(f.format) {
            println!(
                "N/A       {} ({} carries no Vorbis comments)",
                f.file_name(),
                f.format
            );
            continue;
        }
        let mut edit = show_edit.clone();
        edit.track_number = Some((i + 1).to_string());
        if let Some(titles) = &titles {
            edit.title = Some(titles[i].clone());
        }

        let before = tag::read(&f.path)
            .with_context(|| format!("reading tags from {}", f.path.display()))?;
        let changes = before.changes(&edit);
        if changes.is_empty() {
            println!("{}   unchanged", f.file_name());
            continue;
        }
        any_change = true;
        println!("{}", f.file_name());
        for (field, old, new) in &changes {
            println!("  {:<12} {:?} -> {:?}", field.key(), old.unwrap_or(""), new);
        }

        if args.yes {
            let audio_before = ffp(&f.path)
                .with_context(|| format!("reading the audio MD5 of {}", f.path.display()))?;
            tag::apply(&f.path, &edit)
                .with_context(|| format!("writing tags to {}", f.path.display()))?;
            tag::assert_audio_unchanged(&f.path, audio_before)
                .with_context(|| format!("checking {} after the write", f.path.display()))?;
            wrote += 1;
        }
    }

    if !any_change {
        println!("nothing to change");
        return Ok(true);
    }
    if !args.yes {
        println!("plan only, nothing written — pass --yes to write");
        return Ok(true);
    }
    println!("wrote {wrote} files");
    Ok(true)
}

fn read_titles(text: String) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// Rename one show's files to the etree track-name standard (docs/tagging.md §4, §5).
///
/// `--band`/`--date` default from `ShowName::parse` of the folder's own name, and are a
/// command failure — naming exactly what is missing — when neither the flag nor the
/// folder name supplies them. The full diff is always printed; nothing is written unless
/// `--yes`, and a plan holding any collision is refused outright, before anything is
/// written, even for the files that would have been fine.
fn cmd_rename(args: &RenameArgs) -> Result<bool> {
    if !args.dir.is_dir() {
        anyhow::bail!("{} is not a directory", args.dir.display());
    }
    let show_name = args
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ShowName::parse);

    let band = args
        .band
        .clone()
        .or_else(|| show_name.as_ref().map(|s| s.band.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no --band given, and {} is not an etree show folder name to read one from",
                args.dir.display()
            )
        })?;
    let date = match &args.date {
        Some(s) => ShowDate::parse(s)
            .map(|(d, _)| d)
            .ok_or_else(|| anyhow::anyhow!("--date {s:?} is not YYYY-MM-DD or YY-MM-DD"))?,
        None => show_name.as_ref().map(|s| s.date).ok_or_else(|| {
            anyhow::anyhow!(
                "no --date given, and {} is not an etree show folder name to read one from",
                args.dir.display()
            )
        })?,
    };

    let set =
        scan::scan(&args.dir, false).with_context(|| format!("scanning {}", args.dir.display()))?;
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        anyhow::bail!("no audio files found in {}", args.dir.display());
    }

    let spec = NameSpec {
        band,
        date,
        short_year: args.short_year,
        disc: args.disc,
        // No `--keep-suffix` flag (docs/tagging.md §5): keeping a title suffix a file
        // already carries costs nothing when there is none, so it is always on here.
        keep_suffix: true,
    };
    let files: Vec<PathBuf> = set.files.iter().map(|f| f.path.clone()).collect();
    let plan = plan_rename(&files, &spec);
    print_rename_plan(&plan);

    if plan.has_collisions() {
        println!("refusing: more than one file would end up with the same name");
        return Ok(false);
    }
    let changed = plan
        .entries
        .iter()
        .filter(|e| e.status == RenameStatus::Changed)
        .count();
    if changed == 0 {
        println!("nothing to rename");
        return Ok(true);
    }
    if !args.yes {
        println!("plan only, nothing written — pass --yes to rename");
        return Ok(true);
    }

    execute_rename(&plan).with_context(|| format!("renaming {}", args.dir.display()))?;
    println!("renamed {changed} of {} files", plan.entries.len());
    Ok(true)
}

fn print_rename_plan(plan: &RenamePlan) {
    for e in &plan.entries {
        let from = e.from.file_name().unwrap_or_default().to_string_lossy();
        let to = e.to.file_name().unwrap_or_default().to_string_lossy();
        match e.status {
            RenameStatus::Unchanged => println!("{from}   unchanged"),
            RenameStatus::Changed => println!("{from} -> {to}"),
            RenameStatus::Collision => println!("{from} -> {to}   COLLISION"),
        }
    }
}

/// `m:ss.mmm`, the layout shntool uses — sub-second precision matters when the question
/// is whether a track sits on a sector boundary.
fn cmd_torrent_info(file: &Path, list_files: bool) -> Result<bool> {
    let t = Metainfo::read(file).with_context(|| format!("reading {}", file.display()))?;

    println!("{}", t.name);
    println!("  infohash     {}", t.info_hash_hex());
    println!(
        "  pieces       {} x {}",
        t.pieces.len(),
        format_bytes(t.piece_length)
    );
    println!(
        "  total        {} ({} bytes)",
        format_bytes(t.total_length),
        t.total_length
    );
    let real = t.real_files().count();
    let pad = t.files.len() - real;
    if pad > 0 {
        println!("  files        {real} ({pad} padding)");
    } else {
        println!("  files        {real}");
    }
    // A private torrent cannot be reseeded anywhere else, and the flag is inside the
    // infohash, so a trader is entitled to see it before they plan around this file.
    if t.private {
        println!("  private      yes (BEP 27; part of the infohash)");
    }
    if let Some(v) = &t.source {
        println!("  source       {v}");
    }
    if let Some(v) = &t.created_by {
        println!("  created by   {v}");
    }
    if let Some(ts) = t.creation_date {
        println!("  created      {}", format_date(ts));
    }
    if let Some(v) = &t.comment {
        // Trackers write multi-line comments; keep the column alignment intact.
        for (i, line) in v.lines().enumerate() {
            println!("  {:<12} {line}", if i == 0 { "comment" } else { "" });
        }
    }
    for (i, tracker) in t.trackers().enumerate() {
        println!("  {:<12} {tracker}", if i == 0 { "trackers" } else { "" });
    }

    if list_files {
        println!();
        for f in t.real_files() {
            println!("  {:>12}  {}", format_bytes(f.length), f.display_path());
        }
    }
    Ok(true)
}

fn cmd_torrent_create(args: &TorrentCreateArgs) -> Result<bool> {
    let source = args
        .path
        .canonicalize()
        .with_context(|| format!("reading {}", args.path.display()))?;

    // Ids become URLs here, and each --tracker becomes its own tier: clients pick at random
    // within a tier and fall through between them, so putting unrelated sites in one tier
    // is a coin flip over who hears about the seed.
    let list = TrackerList::load().context("reading the tracker list")?;
    let keys = Passkeys::load().context("reading the passkey list")?;
    let chosen = resolve(&args.trackers, &list, &keys)?;
    for warning in &chosen.warnings {
        eprintln!("warning: {warning}");
    }

    // The flags win over the table: a tracker entry can only ever add `private` or a
    // `source`, never take one away that the user asked for.
    let private = args.private || chosen.private;
    let source_tag = args.source.clone().or_else(|| chosen.source.clone());

    let opts = CreateOpts {
        announce: chosen.tiers.clone(),
        piece_length: args.piece_length,
        private,
        source: source_tag.clone(),
        comment: args.comment.clone(),
        include_all: args.include_all,
        overwrite: args.force,
        ..CreateOpts::default()
    };

    let dst = match &args.output {
        Some(o) => o.clone(),
        None => default_output(&source).ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no parent directory to write a torrent beside",
                source.display()
            )
        })?,
    };
    // Writing the .torrent inside the folder it describes adds a file to that folder, so
    // re-creating it later would produce a different infohash.
    if source.is_dir() && dst.parent().is_some_and(|p| p.starts_with(&source)) {
        eprintln!(
            "warning: writing the torrent inside {} means re-creating it later will not \
             produce the same infohash",
            source.display()
        );
    }

    // One job on a queue of one, so a large show's piece-hashing walk shows progress and
    // Ctrl-C can stop it between pieces (docs/job-queue.md §3) instead of only between
    // files, which is all a batch of independent files has to offer.
    let queue: Queue<lh_core::Result<Created>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();
    let _ = ctrlc::set_handler(move || cancel.cancel());

    let job_source = source.clone();
    let job_dst = dst.clone();
    let job_opts = opts.clone();
    queue.submit("torrent create", move |progress| {
        create_with_progress(&job_source, &job_dst, &job_opts, &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        })
    });

    let outcome = loop {
        match queue.events().recv().expect("queue closed unexpectedly") {
            Event::Progress { done, total, .. } => {
                eprint!("\r  hashing piece {done} of {total}");
            }
            Event::Finished { output, .. } => break Some(output),
            Event::Cancelled { .. } => break None,
            Event::Started { .. } => {}
        }
    };
    eprintln!();
    let made = match outcome {
        Some(result) => {
            result.with_context(|| format!("creating a torrent for {}", source.display()))?
        }
        None => {
            println!("cancelled before writing a torrent");
            return Ok(false);
        }
    };

    println!("{}", made.name);
    println!(
        "  {} files   {}   {} {} of {}",
        made.files.len(),
        format_bytes(made.total_length),
        made.pieces,
        if made.pieces == 1 { "piece" } else { "pieces" },
        format_bytes(made.piece_length),
    );
    for (path, why) in &made.excluded {
        let shown = path.strip_prefix(&source).unwrap_or(path);
        println!("  excluded   {} ({})", shown.display(), why.reason());
    }
    println!("  infohash   {}", made.info_hash_hex());
    // The flag is inside the info dictionary, so a table we ship silently decided part of
    // this torrent's identity. Say which entry did it.
    if private {
        let by = if args.private {
            "--private".to_string()
        } else {
            let names: Vec<&str> = chosen
                .chosen
                .iter()
                .filter(|c| c.tracker.as_ref().is_some_and(|t| t.private))
                .map(Chosen::name)
                .collect();
            names.join(", ")
        };
        println!("  private    yes (BEP 27; part of the infohash — set by {by})");
    }
    if let Some(tag) = &source_tag {
        println!("  source     {tag} (part of the infohash)");
    }
    for (i, c) in chosen.chosen.iter().enumerate() {
        let label = if i == 0 { "tracker" } else { "" };
        match &c.tracker {
            Some(t) => println!(
                "  {label:<10} {}  {}  ({})",
                t.name,
                c.announce,
                confirmation(t)
            ),
            None => println!(
                "  {label:<10} {}  (given as a URL, used verbatim)",
                c.announce
            ),
        }
    }
    if chosen.chosen.is_empty() {
        println!("  tracker    none (a trackerless torrent)");
    }
    println!("  wrote      {}", made.path.display());
    Ok(true)
}

/// What we know about an entry, in one parenthesis. Never just a status word: a status
/// with no date is the thing that let TLH recommend a dead tracker for years.
fn confirmation(t: &Tracker) -> String {
    match &t.checked {
        Some(date) => format!("{} — checked {date}", t.health.label()),
        None => format!("{} — from your own list", t.health.label()),
    }
}

/// The list, with what we saw and when. Everything a user needs to decide whether to
/// believe us or go and look — in two lines per entry, not four: only the trackers that
/// answered when checked are in here at all, so there is no dead weight left to explain.
fn cmd_torrent_trackers() -> Result<bool> {
    let list = TrackerList::load().context("reading the tracker list")?;
    let keys = Passkeys::load().context("reading the passkey list")?;

    for t in list.all().filter(|t| t.health.responds()) {
        let origin = match t.origin {
            Origin::Bundled => String::new(),
            Origin::User => "  (yours)".to_string(),
            Origin::Overridden => "  (replaced by your own list)".to_string(),
        };
        println!("{:<14}{}{origin}  —  {}", t.id, t.name, confirmation(t));

        let mut second = t.announce.clone();
        if let Some(saw) = &t.evidence {
            second.push_str(" — ");
            second.push_str(saw);
        }
        println!("{:<14}{second}", "");

        let mut flags = Vec::new();
        if t.private {
            flags.push("sets private: 1 (changes the infohash)".to_string());
        }
        if let Some(tag) = &t.source {
            flags.push(format!("sets info.source {tag:?} (changes the infohash)"));
        }
        if t.needs_passkey() {
            flags.push(match keys.get(&t.id) {
                Some(_) => "passkey configured".to_string(),
                None => "needs a passkey, and none is configured".to_string(),
            });
        }
        if !flags.is_empty() {
            println!("{:<14}{}", "", flags.join("; "));
        }
    }

    let usable = list
        .iter()
        .filter(|t| t.health.responds() && t.health.usable())
        .count();
    let total = list.iter().filter(|t| t.health.responds()).count();
    println!("{usable} of {total} entries can be used as they stand.");
    match &list.user_list {
        Some(path) if path.exists() => println!("your own list: {}", path.display()),
        Some(path) => {
            println!("no list of your own yet; put one at {}", path.display());
            println!(
                "  one `Display Name|announce URL` per line — the format Trader's Little \
                 Helper used, so an existing tracker.lst can be copied straight in"
            );
        }
        None => {}
    }
    Ok(true)
}

fn cmd_torrent_check(file: &Path, path: &Path, quick: bool) -> Result<bool> {
    let meta = Metainfo::read(file).with_context(|| format!("reading {}", file.display()))?;
    let report = if quick {
        check_sizes(&meta, file, path)
    } else {
        check(&meta, file, path)
    }
    .with_context(|| format!("checking against {}", path.display()))?;

    println!("{}", report.name);
    println!(
        "  {}  {} files  {}",
        hex::encode(report.info_hash),
        meta.real_files().count(),
        format_bytes(meta.total_length)
    );
    println!("  root {}", report.root.display());
    println!();

    for outcome in &report.files {
        if outcome.status == FileStatus::Padding {
            continue;
        }
        let name = meta.files[outcome.index].display_path();
        let label = outcome.status.label();
        match &outcome.status {
            FileStatus::WrongSize { expected, actual } => {
                println!("{label:<11} {name}  (expected {expected} bytes, found {actual})")
            }
            FileStatus::Unreadable { reason } => println!("{label:<11} {name}  ({reason})"),
            FileStatus::Corrupt { bad_pieces } => {
                println!("{label:<11} {name}  ({})", pieces_phrase(bad_pieces))
            }
            FileStatus::Suspect { piece, shared_with } => {
                let others: Vec<String> = shared_with
                    .iter()
                    .map(|i| meta.files[*i].display_path())
                    .collect();
                println!(
                    "{label:<11} {name}  (piece {piece} is shared with {}; either could be at fault)",
                    others.join(", ")
                );
            }
            FileStatus::Partial {
                verified,
                unverifiable,
            } => println!(
                "{label:<11} {name}  ({verified} pieces verified, {unverifiable} unreadable \
                 because a neighbouring file is bad)"
            ),
            _ => println!("{label:<11} {name}"),
        }
    }
    for extra in &report.extra_local {
        let shown = extra.strip_prefix(&report.root).unwrap_or(extra);
        println!("{:<11} {}", "EXTRA", shown.display());
    }

    println!();
    let total = meta.real_files().count();
    if let Some(p) = report.pieces {
        print!("{} of {} pieces verified", p.ok, p.total);
        if p.failed > 0 {
            print!(", {} failed", p.failed);
        }
        if p.unverifiable > 0 {
            print!(", {} unverifiable", p.unverifiable);
        }
        println!();
    }
    match report.verdict() {
        Verdict::Incomplete => {
            let n = report.needs_attention().count();
            let plural = if n == 1 { "file needs" } else { "files need" };
            println!("{n} of {total} {plural} attention");
        }
        Verdict::SizesMatch => println!("all {total} files match by size (contents not read)"),
        Verdict::Complete => println!("all {total} files verified"),
    }
    Ok(report.verdict() != Verdict::Incomplete)
}

/// The one command that produces files people will trade, so it says exactly what
/// produced each one. Sources are never modified and never deleted (Principle 1).
fn cmd_convert(args: &ConvertArgs) -> Result<bool> {
    let (files, mut ok) = collect(&args.paths)?;

    // Discovered once, before any work: if the encoder is missing, say so now rather
    // than after converting half a show.
    let encoder = match args.to {
        Target::Flac => Some(
            Registry::discover_one(ToolId::Flac)
                .require(ToolId::Flac)
                .cloned()?,
        ),
        Target::Wav => None,
    };
    let opts = EncodeOpts {
        compression_level: args.level,
        ..EncodeOpts::default()
    };

    let (want, extension) = match args.to {
        Target::Wav => (AudioFormat::Wav, "wav"),
        Target::Flac => (AudioFormat::Flac, "flac"),
    };

    let to = args.to;
    let force = args.force;
    let out_dir = args.out_dir.clone();
    let results = run_batch(&files, move |f, progress| -> ConvertOutcome {
        if f.format == want {
            return ConvertOutcome::Skipped;
        }
        let dst = match destination(&f.path, extension, out_dir.as_deref()) {
            Some(d) => d,
            None => return ConvertOutcome::NoFileName,
        };
        let result = match to {
            Target::Wav => to_wav_with_progress(&f.path, &dst, force, &mut |done, total| {
                progress.report(done, total);
                !progress.is_cancelled()
            }),
            Target::Flac => to_flac_cancellable(
                &f.path,
                &dst,
                encoder.as_ref().expect("discovered above"),
                &opts,
                force,
                &mut || !progress.is_cancelled(),
            ),
        };
        match result {
            Ok(done) => ConvertOutcome::Done(Box::new(done)),
            Err(e) => ConvertOutcome::Failed(e),
        }
    });

    let mut written = 0usize;
    for (f, outcome) in &results {
        match outcome {
            Some(ConvertOutcome::Skipped) => {
                println!("SKIPPED   {} (already {want})", f.file_name())
            }
            Some(ConvertOutcome::NoFileName) => {
                ok = false;
                println!(
                    "FAILED    {} (has no file name to work from)",
                    f.path.display()
                );
            }
            Some(ConvertOutcome::Done(done)) => {
                written += 1;
                report_conversion(done, args.provenance);
            }
            Some(ConvertOutcome::Failed(e)) => {
                ok = false;
                println!("FAILED    {}: {e}", f.file_name());
            }
            None => {
                ok = false;
                println!("CANCELLED {}", f.file_name());
            }
        }
    }

    println!("{written} of {} files converted", files.len());
    Ok(ok)
}

/// What became of one file's conversion. `Conversion` is boxed only to keep this enum
/// small relative to its rarest, biggest variant — it is moved through the job queue's
/// channel once per file.
enum ConvertOutcome {
    Skipped,
    NoFileName,
    Done(Box<Conversion>),
    Failed(lh_core::Error),
}

fn report_conversion(done: &Conversion, show_provenance: bool) {
    let name = done
        .output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| done.output.display().to_string());
    if done.checked_against_source {
        println!("WROTE     {name}");
    } else {
        // A weaker result than the usual one, and it says so rather than looking the same.
        println!("WROTE     {name}  (unchecked: nothing in the source to compare against)");
    }
    if show_provenance {
        for line in done.provenance.render().lines() {
            println!("          {line}");
        }
    }
}

/// The Tools panel, headless. Every operation that shells out logs the same facts this
/// prints, so a trader can tell what produced a file before they trust it (Principle 2).
fn cmd_tools() -> Result<bool> {
    let registry = Registry::discover();
    for (id, discovery) in registry.entries() {
        match discovery {
            Discovery::Found(t) => {
                println!(
                    "{:<10} {}  ({})",
                    id.name(),
                    t.path.display(),
                    t.source.label()
                );
                println!("{:<10} {}", "", t.version);
                println!("{:<10} sha256 {}", "", t.sha256);
            }
            Discovery::Unusable { path, reason } => {
                println!("{:<10} {} is unusable", id.name(), path.display());
                println!("{:<10} {reason}", "");
            }
            Discovery::NotFound { searched } => {
                let need = if id.is_required() {
                    "needed for"
                } else {
                    "only needed for"
                };
                println!("{:<10} not found — {need} {}", id.name(), id.purpose());
                println!("{:<10} looked in {}", "", searched.join(", "));
                println!(
                    "{:<10} point at your own with {}=/path/to/{}",
                    "",
                    id.env_var(),
                    id.name()
                );
            }
        }
    }

    let missing: Vec<ToolId> = registry.missing_required().collect();
    if missing.is_empty() {
        Ok(true)
    } else {
        println!();
        for id in missing {
            println!(
                "{} is required and was not found; {} will not run",
                id,
                id.purpose()
            );
        }
        Ok(false)
    }
}

fn pieces_phrase(pieces: &[u32]) -> String {
    if pieces.len() == 1 {
        format!("piece {}", pieces[0])
    } else {
        let list: Vec<String> = pieces.iter().map(u32::to_string).collect();
        format!("pieces {}", list.join(", "))
    }
}

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Torrent creation dates matter for identifying an old seed, so show a date rather than
/// an epoch. Civil-from-days, so this needs no date library.
fn format_date(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn format_duration(secs: f64) -> String {
    let millis = (secs * 1000.0).round() as u64;
    let (m, rem) = (millis / 60_000, millis % 60_000);
    format!("{m}:{s:02}.{ms:03}", s = rem / 1000, ms = rem % 1000)
}
