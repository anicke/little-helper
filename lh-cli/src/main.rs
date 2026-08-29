//! `lh` — the headless half of Little Helper.
//!
//! Exit codes: 0 everything passed, 1 at least one file failed, 2 the command itself failed.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lh_core::analysis::{Sbe, Verification, sbe, verify};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute};
use lh_core::model::AudioFile;
use lh_core::torrent::Metainfo;
use lh_core::{format, scan};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "lh",
    version,
    about = "Little Helper — lossless audio for traders"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what each file is, including the encoder that produced it.
    Info(Paths),
    /// Decode each file and check it against the MD5 it carries.
    Verify(Paths),
    /// Report sector boundary errors.
    Sbe(Paths),
    /// Write or print FFP checksums (audio MD5 from the FLAC header).
    Ffp(ChecksumArgs),
    /// Write or print MD5 checksums of the file bytes.
    Md5(ChecksumArgs),
    /// Write or print ST5 checksums (audio data only).
    St5(ChecksumArgs),
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
}

#[derive(Subcommand)]
enum TorrentCommand {
    /// Show what a .torrent contains: infohash, trackers, pieces and file list.
    Info {
        /// The .torrent file.
        file: PathBuf,
        /// Suppress the file listing.
        #[arg(long)]
        no_files: bool,
    },
}

#[derive(clap::Args)]
struct Paths {
    /// Files or folders. Defaults to the current directory.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
    /// Descend into subdirectories.
    #[arg(short, long)]
    recursive: bool,
}

#[derive(clap::Args)]
struct ChecksumArgs {
    #[command(flatten)]
    paths: Paths,
    /// Write to this file instead of printing to stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lh: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether every file passed.
fn run(cli: Cli) -> Result<bool> {
    match cli.command {
        Command::Info(p) => cmd_info(&p),
        Command::Verify(p) => cmd_verify(&p),
        Command::Sbe(p) => cmd_sbe(&p),
        Command::Ffp(a) => cmd_checksum(ChecksumKind::Ffp, &a),
        Command::Md5(a) => cmd_checksum(ChecksumKind::Md5, &a),
        Command::St5(a) => cmd_checksum(ChecksumKind::St5, &a),
        Command::Check { file } => cmd_check(&file),
        Command::Torrent { command } => match command {
            TorrentCommand::Info { file, no_files } => cmd_torrent_info(&file, !no_files),
        },
    }
}

/// Expand files and folders into a flat list of audio files, reporting anything skipped
/// rather than dropping it silently.
fn collect(p: &Paths) -> Result<(Vec<AudioFile>, bool)> {
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
    for f in &files {
        match verify(&f.path) {
            Ok(Verification::Ok) => println!("OK        {}", f.file_name()),
            Ok(Verification::Md5Mismatch { stored, computed }) => {
                ok = false;
                println!(
                    "MISMATCH  {}\n            stored   {}\n            computed {}",
                    f.file_name(),
                    hex::encode(stored),
                    hex::encode(computed)
                );
            }
            Ok(Verification::NoStoredMd5 { .. }) => {
                println!(
                    "NO MD5    {} (decoded cleanly, nothing to compare)",
                    f.file_name()
                )
            }
            Err(e) => {
                ok = false;
                println!("FAILED    {}: {e}", f.file_name());
            }
        }
    }
    Ok(ok)
}

fn cmd_sbe(p: &Paths) -> Result<bool> {
    let (files, mut ok) = collect(p)?;
    for f in &files {
        match sbe(&f.stream_info) {
            Sbe::Aligned => println!("ALIGNED   {}", f.file_name()),
            Sbe::Misaligned { remainder_frames } => {
                ok = false;
                println!("SBE       {} (+{remainder_frames} frames)", f.file_name());
            }
            Sbe::NotApplicable { reason } => println!("N/A       {} ({reason})", f.file_name()),
        }
    }
    Ok(ok)
}

fn cmd_checksum(kind: ChecksumKind, args: &ChecksumArgs) -> Result<bool> {
    let (files, mut ok) = collect(&args.paths)?;
    let mut out = ChecksumFile::new(kind);
    for f in &files {
        match compute(kind, &f.path) {
            Ok(digest) => out.entries.push(Entry {
                file_name: f.file_name(),
                digest,
            }),
            Err(e) => {
                ok = false;
                eprintln!("{}: {e}", f.file_name());
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

fn cmd_check(file: &Path) -> Result<bool> {
    let kind = match file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("ffp") => ChecksumKind::Ffp,
        Some("md5") => ChecksumKind::Md5,
        Some("st5") => ChecksumKind::St5,
        other => anyhow::bail!(
            "cannot tell what kind of checksum file this is from {:?}; expected .ffp, .md5 or .st5",
            other.unwrap_or_default()
        ),
    };

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
    for (i, tracker) in t.announce.iter().enumerate() {
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
