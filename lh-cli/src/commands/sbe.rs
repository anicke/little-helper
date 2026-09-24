use crate::*;
use anyhow::{Context, Result};
use lh_core::analysis::{Sbe, sbe};
use lh_core::convert::{EncodeOpts, destination};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::repair::{
    FixPlan, FixStep, Fixed, InPlace, RepairEncode, TailPolicy, execute_fix, fix_in_place,
    plan_fix, set_format,
};
use lh_core::scan;
use lh_core::tools::{Registry, ToolId};
use std::path::Path;

pub(crate) fn cmd_sbe(p: &Paths) -> Result<bool> {
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
/// padded with silence when `--pad-tail` is given (docs/sbe-repair.md R1–R3), on
/// a set of FLACs or, with no encode at all, of WAVs (R5).
pub(crate) fn cmd_sbe_fix(args: &SbeFixArgs) -> Result<bool> {
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

    let plan = plan_fix(
        &set.files,
        args.direction.into(),
        tail_policy(args.pad_tail),
    )
    .with_context(|| format!("planning a fix for {}", args.dir.display()))?;

    if args.dry_run {
        print_fix_plan(&args.dir, &set.files, &plan, args.pad_tail);
        return Ok(plan.fully_fixed);
    }

    if args.output.is_none() && !args.in_place {
        anyhow::bail!(
            "sbe fix needs -o/--output or --in-place to execute — it never writes over the \
             originals (Principle 1)"
        );
    }
    // A set of WAVs is fixed without encoding anything, so only FLAC needs `flac`.
    let format = set_format(&set.files)?;
    let flac = match format {
        AudioFormat::Flac => Some(
            Registry::discover_one(ToolId::Flac)
                .require(ToolId::Flac)
                .cloned()?,
        ),
        _ => None,
    };
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: flac.as_ref(),
        opts: &opts,
        overwrite: args.overwrite,
    };

    // Minutes of work on a real show; one line on stderr, rewritten as each file moves on.
    let on_step = |i: usize, step: FixStep| {
        eprint!("\r  {:<8}  {:<60}", step.label(), set.files[i].file_name());
    };

    if args.in_place {
        let done = fix_in_place(&set.files, &plan, &encode, &on_step);
        eprintln!();
        let done = done.with_context(|| format!("repairing {}", args.dir.display()))?;
        print_in_place(&set.files, &done);
        print_tail_note(&set.files, &plan);
        return Ok(plan.fully_fixed);
    }

    let out_dir = args.output.as_deref().expect("checked above");
    let dsts = set
        .files
        .iter()
        .map(|f| {
            destination(&f.path, repair_extension(format), Some(out_dir))
                .map_err(|_| anyhow::anyhow!("{} has no file name", f.path.display()))
        })
        .collect::<Result<Vec<_>>>()?;

    let fixed = execute_fix(&set.files, &plan, &dsts, &encode, &on_step);
    eprintln!();
    let fixed = fixed.with_context(|| format!("repairing {}", args.dir.display()))?;

    print_fixed(&set.files, &fixed);
    print_tail_note(&set.files, &plan);

    Ok(plan.fully_fixed)
}

/// The extension a fixed file is written with: its set's own format's.
pub fn repair_extension(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Wav => "wav",
        _ => "flac",
    }
}

/// `--pad-tail`'s choice, for anything that holds it as a flag.
pub fn tail_policy(pad_tail: bool) -> TailPolicy {
    if pad_tail {
        TailPolicy::Pad
    } else {
        TailPolicy::Report
    }
}

/// One line per file [`execute_fix`] wrote under `-o`.
pub fn print_fixed(files: &[AudioFile], fixed: &[Fixed]) {
    for (f, fixed) in files.iter().zip(fixed) {
        println!(
            "FIXED     {} -> {}   audio md5 {}",
            f.file_name(),
            fixed.path.display(),
            hex::encode(fixed.audio_md5)
        );
    }
}

/// One line per file of an `--in-place` fix, in the order [`fix_in_place`] returns them.
pub fn print_in_place(files: &[AudioFile], done: &[InPlace]) {
    for (f, d) in files.iter().zip(done) {
        match d {
            InPlace::Replaced { fixed, original } => println!(
                "FIXED     {}   original moved to {}   audio md5 {}",
                f.file_name(),
                original.display(),
                hex::encode(fixed.audio_md5)
            ),
            InPlace::Unchanged { .. } => println!("UNCHANGED {}", f.file_name()),
        }
    }
}

pub fn print_tail_note(files: &[AudioFile], plan: &FixPlan) {
    if !plan.fully_fixed {
        println!(
            "{}   still misaligned — rerun with --pad-tail to close it with silence",
            files.last().expect("checked non-empty above").file_name()
        );
    }
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
