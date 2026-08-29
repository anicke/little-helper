//! Finding a torrent's files on disk, and the size pre-check.
//!
//! Nothing here reads file contents. Sizes alone answer "did the download finish?", which
//! is the question `--quick` exists for; "is it intact?" needs the piece hashing in T3.

use super::metainfo::Metainfo;
use super::report::{FileOutcome, FileStatus, TorrentReport};
use crate::error::{Error, Result};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Names Windows refuses regardless of extension. A torrent containing one cannot be
/// written there, and silently mangling the name would be worse than saying so.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Work out which directory the torrent's relative paths hang off.
///
/// Users point at either the folder *containing* the show or the show folder *itself*,
/// and both should work.
pub fn resolve_root(meta: &Metainfo, given: &Path) -> Result<PathBuf> {
    if given.is_file() {
        // They pointed at the file itself; its parent is the root.
        return Ok(given
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf());
    }
    if !given.is_dir() {
        return Err(Error::torrent(
            given,
            "no such file or directory to check the torrent against",
        ));
    }
    // A single-file torrent has no root directory of its own: `name` is the file.
    if meta.is_single_file {
        return Ok(given.to_path_buf());
    }
    let inside = given.join(&meta.name);
    if inside.is_dir() {
        return Ok(inside);
    }
    if given.file_name().is_some_and(|n| n == meta.name.as_str()) {
        return Ok(given.to_path_buf());
    }
    // Fall back to what we were given and let the per-file report say what is missing.
    Ok(given.to_path_buf())
}

/// Join validated components onto the root, refusing anything that could still escape it.
///
/// Components were already checked at parse time, so this is the second line of defence
/// plus the platform-specific rules that only matter once a real path is built.
pub fn join_checked(root: &Path, components: &[String], origin: &Path) -> Result<PathBuf> {
    for component in components {
        // Reserved names are matched on the stem, so "CON.txt" is caught too.
        let stem = component
            .split('.')
            .next()
            .unwrap_or(component)
            .to_ascii_uppercase();
        if WINDOWS_RESERVED.contains(&stem.as_str()) {
            return Err(Error::unsafe_path(
                origin,
                format!("{component:?} is a reserved device name on Windows"),
            ));
        }
    }

    let joined = components.iter().fold(root.to_path_buf(), |p, c| p.join(c));

    // Belt and braces: nothing that survived validation should be able to climb out, so
    // if this ever fires the validation above has a hole in it.
    if joined
        .strip_prefix(root)
        .is_err_and(|_| !root.as_os_str().is_empty())
        || joined.components().any(|c| c == Component::ParentDir)
    {
        return Err(Error::unsafe_path(
            origin,
            format!("{} escapes the torrent root", joined.display()),
        ));
    }
    Ok(joined)
}

/// Compare what the torrent says against what is on disk, by size only.
pub fn check_sizes(meta: &Metainfo, torrent_path: &Path, given: &Path) -> Result<TorrentReport> {
    let root = resolve_root(meta, given)?;
    let mut files = Vec::with_capacity(meta.files.len());
    let mut expected_paths = HashSet::new();

    for (index, file) in meta.files.iter().enumerate() {
        let path = join_checked(&root, &file.path, torrent_path)?;
        expected_paths.insert(path.clone());

        // Padding is stream bytes, not a file anyone downloads.
        if file.is_pad {
            files.push(FileOutcome {
                index,
                path,
                status: FileStatus::Padding,
            });
            continue;
        }

        let status = match std::fs::metadata(&path) {
            Ok(m) if !m.is_file() => FileStatus::Unreadable {
                reason: "not a regular file".into(),
            },
            Ok(m) if m.len() == file.length => FileStatus::SizeOk,
            Ok(m) => FileStatus::WrongSize {
                expected: file.length,
                actual: m.len(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileStatus::Missing,
            Err(e) => FileStatus::Unreadable {
                reason: e.to_string(),
            },
        };
        files.push(FileOutcome {
            index,
            path,
            status,
        });
    }

    Ok(TorrentReport {
        info_hash: meta.info_hash,
        name: meta.name.clone(),
        extra_local: find_extras(meta, &root, &expected_paths),
        root,
        files,
        quick: true,
        pieces: None,
    })
}

/// Files sitting in the torrent's folder that the torrent does not list. Traders keep
/// `info.txt`, artwork and `.ffp` sidecars next to a show and want them acknowledged, not
/// flagged as errors.
///
/// Skipped entirely for single-file torrents: the containing directory is the user's own,
/// and listing its other contents would be noise.
fn find_extras(meta: &Metainfo, root: &Path, expected: &HashSet<PathBuf>) -> Vec<PathBuf> {
    if meta.is_single_file || !root.is_dir() {
        return Vec::new();
    }
    let mut extras: Vec<PathBuf> = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| !expected.contains(p))
        .collect();
    extras.sort();
    extras
}
