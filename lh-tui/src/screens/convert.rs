use std::io;
use std::process::ExitCode;

use crate::*;
use lh_cli::{ConvertArgs, Target};
use lh_core::convert::{Conversion, EncodeOpts, destination, to_flac, to_wav};
use lh_core::job::Queue;
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::tools::{Registry, Tool, ToolId};
use ratatui::DefaultTerminal;
use ratatui::style::Style;

// --- Convert -------------------------------------------------------------------------
//
// Unlike verify/checksum, this is the one screen where a file's own progress is worth
// showing: `to_wav` reports (frames written, frames total) once per decoded block, so a
// decoding row can show a live percentage rather than just a spinner (`docs/tui.md` §5
// calls this out as "the reason §2 calls out progress rendering as the real per-screen
// variable"). `to_flac` has no such number to relay — `flac` only draws its own percentage
// when stderr is a terminal, which piped through `Command` it never is
// (`lh-core/src/convert/mod.rs`'s own doc comment), calling its progress with `(0, 0)`
// instead — so an encoding row just spins.

#[derive(Clone)]
pub(crate) enum ConvertStatus {
    Pending,
    Running { done: u32, total: u32 },
    Skipped(AudioFormat),
    Done { unchecked: bool, output: String },
    Failed(String),
}

impl RowStatus for ConvertStatus {
    fn pending() -> Self {
        ConvertStatus::Pending
    }

    fn running(done: u32, total: u32) -> Self {
        ConvertStatus::Running { done, total }
    }

    fn cancelled() -> Self {
        ConvertStatus::Failed("cancelled".to_string())
    }

    fn cell(&self, spin: char, theme: &Theme) -> (String, Style) {
        match self {
            ConvertStatus::Pending => ("pending".to_string(), theme.dim),
            ConvertStatus::Running { done, total } if *total > 0 => {
                let pct = (u64::from(*done) * 100 / u64::from(*total)).min(100);
                (format!("{spin} {pct}%"), theme.accent)
            }
            ConvertStatus::Running { .. } => (format!("{spin} running"), theme.accent),
            ConvertStatus::Skipped(_) => ("SKIPPED".to_string(), theme.dim),
            ConvertStatus::Done { .. } => ("OK".to_string(), theme.ok),
            ConvertStatus::Failed(_) => ("FAILED".to_string(), theme.error),
        }
    }

    fn detail(&self) -> String {
        match self {
            ConvertStatus::Pending | ConvertStatus::Running { .. } => String::new(),
            ConvertStatus::Skipped(want) => format!("already {want}"),
            ConvertStatus::Done { unchecked, output } => {
                if *unchecked {
                    format!("-> {output}  (unchecked: nothing to compare against)")
                } else {
                    format!("-> {output}")
                }
            }
            ConvertStatus::Failed(e) => e.clone(),
        }
    }

    fn tally(&self, counts: &mut Counts) {
        match self {
            ConvertStatus::Pending | ConvertStatus::Running { .. } => {}
            ConvertStatus::Skipped(_) => counts.good("skipped"),
            ConvertStatus::Done { .. } => counts.good("written"),
            ConvertStatus::Failed(_) => counts.bad("failed"),
        }
    }
}

/// Mirrors `lh-cli`'s own (private) `ConvertOutcome` (`lh-cli/src/commands/convert.rs`) — small enough
/// that duplicating it here beats exporting an internal type just for this screen, the
/// same call every other screen's own `Status` enum already makes.
pub(crate) enum ConvertOutcome {
    Skipped,
    NoFileName,
    Done(Box<Conversion>),
    Failed(lh_core::Error),
}

