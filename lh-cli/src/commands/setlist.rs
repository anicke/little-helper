use crate::*;
use anyhow::{Context, Result};
use lh_core::infofile;
use lh_core::scan;

/// Write one show's info `.txt` from its tags (docs/info-file.md).
///
/// Everything the file could not say cleanly is printed first, then the text itself. Nothing
/// is written without `--yes`, and an existing file of the same name is only replaced with
/// `--force` — it is usually hand-written and cannot be regenerated.
pub(crate) fn cmd_setlist(args: &SetlistArgs) -> Result<bool> {
    if !args.dir.is_dir() {
        anyhow::bail!("{} is not a directory", args.dir.display());
    }
    let set =
        scan::scan(&args.dir, false).with_context(|| format!("scanning {}", args.dir.display()))?;
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    let plan = infofile::plan(&args.dir, &set.files)
        .with_context(|| format!("reading tags in {}", args.dir.display()))?;
    if plan.tracks.is_empty() {
        anyhow::bail!("no FLAC files in {}", args.dir.display());
    }

    for problem in &plan.problems {
        println!("note: {problem}");
    }
    if !plan.problems.is_empty() {
        println!();
    }
    print!("{}", infofile::render(&plan));
    println!();

    if plan.exists() && !args.force {
        println!(
            "{} already exists — pass --force to replace it",
            plan.path.display()
        );
        return Ok(!args.yes);
    }
    if !args.yes {
        println!(
            "plan only, nothing written — pass --yes to write {}",
            plan.path.display()
        );
        return Ok(true);
    }
    let path = infofile::write(&plan, args.force)
        .with_context(|| format!("writing {}", plan.path.display()))?;
    println!("wrote {}", path.display());
    Ok(true)
}
