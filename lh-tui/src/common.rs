use std::process::ExitCode;
use std::time::Instant;

use lh_cli::Paths;
use lh_core::checksum::ChecksumKind;
use lh_core::model::AudioFile;
use lh_core::scan;
use std::path::Path;

/// `q`, `Esc` or `Ctrl-C`: the keys every screen leaves on.
pub(crate) fn is_quit(key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

pub(crate) const SPINNER: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];

/// The elapsed time a header shows: keeps advancing every frame while `done` is false, then
/// freezes at the instant `done` first turns true. Every screen redraws on an 80ms poll even
/// once its work is finished (waiting for `q`), so a header computing `start.elapsed()` live
/// would otherwise keep climbing while nothing is actually happening. `finished_at` is the
/// caller's own `Option<Instant>`, `None` until that instant, so the freeze survives frames.
pub(crate) fn header_elapsed(start: Instant, finished_at: &mut Option<Instant>, done: bool) -> f32 {
    if done && finished_at.is_none() {
        *finished_at = Some(Instant::now());
    }
    finished_at
        .unwrap_or_else(Instant::now)
        .duration_since(start)
        .as_secs_f32()
}

/// Expand files and folders into a flat list of audio files, reporting anything skipped
/// rather than dropping it silently — the same shape as `lh_cli::collect`, kept local so
/// this binary only reaches `lh-cli` for its grammar (`docs/architecture-cleanup.md` A2).
pub(crate) fn collect(p: &Paths) -> anyhow::Result<(Vec<AudioFile>, bool)> {
    let set = scan::collect(&p.paths, p.recursive)?;
    let mut clean = true;
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
        clean = false;
    }
    Ok((set.files, clean))
}

/// `ChecksumKind::from_path`, with an error naming the extension it could not place —
/// local for the same reason [`collect`] is (A2).
pub(crate) fn checksum_kind_for(file: &Path) -> anyhow::Result<ChecksumKind> {
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

/// A path's last component for display, or the whole path when it has none.
pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What the header shows for where these files came from: the one path given, or a count
/// when there were several — `Paths` allows more than one, unlike the plain folder this
/// screen used to assume.
pub(crate) fn describe(paths: &Paths) -> String {
    match paths.paths.as_slice() {
        [one] => one.display().to_string(),
        many => format!("{} paths", many.len()),
    }
}

/// Why a screen could not open: what to tell the person, and the exit code `lh` itself
/// would have used for it. The single-command entry points print it and exit; the
/// workspace shows it beside the step instead, since stderr is hidden behind its screen.
pub(crate) struct Refusal {
    pub(crate) message: String,
    pub(crate) code: u8,
}

impl Refusal {
    pub(crate) fn new(code: u8, message: impl Into<String>) -> Self {
        Refusal {
            message: message.into(),
            code,
        }
    }

    pub(crate) fn exit(self) -> ExitCode {
        eprintln!("{}", self.message);
        ExitCode::from(self.code)
    }
}

/// One show folder's audio files (not recursive — a show is one folder), plus a line for
/// each file the scan skipped, left for the caller to print or show.
pub(crate) struct Folder {
    pub(crate) files: Vec<AudioFile>,
    pub(crate) skipped: Vec<String>,
}

/// The scan tag and rename open with, and every workspace step: refuses a path that is not
/// a folder, or a folder with no audio in it.
pub(crate) fn scan_folder(dir: &Path) -> Result<Folder, Refusal> {
    if !dir.is_dir() {
        return Err(Refusal::new(
            2,
            format!("lh-tui: {} is not a directory", dir.display()),
        ));
    }
    let set = scan::scan(dir, false)
        .map_err(|e| Refusal::new(2, format!("lh-tui: scanning {}: {e:#}", dir.display())))?;
    if set.files.is_empty() {
        return Err(Refusal::new(
            1,
            format!("no audio files found in {}", dir.display()),
        ));
    }
    let skipped = set
        .skipped
        .iter()
        .map(|(path, why)| format!("skipped {}: {why}", path.display()))
        .collect();
    Ok(Folder {
        files: set.files,
        skipped,
    })
}
