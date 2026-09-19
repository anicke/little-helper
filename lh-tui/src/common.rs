use std::time::Instant;

use lh_cli::Paths;
use lh_core::checksum::ChecksumKind;
use lh_core::model::AudioFile;
use lh_core::scan;
use std::path::Path;

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

/// What the header shows for where these files came from: the one path given, or a count
/// when there were several — `Paths` allows more than one, unlike the plain folder this
/// screen used to assume.
pub(crate) fn describe(paths: &Paths) -> String {
    match paths.paths.as_slice() {
        [one] => one.display().to_string(),
        many => format!("{} paths", many.len()),
    }
}
