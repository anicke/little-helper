use crate::*;
use anyhow::{Context, Result};
use lh_core::job::CancelToken;
use lh_core::sample::{self, Clip, Request, format_size, format_time};

/// Cut one MP3 sample from one track (docs/sample.md). An error when it cannot fit the
/// limit rather than a quietly shorter clip: the person picked the length.
pub(crate) fn cmd_sample(args: &SampleArgs) -> Result<bool> {
    if args.path.is_dir() {
        anyhow::bail!(
            "{} is a folder; give one track (lh-tui sample takes a folder and lets you pick)",
            args.path.display()
        );
    }
    let file = lh_core::format::probe(&args.path)?;
    let encoder = sample::find_encoder()?;
    let request = Request {
        clip: clip_for(args, file.stream_info.duration_secs()),
        mode: args.mode,
        max_bytes: Some(args.max_size),
        overwrite: args.force,
    };
    let dst = match &args.output {
        Some(p) => p.clone(),
        None => sample::default_output(&args.path)?,
    };

    let cancel = CancelToken::new();
    let handler = cancel.clone();
    let _ = ctrlc::set_handler(move || handler.cancel());
    let done = sample::encode(&args.path, &dst, &encoder, &request, &mut |_, _| {
        !cancel.is_cancelled()
    })
    .with_context(|| format!("sampling {}", args.path.display()))?;

    println!(
        "wrote {} ({}, {} to {}, {}; limit {})",
        done.output.display(),
        format_size(done.bytes),
        format_time(done.clip.start),
        format_time(done.clip.end()),
        args.mode,
        format_size(args.max_size)
    );
    if args.provenance {
        print!("{}", done.provenance.render());
    }
    Ok(true)
}

/// `--start` and `--length` where given, the default clip where not. A start given alone
/// keeps the default length; a length given alone is centred.
fn clip_for(args: &SampleArgs, track_secs: Option<f64>) -> Clip {
    let default = Clip::default_for(track_secs, args.mode, args.max_size);
    let length = args.length.unwrap_or(default.length);
    Clip {
        start: args
            .start
            .unwrap_or_else(|| Clip::centred(track_secs, length).start),
        length,
    }
}
