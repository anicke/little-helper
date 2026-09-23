use crate::*;
use anyhow::{Context, Result};
use lh_core::checksum::ffp;
use lh_core::scan;
use lh_core::tag::{self, Tags};
use std::path::Path;

/// Write the etree Vorbis-comment tags for one show (docs/tagging.md §1, §4, §5).
///
/// Only FLAC files are tracks; anything else in the folder (a WAV left beside its converted
/// FLAC) is reported as N/A and left out entirely. `TRACKNUMBER` always comes from position
/// among the FLAC files, never typed. Every other field is show-level — one value applied
/// to every file — except `TITLE`, which comes
/// from `--titles` in file order when given. The full diff is always printed; nothing is
/// written unless `--yes` is given, and a write is followed immediately by the
/// audio-MD5 recheck docs/tagging.md §1's contract requires — a mismatch aborts the rest
/// of the run rather than being treated as one file's failure, because it means a bug in
/// us, not in the input.
pub(crate) fn cmd_tag(args: &TagArgs) -> Result<bool> {
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
    let (files, others): (Vec<_>, Vec<_>) = set
        .files
        .into_iter()
        .partition(|f| tag::is_taggable(f.format));
    for f in &others {
        println!(
            "N/A       {} ({} carries no Vorbis comments)",
            f.file_name(),
            f.format
        );
    }
    if files.is_empty() {
        anyhow::bail!("no FLAC files to tag in {}", args.dir.display());
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
        if titles.len() != files.len() {
            anyhow::bail!(
                "{} titles given but {} FLAC files in {}",
                titles.len(),
                files.len(),
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
    for (i, f) in files.iter().enumerate() {
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
