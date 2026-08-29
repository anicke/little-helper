use crate::error::Result;
use crate::format;
use crate::model::{AudioFile, AudioFormat};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// One folder's worth of files — a show, or a few shows. Not an archive.
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub files: Vec<AudioFile>,
    /// Files we recognized but could not read, with the reason. Never silently dropped.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Walk `root` and probe every audio file found. `recursive` follows subdirectories,
/// which is how a multi-disc show is usually laid out.
pub fn scan(root: &Path, recursive: bool) -> Result<WorkingSet> {
    let mut set = WorkingSet::default();
    let walker = WalkDir::new(root)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .sort_by_file_name();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e.path().unwrap_or(root).to_path_buf();
                set.skipped.push((path, e.to_string()));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if AudioFormat::from_path(path).is_none() {
            continue;
        }
        match format::probe(path) {
            Ok(f) => set.files.push(f),
            Err(e) => set.skipped.push((path.to_path_buf(), e.to_string())),
        }
    }
    Ok(set)
}
