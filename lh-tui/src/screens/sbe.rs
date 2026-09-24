use std::io;
use std::process::ExitCode;

use crate::*;
use lh_cli::Paths;
use lh_core::analysis::{Sbe, sbe};
use lh_core::job::Queue;
use lh_core::model::AudioFile;
use ratatui::DefaultTerminal;
use ratatui::style::Style;

// --- SBE (sector boundary error) ------------------------------------------------------
//
// `analysis::sbe` is a pure, infallible function over a `StreamInfo` `collect` already
// probed — no decode, no I/O, no `Result` — so this screen is the same per-file batch shape
// as verify/checksum (`docs/tui.md` §2) with `T = Sbe` directly rather than `Result<Sbe>`,
// mirroring `cmd_sbe`'s own `run_batch(&files, |f, _| sbe(&f.stream_info))`.

#[derive(Clone)]
pub(crate) enum SbeStatus {
    Pending,
    Running,
    Aligned,
    Misaligned { remainder_frames: u64 },
    NotApplicable { reason: &'static str },
    Failed(String),
}

impl RowStatus for SbeStatus {
    fn pending() -> Self {
        SbeStatus::Pending
    }

    fn running(_done: u32, _total: u32) -> Self {
        SbeStatus::Running
    }

    fn cancelled() -> Self {
        SbeStatus::Failed("cancelled".to_string())
    }

    fn cell(&self, spin: char, theme: &Theme) -> (String, Style) {
        match self {
            SbeStatus::Pending => ("pending".to_string(), theme.dim),
            SbeStatus::Running => (format!("{spin} running"), theme.accent),
            SbeStatus::Aligned => ("ALIGNED".to_string(), theme.ok),
            SbeStatus::Misaligned { .. } => ("MISALIGNED".to_string(), theme.error),
            // Neutral, not a warning: most non-CDDA files hit this and it doesn't count
            // against "clean" (same treatment convert gives `Skipped`, not verify's `NoMd5`).
            SbeStatus::NotApplicable { .. } => ("N/A".to_string(), theme.dim),
            SbeStatus::Failed(_) => ("FAILED".to_string(), theme.error),
        }
    }

    fn detail(&self) -> String {
        match self {
            SbeStatus::Pending | SbeStatus::Running | SbeStatus::Aligned => String::new(),
            SbeStatus::Misaligned { remainder_frames } => {
                format!("+{remainder_frames} frames past a sector boundary")
            }
            SbeStatus::NotApplicable { reason } => reason.to_string(),
            SbeStatus::Failed(e) => e.clone(),
        }
    }

    fn tally(&self, counts: &mut Counts) {
        match self {
            SbeStatus::Pending | SbeStatus::Running => {}
            SbeStatus::Aligned => counts.good("aligned"),
            SbeStatus::Misaligned { .. } => counts.bad("misaligned"),
            SbeStatus::NotApplicable { .. } => counts.good("n/a"),
            SbeStatus::Failed(_) => counts.bad("failed"),
        }
    }
}

pub(crate) fn run_sbe(paths: Paths, theme: ThemeName) -> ExitCode {
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
        run_sbe_screen(&mut terminal, &label, &files, Theme::new(theme))
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

/// Returns whether every file came back clean — no misalignment, no failure — the same
/// notion `lh sbe`'s own exit code uses. `NotApplicable` doesn't count against it, the same
/// way `cmd_sbe` never sets `ok = false` for a file that simply isn't CD audio.
pub(crate) fn run_sbe_screen(
    terminal: &mut DefaultTerminal,
    root: &str,
    files: &[AudioFile],
    theme: Theme,
) -> io::Result<bool> {
    let queue: Queue<Sbe> = Queue::new();
    for f in files {
        let info = f.stream_info.clone();
        queue.submit(f.file_name(), move |_progress| sbe(&info));
    }

    let view = View {
        command: "sbe".to_string(),
        target: root,
        unit: "files",
        detail: "detail",
        widths: (45, 45),
        labels: &["aligned", "n/a", "misaligned", "failed"],
    };
    let names: Vec<String> = files.iter().map(AudioFile::file_name).collect();
    let outcome = run_batch(
        terminal,
        &view,
        &names,
        queue,
        |_, output| match output {
            Sbe::Aligned => SbeStatus::Aligned,
            Sbe::Misaligned { remainder_frames } => SbeStatus::Misaligned { remainder_frames },
            Sbe::NotApplicable { reason } => SbeStatus::NotApplicable { reason },
        },
        &theme,
    )?;
    Ok(outcome.clean)
}
