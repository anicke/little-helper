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

/// True when both paths name a file that already exists and is the same one. A
/// destination that does not exist yet cannot be the source.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
