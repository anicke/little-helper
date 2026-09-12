//! Writing a file without ever putting a half-written one under its real name.
//!
//! Principle 1: v0.1 modifies nothing in place. Outputs are staged beside the destination
//! and renamed in only once they have been checked, so an interrupted run leaves the
//! original untouched and leaves no debris under the name people will look for.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// A destination being built. Dropping it without committing removes the partial file
/// (Principle 1: never a half-written file under the real name).
pub struct TempOutput {
    temp: PathBuf,
    final_path: PathBuf,
    committed: bool,
}

impl TempOutput {
    pub fn stage(src: &Path, dst: &Path, overwrite: bool) -> Result<Self> {
        if same_file(src, dst) {
            return Err(Error::malformed(
                dst,
                "the output is the input; that would destroy the original",
            ));
        }
        if dst.exists() && !overwrite {
            return Err(Error::OutputExists {
                path: dst.to_path_buf(),
            });
        }
        if let Some(parent) = dst.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        // Beside the destination, so the rename stays on one filesystem and is atomic.
        let name = dst.file_name().unwrap_or_default().to_string_lossy();
        let temp = dst.with_file_name(format!(".{name}.lh-{}.part", std::process::id()));
        Ok(Self {
            temp,
            final_path: dst.to_path_buf(),
            committed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.temp
    }

    pub fn commit(mut self) -> Result<PathBuf> {
        std::fs::rename(&self.temp, &self.final_path)
            .map_err(|e| Error::io(&self.final_path, e))?;
        self.committed = true;
        Ok(self.final_path.clone())
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

/// Rename every staged output into place, or none of them.
///
/// [`TempOutput::commit`] is one file at a time; a repair touches two files (or a whole
/// chain of them) at once, and the set must never be observed half fixed under real names
/// (Principle 1, docs/sbe-repair.md §4 step 7). If a rename partway through fails, every
/// output already renamed in this call is removed again before the error is returned.
pub fn commit_all(outputs: Vec<TempOutput>) -> Result<Vec<PathBuf>> {
    let mut done: Vec<PathBuf> = Vec::with_capacity(outputs.len());
    for mut output in outputs {
        match std::fs::rename(&output.temp, &output.final_path) {
            Ok(()) => {
                // Already moved out from under `output.temp`, so `Drop` has nothing left
                // to clean up.
                output.committed = true;
                done.push(output.final_path.clone());
            }
            Err(e) => {
                let failed_path = output.final_path.clone();
                for path in done.into_iter().rev() {
                    let _ = std::fs::remove_file(&path);
                }
                return Err(Error::io(&failed_path, e));
            }
        }
    }
    Ok(done)
}

/// True when both paths name a file that already exists and is the same one. A
/// destination that does not exist yet cannot be the source.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_all_renames_every_output() {
        let dir = tempfile::tempdir().unwrap();
        let src_a = dir.path().join("src_a");
        let src_b = dir.path().join("src_b");
        std::fs::write(&src_a, b"a").unwrap();
        std::fs::write(&src_b, b"b").unwrap();
        let dst_a = dir.path().join("dst_a");
        let dst_b = dir.path().join("dst_b");

        let a = TempOutput::stage(&src_a, &dst_a, false).unwrap();
        std::fs::write(a.path(), b"fixed a").unwrap();
        let b = TempOutput::stage(&src_b, &dst_b, false).unwrap();
        std::fs::write(b.path(), b"fixed b").unwrap();

        let committed = commit_all(vec![a, b]).unwrap();
        assert_eq!(committed, vec![dst_a.clone(), dst_b.clone()]);
        assert_eq!(std::fs::read(&dst_a).unwrap(), b"fixed a");
        assert_eq!(std::fs::read(&dst_b).unwrap(), b"fixed b");
    }

    /// If a later output in the batch cannot be committed, an earlier one already renamed
    /// into place in the same call must not survive under its real name either — the set
    /// is fixed all at once or not at all.
    #[test]
    fn commit_all_undoes_earlier_renames_when_a_later_one_fails() {
        let dir = tempfile::tempdir().unwrap();
        let src_a = dir.path().join("src_a");
        let src_b = dir.path().join("src_b");
        std::fs::write(&src_a, b"a").unwrap();
        std::fs::write(&src_b, b"b").unwrap();
        let dst_a = dir.path().join("dst_a");
        let dst_b = dir.path().join("dst_b");

        let a = TempOutput::stage(&src_a, &dst_a, false).unwrap();
        std::fs::write(a.path(), b"fixed a").unwrap();
        let b = TempOutput::stage(&src_b, &dst_b, false).unwrap();
        std::fs::write(b.path(), b"fixed b").unwrap();
        // Sabotage the second rename: remove the staged temp file out from under it.
        std::fs::remove_file(b.path()).unwrap();

        let err = commit_all(vec![a, b]).unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err}");
        assert!(!dst_a.exists(), "the first file must not survive alone");
        assert!(!dst_b.exists());
    }
}
