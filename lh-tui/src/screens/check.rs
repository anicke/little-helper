use std::io;
use std::process::ExitCode;

use crate::*;
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, EntryOutcome, check_entry};
use lh_core::job::Queue;
use ratatui::DefaultTerminal;
use ratatui::style::Style;
use std::path::{Path, PathBuf};

// --- Check (verify files against an existing .ffp/.md5/.st5) ------------------------
//
// Unlike verify/checksum, the row list here doesn't come from scanning a folder for
// audio files — it comes from the checksum file's own entries (`lh_cli::cmd_check`'s
// shape), and checking one just means recomputing its digest and comparing, so a row can
// also come back `Missing`: the entry names a file that isn't there at all, which is not
// the same kind of trouble as a digest that doesn't match or a read that failed outright.

#[derive(Clone)]
pub(crate) enum CheckStatus {
    Pending,
    Running,
    Ok,
    Mismatch {
        expected: [u8; 16],
        actual: [u8; 16],
    },
    Missing,
    Failed(String),
}

impl RowStatus for CheckStatus {
    fn pending() -> Self {
        CheckStatus::Pending
    }

    fn running(_done: u32, _total: u32) -> Self {
        CheckStatus::Running
    }

    fn cancelled() -> Self {
        CheckStatus::Failed("cancelled".to_string())
    }

    fn cell(&self, spin: char, theme: &Theme) -> (String, Style) {
        match self {
            CheckStatus::Pending => ("pending".to_string(), theme.dim),
            CheckStatus::Running => (format!("{spin} running"), theme.accent),
            CheckStatus::Ok => ("OK".to_string(), theme.ok),
            CheckStatus::Missing => ("MISSING".to_string(), theme.warn),
            CheckStatus::Mismatch { .. } => ("MISMATCH".to_string(), theme.error),
            CheckStatus::Failed(_) => ("FAILED".to_string(), theme.error),
        }
    }

    fn detail(&self) -> String {
        match self {
            CheckStatus::Pending | CheckStatus::Running | CheckStatus::Ok => String::new(),
            CheckStatus::Missing => "no such file".to_string(),
            CheckStatus::Mismatch { expected, actual } => {
                format!(
                    "expected {} actual {}",
                    hex::encode(expected),
                    hex::encode(actual)
                )
            }
            CheckStatus::Failed(e) => e.clone(),
        }
    }

    fn tally(&self, counts: &mut Counts) {
        match self {
            CheckStatus::Pending | CheckStatus::Running => {}
            CheckStatus::Ok => counts.good("ok"),
            CheckStatus::Missing => counts.bad("missing"),
            CheckStatus::Mismatch { .. } => counts.bad("mismatch"),
            CheckStatus::Failed(_) => counts.bad("failed"),
        }
    }
}

/// The list in `file`, what kind it is, and the folder its entries are relative to.
pub(crate) fn prepare_check(file: &Path) -> Result<(ChecksumKind, ChecksumFile, PathBuf), Refusal> {
    let kind = checksum_kind_for(file).map_err(|e| Refusal::new(2, format!("lh-tui: {e:#}")))?;
    let list = ChecksumFile::read(kind, file)
        .map_err(|e| Refusal::new(2, format!("lh-tui: reading {}: {e:#}", file.display())))?;
    let dir = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok((kind, list, dir))
}

pub(crate) fn run_check(file: PathBuf, theme: ThemeName) -> ExitCode {
    let (kind, list, dir) = match prepare_check(&file) {
        Ok(v) => v,
        Err(refusal) => return refusal.exit(),
    };
    if list.entries.is_empty() {
        eprintln!("no entries in {}", file.display());
        return ExitCode::SUCCESS;
    }

    let result = {
        let mut terminal = TerminalGuard::new();
        run_check_screen(
            &mut terminal,
            kind,
            &file.display().to_string(),
            dir,
            &list.entries,
            Theme::new(theme),
        )
    };

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether every entry checked out clean — the same notion of "ok" `lh check`'s
/// exit code uses: no missing file, no mismatch, no read failure.
pub(crate) fn run_check_screen(
    terminal: &mut DefaultTerminal,
    kind: ChecksumKind,
    label: &str,
    dir: PathBuf,
    entries: &[Entry],
    theme: Theme,
) -> io::Result<bool> {
    let queue: Queue<EntryOutcome> = Queue::new();
    for e in entries {
        let target_dir = dir.clone();
        let entry = e.clone();
        queue.submit(e.file_name.clone(), move |_progress| {
            check_entry(kind, &target_dir, &entry)
        });
    }

    let view = View {
        command: format!("check {}", kind.label()),
        target: label,
        unit: "entries",
        detail: "detail",
        widths: (45, 45),
        labels: &["ok", "missing", "mismatch", "failed"],
    };
    let names: Vec<String> = entries.iter().map(|e| e.file_name.clone()).collect();
    let outcome = run_batch(
        terminal,
        &view,
        &names,
        queue,
        |_, output| match output {
            EntryOutcome::Ok => CheckStatus::Ok,
            EntryOutcome::Mismatch { expected, actual } => {
                CheckStatus::Mismatch { expected, actual }
            }
            EntryOutcome::Missing => CheckStatus::Missing,
            EntryOutcome::Failed(e) => CheckStatus::Failed(e.to_string()),
        },
        &theme,
    )?;
    Ok(outcome.clean)
}