pub(crate) fn run_convert(args: ConvertArgs, theme: ThemeName) -> ExitCode {
    let label = describe(&args.paths);
    let (files, mut clean) = match collect(&args.paths) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
    };

    // Discovered once, up front, exactly like `cmd_convert`: if the encoder is missing,
    // say so before converting anything rather than after half a show.
    let encoder = match args.to {
        Target::Flac => match Registry::discover_one(ToolId::Flac).require(ToolId::Flac) {
            Ok(t) => Some(t.clone()),
            Err(e) => {
                eprintln!("lh-tui: {e:#}");
                return ExitCode::from(2);
            }
        },
        Target::Wav => None,
    };

    if files.is_empty() {
        eprintln!("no audio files found under {label}");
        return if clean {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }

    let result = {
        let mut terminal = TerminalGuard::new();
        run_convert_screen(
            &mut terminal,
            &label,
            &files,
            &args,
            encoder,
            Theme::new(theme),
        )
    };

    let (ok, written) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            return ExitCode::from(2);
        }
    };
    clean &= ok;

    // A table cell is nowhere near wide enough for a full provenance render, so
    // `--provenance` prints it after the screen exits instead — same information
    // `cmd_convert`'s own `report_conversion` shows inline, just relocated.
    if args.provenance {
        for done in &written {
            for line in done.provenance.render().lines() {
                println!("{line}");
            }
        }
    }

    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Returns whether every file converted cleanly (a skip counts as clean, same as
/// `cmd_convert`'s own exit code) plus every successful conversion's record, in
/// submission order — used only for the post-loop `--provenance` dump above.
fn run_convert_screen(
    terminal: &mut DefaultTerminal,
    root: &str,
    files: &[AudioFile],
    args: &ConvertArgs,
    encoder: Option<Tool>,
    theme: Theme,
) -> io::Result<(bool, Vec<Conversion>)> {
    let (want, extension) = match args.to {
        Target::Wav => (AudioFormat::Wav, "wav"),
        Target::Flac => (AudioFormat::Flac, "flac"),
    };
    let to = args.to;
    let force = args.force;
    let out_dir = args.out_dir.clone();
    let opts = EncodeOpts {
        compression_level: args.level,
        ..EncodeOpts::default()
    };

    let queue: Queue<ConvertOutcome> = Queue::new();
    for f in files {
        let path = f.path.clone();
        let format = f.format;
        let out_dir = out_dir.clone();
        let opts = opts.clone();
        let encoder = encoder.clone();
        queue.submit(f.file_name(), move |progress| -> ConvertOutcome {
            if format == want {
                return ConvertOutcome::Skipped;
            }
            let dst = match destination(&path, extension, out_dir.as_deref()) {
                Ok(d) => d,
                Err(_) => return ConvertOutcome::NoFileName,
            };
            let on_progress = &mut |done, total| {
                progress.report(done, total);
                !progress.is_cancelled()
            };
            let result = match to {
                Target::Wav => to_wav(&path, &dst, force, on_progress),
                Target::Flac => to_flac(
                    &path,
                    &dst,
                    encoder
                        .as_ref()
                        .expect("discovered before the screen opened"),
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
    }

    let view = View {
        command: format!("convert --to {want}"),
        target: root,
        unit: "files",
        detail: "detail",
        widths: (35, 55),
        labels: &["written", "skipped", "failed"],
    };
    let names: Vec<String> = files.iter().map(AudioFile::file_name).collect();
    let mut conversions: Vec<Option<Conversion>> = (0..files.len()).map(|_| None).collect();
    let outcome = run_batch(
        terminal,
        &view,
        &names,
        queue,
        |index, output| match output {
            ConvertOutcome::Skipped => ConvertStatus::Skipped(want),
            ConvertOutcome::NoFileName => {
                ConvertStatus::Failed("has no file name to work from".to_string())
            }
            ConvertOutcome::Done(c) => {
                let status = ConvertStatus::Done {
                    unchecked: !c.checked_against_source,
                    output: c
                        .output
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| c.output.display().to_string()),
                };
                conversions[index] = Some(*c);
                status
            }
            ConvertOutcome::Failed(e) => ConvertStatus::Failed(e.to_string()),
        },
        &theme,
    )?;

    Ok((outcome.clean, conversions.into_iter().flatten().collect()))
}
