use crate::*;
use anyhow::{Context, Result};
use lh_core::etree::{ShowDate, ShowName};
use lh_core::rename::{NameSpec, RenamePlan, RenameStatus, execute_rename, plan_rename};
use lh_core::scan;
use std::path::PathBuf;

/// Rename one show's files to the etree track-name standard (docs/tagging.md §4, §5).
///
/// `--band`/`--date` default from `ShowName::parse` of the folder's own name, and are a
/// command failure — naming exactly what is missing — when neither the flag nor the
/// folder name supplies them. The full diff is always printed; nothing is written unless
/// `--yes`, and a plan holding any collision is refused outright, before anything is
/// written, even for the files that would have been fine.
pub(crate) fn cmd_rename(args: &RenameArgs) -> Result<bool> {
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
