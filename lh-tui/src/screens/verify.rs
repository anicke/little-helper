use std::io;
use std::process::ExitCode;

use crate::*;
use lh_cli::Paths;
use lh_core::analysis::{Verification, verify};
use lh_core::job::Queue;
use lh_core::model::AudioFile;
use ratatui::DefaultTerminal;
use ratatui::style::Style;

#[derive(Clone)]
pub(crate) enum Status {
    Pending,
    Running,
    Ok,
    NoMd5,
    Mismatch {
        stored: [u8; 16],
        computed: [u8; 16],
    },
    Failed(String),
}

impl RowStatus for Status {
    fn pending() -> Self {
        Status::Pending
    }

    fn running(_done: u32, _total: u32) -> Self {
        Status::Running
    }

    fn cancelled() -> Self {
        Status::Failed("cancelled".to_string())
    }

    fn cell(&self, spin: char, theme: &Theme) -> (String, Style) {
        match self {
            Status::Pending => ("pending".to_string(), theme.dim),
            Status::Running => (format!("{spin} running"), theme.accent),
            Status::Ok => ("OK".to_string(), theme.ok),
            Status::NoMd5 => ("NO MD5".to_string(), theme.warn),
            Status::Mismatch { .. } => ("MISMATCH".to_string(), theme.error),
            Status::Failed(_) => ("FAILED".to_string(), theme.error),
        }
    }

    fn detail(&self) -> String {
        match self {
            Status::Pending | Status::Running | Status::Ok => String::new(),
            Status::NoMd5 => "decoded cleanly, nothing to compare".to_string(),
            Status::Mismatch { stored, computed } => {
                format!(
                    "stored {} computed {}",
                    hex::encode(stored),
                    hex::encode(computed)
                )
            }
            Status::Failed(e) => e.clone(),
        }
    }

    fn tally(&self, counts: &mut Counts) {
        match self {
            Status::Pending | Status::Running => {}
            Status::Ok => counts.good("ok"),
            Status::NoMd5 => counts.good("no-md5"),
            Status::Mismatch { .. } => counts.bad("mismatch"),
            Status::Failed(_) => counts.bad("failed"),
        }
    }
}

pub(crate) fn run_verify(paths: Paths, theme: ThemeName) -> ExitCode {
    let label = describe(&paths);
    let (files, mut clean) = match collect(&paths) {
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
        run_verify_screen(&mut terminal, &label, &files, Theme::new(theme))
    };

    match result {
        Ok(ok) => {
            clean &= ok;
            if clean {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether every file verified cleanly (no mismatches, no failures) — the same
/// notion of "ok" `lh verify`'s exit code uses, so quitting the screen early still leaves
/// scripts able to tell success from trouble via `$?`.
pub(crate) fn run_verify_screen(
    terminal: &mut DefaultTerminal,
    root: &str,
    files: &[AudioFile],
    theme: Theme,
) -> io::Result<bool> {
    let queue: Queue<lh_core::Result<Verification>> = Queue::new();
    for f in files {
        let path = f.path.clone();
        queue.submit(f.file_name(), move |_progress| verify(&path));
    }

    let view = View {
        command: "verify".to_string(),
        target: root,
        unit: "files",
        detail: "detail",
        widths: (45, 45),
        labels: &["ok", "no-md5", "mismatch", "failed"],
    };
    let names: Vec<String> = files.iter().map(AudioFile::file_name).collect();
    let outcome = run_batch(
        terminal,
        &view,
        &names,
        queue,
        |_, output| match output {
            Ok(Verification::Ok) => Status::Ok,
            Ok(Verification::NoStoredMd5 { .. }) => Status::NoMd5,
            Ok(Verification::Md5Mismatch { stored, computed }) => {
                Status::Mismatch { stored, computed }
            }
            Err(e) => Status::Failed(e.to_string()),
        },
        &theme,
    )?;
    Ok(outcome.clean)
}
