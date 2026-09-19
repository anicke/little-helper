use crate::*;
use anyhow::Result;
use lh_core::convert::{Conversion, EncodeOpts, destination, to_flac, to_wav};
use lh_core::model::AudioFormat;
use lh_core::tools::{Registry, ToolId};

/// The one command that produces files people will trade, so it says exactly what
/// produced each one. Sources are never modified and never deleted (Principle 1).
pub(crate) fn cmd_convert(args: &ConvertArgs) -> Result<bool> {
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
