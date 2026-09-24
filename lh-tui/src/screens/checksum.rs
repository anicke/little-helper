use std::io;
use std::process::ExitCode;

use crate::*;
use lh_cli::ChecksumArgs;
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute};
use lh_core::job::Queue;
use lh_core::model::AudioFile;
use ratatui::DefaultTerminal;
use ratatui::style::Style;

// --- Checksum (ffp / md5 / st5) ------------------------------------------------------
//
// One screen for all three `ChecksumKind`s, the same way `lh-cli::cmd_checksum` is one
// function parameterized by `kind` rather than three near-duplicates (`docs/tui.md` §3).
// Unlike verify, `checksum::compute` only ever succeeds with a digest or fails outright —
// there is no "no md5 to compare" or "mismatch" outcome — so the digest itself is the
// payload worth showing, not just a status word.

#[derive(Clone)]
pub(crate) enum ChecksumStatus {
    Pending,
    Running,
    Ok([u8; 16]),
    Failed(String),
}

impl RowStatus for ChecksumStatus {
    fn pending() -> Self {
        ChecksumStatus::Pending
    }

    fn running(_done: u32, _total: u32) -> Self {
        ChecksumStatus::Running
    }

    fn cancelled() -> Self {
        ChecksumStatus::Failed("cancelled".to_string())
    }

    fn cell(&self, spin: char, theme: &Theme) -> (String, Style) {
        match self {
            ChecksumStatus::Pending => ("pending".to_string(), theme.dim),
            ChecksumStatus::Running => (format!("{spin} running"), theme.accent),
            ChecksumStatus::Ok(_) => ("OK".to_string(), theme.ok),
            ChecksumStatus::Failed(_) => ("FAILED".to_string(), theme.error),
        }
    }

    /// The digest for a successful row, unlike verify's detail column: checksum's whole
    /// purpose is the digest, not just an explanation attached to a failure.
    fn detail(&self) -> String {
        match self {
            ChecksumStatus::Pending | ChecksumStatus::Running => String::new(),
            ChecksumStatus::Ok(digest) => hex::encode(digest),
            ChecksumStatus::Failed(e) => e.clone(),
        }
    }

    fn tally(&self, counts: &mut Counts) {
        match self {
            ChecksumStatus::Pending | ChecksumStatus::Running => {}
            ChecksumStatus::Ok(_) => counts.good("ok"),
            ChecksumStatus::Failed(_) => counts.bad("failed"),
        }
    }
}

pub(crate) fn run_checksum(kind: ChecksumKind, args: ChecksumArgs, theme: ThemeName) -> ExitCode {
    let label = describe(&args.paths);
    let (files, mut clean) = match collect(&args.paths) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
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
        run_checksum_screen(&mut terminal, kind, &label, &files, Theme::new(theme))
    };

    let (ok, entries) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            return ExitCode::from(2);
        }
    };
    clean &= ok;

    let mut out = ChecksumFile::new(kind);
    out.entries = entries;
    match &args.output {
        Some(path) => {
            if let Err(e) = out.write(path) {
                eprintln!("lh-tui: writing {}: {e:#}", path.display());
                return ExitCode::from(2);
            }
            eprintln!(
                "wrote {} {} entries to {}",
                out.entries.len(),
                kind.label(),
                path.display()
            );
        }
        None => print!("{}", out.render()),
    }

    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Returns whether every file computed cleanly, plus every successful entry in submission
/// order — the order `collect` produced the file list in, not completion order, so a
/// written `.ffp` does not reorder itself between runs just because the queue's worker
/// pool finished files in a different sequence this time (`docs/tui.md` §3, matching
/// `lh-gui`'s S3 checksum-create area).
pub(crate) fn run_checksum_screen(
    terminal: &mut DefaultTerminal,
    kind: ChecksumKind,
    root: &str,
    files: &[AudioFile],
    theme: Theme,
) -> io::Result<(bool, Vec<Entry>)> {
    let queue: Queue<lh_core::Result<[u8; 16]>> = Queue::new();
    for f in files {
        let path = f.path.clone();
        queue.submit(f.file_name(), move |_progress| compute(kind, &path));
    }

    let view = View {
        command: kind.label().to_string(),
        target: root,
        unit: "files",
        detail: "digest",
        widths: (35, 55),
        labels: &["ok", "failed"],
    };
    let names: Vec<String> = files.iter().map(AudioFile::file_name).collect();
    let outcome = run_batch(
        terminal,
        &view,
        &names,
        queue,
        |_, output| match output {
            Ok(digest) => ChecksumStatus::Ok(digest),
            Err(e) => ChecksumStatus::Failed(e.to_string()),
        },
        &theme,
    )?;

    let entries = files
        .iter()
        .zip(&outcome.rows)
        .filter_map(|(f, status)| match status {
            ChecksumStatus::Ok(digest) => Some(Entry {
                file_name: f.file_name(),
                digest: *digest,
            }),
            _ => None,
        })
        .collect();
    Ok((outcome.clean, entries))
}
