use crate::*;
use anyhow::Result;
use lh_core::convert::{Conversion, EncodeOpts, ORIGINALS_DIR, destination, to_flac, to_wav};
use lh_core::model::AudioFormat;
use lh_core::tools::{Registry, ToolId};
use std::path::PathBuf;

/// The one command that produces files people will trade, so it says exactly what
/// produced each one. Sources are never modified and never deleted (Principle 1) — with
/// `--move-sources` a checked one is only moved into `_original/`.
pub(crate) fn cmd_convert(args: &ConvertArgs) -> Result<bool> {
    if args.move_sources && args.to != Target::Flac {
        anyhow::bail!("--move-sources only applies to --to flac");
    }
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
    let move_sources = args.move_sources;
    let results = run_batch(&files, move |f, progress| -> ConvertOutcome {
        if f.format == want {
            return ConvertOutcome::Skipped;
        }
        let dst = match destination(&f.path, extension, out_dir.as_deref()) {
            Ok(d) => d,
            Err(_) => return ConvertOutcome::NoFileName,
        };
        let on_progress = &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        };
        let result = match to {
            Target::Wav => to_wav(&f.path, &dst, force, on_progress),
            Target::Flac => to_flac(
                &f.path,
                &dst,
                encoder.as_ref().expect("discovered above"),
                &opts,
                force,
                on_progress,
            ),
        };
        match result {
            Ok(done) => {
                let moved = move_sources.then(|| done.move_source_to_originals());
                ConvertOutcome::Done(Box::new(done), moved)
            }
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
            Some(ConvertOutcome::Done(done, moved)) => {
                written += 1;
                report_conversion(done, args.provenance);
                match moved {
                    Some(Ok(_)) => println!("MOVED     {} -> {ORIGINALS_DIR}/", f.file_name()),
                    // The FLAC stands; the folder just still holds its source, which is
                    // what a clean run promised it wouldn't.
                    Some(Err(e)) => {
                        ok = false;
                        println!("KEPT      {}: {e}", f.file_name());
                    }
                    None => {}
                }
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

/// What became of one file's conversion, and of its source when `--move-sources` asked for
/// it to be set aside. `Conversion` is boxed only to keep this enum small relative to its
/// rarest, biggest variant — it is moved through the job queue's channel once per file.
enum ConvertOutcome {
    Skipped,
    NoFileName,
    Done(Box<Conversion>, Option<lh_core::Result<PathBuf>>),
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
