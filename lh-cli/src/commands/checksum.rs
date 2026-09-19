use crate::*;
use anyhow::{Context, Result};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, EntryOutcome, check_entry, compute};
use std::path::Path;

pub(crate) fn cmd_checksum(kind: ChecksumKind, args: &ChecksumArgs) -> Result<bool> {
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

/// `ChecksumKind::from_path`, with `lh check`'s own error wording when a path names none
/// of `.ffp`/`.md5`/`.st5`.
fn checksum_kind_for(file: &Path) -> Result<ChecksumKind> {
    ChecksumKind::from_path(file).ok_or_else(|| {
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        anyhow::anyhow!(
            "cannot tell what kind of checksum file this is from {ext:?}; expected .ffp, .md5 or .st5"
        )
    })
}

pub(crate) fn cmd_check(file: &Path) -> Result<bool> {
    let kind = checksum_kind_for(file)?;

    let list =
        ChecksumFile::read(kind, file).with_context(|| format!("reading {}", file.display()))?;
    let dir = file.parent().unwrap_or(Path::new("."));

    let mut ok = true;
    for entry in &list.entries {
        match check_entry(kind, dir, entry) {
            EntryOutcome::Ok => println!("OK        {}", entry.file_name),
            EntryOutcome::Missing => {
                ok = false;
                println!("MISSING   {}", entry.file_name);
            }
            EntryOutcome::Mismatch { expected, actual } => {
                ok = false;
                println!(
                    "MISMATCH  {}\n            expected {}\n            actual   {}",
                    entry.file_name,
                    hex::encode(expected),
                    hex::encode(actual)
                );
            }
            EntryOutcome::Failed(e) => {
                ok = false;
                println!("FAILED    {}: {e}", entry.file_name);
            }
        }
    }
    println!("{} {} entries checked", list.entries.len(), kind.label());
    Ok(ok)
}
