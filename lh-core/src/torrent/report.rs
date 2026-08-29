//! What a torrent check concluded, per file and overall.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    /// Sizes agree. Contents were not read — this is what `--quick` can conclude.
    SizeOk,
    /// Every piece overlapping this file verified.
    Complete,
    /// Pieces lying wholly inside this file failed. It is definitely this file.
    Corrupt {
        bad_pieces: Vec<u32>,
    },
    /// Only a piece shared with a neighbour failed. The data does not say which file is at
    /// fault, so we name both rather than guessing.
    Suspect {
        piece: u32,
        shared_with: Vec<usize>,
    },
    Missing,
    WrongSize {
        expected: u64,
        actual: u64,
    },
    Unreadable {
        reason: String,
    },
    /// BEP 47 padding: zero bytes in the stream that are not expected on disk.
    Padding,
}

impl FileStatus {
    /// Whether this outcome should stop the overall check from passing.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Self::Corrupt { .. }
                | Self::Suspect { .. }
                | Self::Missing
                | Self::WrongSize { .. }
                | Self::Unreadable { .. }
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::SizeOk => "SIZE OK",
            Self::Complete => "OK",
            Self::Corrupt { .. } => "CORRUPT",
            Self::Suspect { .. } => "SUSPECT",
            Self::Missing => "MISSING",
            Self::WrongSize { .. } => "WRONG SIZE",
            Self::Unreadable { .. } => "UNREADABLE",
            Self::Padding => "PADDING",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileOutcome {
    /// Index into [`crate::torrent::Metainfo::files`].
    pub index: usize,
    /// Where we looked for it.
    pub path: PathBuf,
    pub status: FileStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Everything the torrent lists is present and correct.
    Complete,
    /// Sizes all agree, but nothing was hashed.
    SizesMatch,
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct TorrentReport {
    pub info_hash: [u8; 20],
    pub name: String,
    /// The directory the torrent's paths were resolved against.
    pub root: PathBuf,
    pub files: Vec<FileOutcome>,
    /// Present locally, absent from the torrent — `info.txt`, artwork, `.ffp` sidecars.
    /// Informational, never a failure. Empty for single-file torrents, where everything
    /// else in the directory is somebody else's business.
    pub extra_local: Vec<PathBuf>,
    /// True when only sizes were compared.
    pub quick: bool,
}

impl TorrentReport {
    pub fn failures(&self) -> impl Iterator<Item = &FileOutcome> {
        self.files.iter().filter(|f| f.status.is_failure())
    }

    pub fn verdict(&self) -> Verdict {
        if self.failures().next().is_some() {
            Verdict::Incomplete
        } else if self.quick {
            Verdict::SizesMatch
        } else {
            Verdict::Complete
        }
    }
}
