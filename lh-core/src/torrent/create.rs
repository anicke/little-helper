//! Making a `.torrent` from a folder on disk.
//!
//! The payload is only ever read; the single thing written is the new `.torrent`, staged
//! beside its destination and renamed in after it has been checked (Principle 1).
//!
//! Everything downstream of the file list depends on the file list being *exactly* right,
//! so this module is mostly about being explicit: what was included, what was excluded and
//! why, and what order it all went in. Nothing is dropped silently.

use super::encode::{Content, Draft, encode};
use super::metainfo::{Metainfo, TorrentFile};
use super::stream::{READ_BUF, SpanReader, build_spans, spans_overlapping};
use crate::error::{Error, Result};
use crate::output::TempOutput;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

/// Piece length bounds. Below 16 KiB the metadata bloats; above 16 MiB a single bad piece
/// costs an absurd re-download. Both ends are what the rest of the world uses.
pub const MIN_PIECE_LENGTH: u64 = 16 * 1024;
pub const MAX_PIECE_LENGTH: u64 = 16 * 1024 * 1024;

/// What the automatic piece length aims for. `pieces` costs 20 bytes each, so this also
/// decides how big the `.torrent` itself gets: 2000 pieces is a 40 KB torrent.
const TARGET_PIECES: u64 = 2000;

/// Names we leave out unless told otherwise. Nobody wants `Thumbs.db` in a seed, and a
/// torrent of a torrent helps no one.
const NOISE: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

#[derive(Debug, Clone)]
pub struct CreateOpts {
    /// Tracker tiers, already resolved to real URLs. Empty makes a trackerless torrent.
    pub announce: Vec<Vec<String>>,
    /// `None` picks one from the payload size.
    pub piece_length: Option<u64>,
    pub private: bool,
    pub source: Option<String>,
    pub comment: Option<String>,
    /// Keep the files that would otherwise be excluded as noise.
    pub include_all: bool,
    pub overwrite: bool,
    pub created_by: String,
}

impl Default for CreateOpts {
    fn default() -> Self {
        Self {
            announce: Vec::new(),
            piece_length: None,
            private: false,
            source: None,
            comment: None,
            include_all: false,
            overwrite: false,
            created_by: format!("Little Helper {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Why a file on disk did not make it into the torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// OS metadata, or a `.torrent`, or one of our own staging files.
    Noise,
    /// v1 torrents cannot express an empty directory at all.
    EmptyDirectory,
}

impl Skipped {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Noise => "not part of the recording",
            Self::EmptyDirectory => "empty directories cannot be expressed in a v1 torrent",
        }
    }
}

/// What was made, and what was left out of it.
#[derive(Debug, Clone)]
pub struct Created {
    pub path: PathBuf,
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u64,
    pub pieces: usize,
    pub files: Vec<TorrentFile>,
    pub total_length: u64,
    /// Never dropped silently: the caller is expected to show these.
    pub excluded: Vec<(PathBuf, Skipped)>,
}

impl Created {
    pub fn info_hash_hex(&self) -> String {
        hex::encode(self.info_hash)
    }
}

/// Where a torrent goes when nobody names a destination: beside the source, named after
/// it. Not cosmetic — writing the `.torrent` *inside* the folder it describes would add a
/// file to that folder, so re-creating it later produces a different infohash
/// (`docs/torrent-creation.md` §6). Both front ends need exactly this, so it lives here
/// rather than being duplicated the way `convert::destination` was before G3.
pub fn default_output(source: &Path) -> Option<PathBuf> {
    let parent = source.parent()?;
    let mut name = source.file_name()?.to_os_string();
    name.push(".torrent");
    Some(parent.join(name))
}

/// A file that will go into the torrent.
struct SourceFile {
    /// Path components relative to the torrent root.
    path: Vec<String>,
    /// Where it actually lives.
    disk: PathBuf,
    length: u64,
}

/// Build a torrent for `source` — a folder, or a single file — and write it to `dst`.
pub fn create(source: &Path, dst: &Path, opts: &CreateOpts) -> Result<Created> {
    create_with_progress(source, dst, opts, &mut |_, _| true)
}

/// `progress` is called with (pieces done, pieces total) as the payload is walked. It
/// returns whether to keep going — `false` stops the walk and the call returns
/// `Err(Error::Cancelled)` rather than a partial `Created`, matching Principle 1: nothing
/// half-done is ever handed back as though it were a result. This is `job`'s cancellation
/// checkpoint (docs/job-queue.md §2); `create.rs` has no dependency on the `job` module
/// itself, only on this bool.
pub fn create_with_progress(
    source: &Path,
    dst: &Path,
    opts: &CreateOpts,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<Created> {
    let name = torrent_name(source)?;
    let meta = std::fs::metadata(source).map_err(|e| Error::io(source, e))?;
    let single = meta.is_file();

    let mut excluded = Vec::new();
    let files = if single {
        vec![SourceFile {
            path: vec![name.clone()],
            disk: source.to_path_buf(),
            length: meta.len(),
        }]
    } else {
        collect(source, opts.include_all, &mut excluded)?
    };

    if files.is_empty() {
        return Err(Error::TorrentEncode {
            detail: format!("{} contains no files to put in a torrent", source.display()),
        });
    }
    let total_length: u64 = files.iter().map(|f| f.length).sum();
    if total_length == 0 {
        return Err(Error::TorrentEncode {
            detail: format!("{} has no data in it to seed", source.display()),
        });
    }

    let piece_length = match opts.piece_length {
        Some(n) => {
            validate_piece_length(n)?;
            n
        }
        None => auto_piece_length(total_length),
    };

    let pieces = hash_pieces(&files, piece_length, total_length, progress)?;

    let draft = Draft {
        name: name.clone(),
        piece_length,
        pieces,
        content: if single {
            Content::Single {
                length: total_length,
            }
        } else {
            Content::Multi {
                files: files
                    .iter()
                    .map(|f| TorrentFile {
                        path: f.path.clone(),
                        length: f.length,
                        // We do not create padding: see the plan's open question 5.
                        is_pad: false,
                    })
                    .collect(),
            }
        },
        private: opts.private,
        source: opts.source.clone(),
        announce: opts.announce.clone(),
        comment: opts.comment.clone(),
        created_by: Some(opts.created_by.clone()),
        // Epoch seconds, UTC. TLH shipped this as local time and had to fix it.
        creation_date: Some(now_utc_secs()),
    };
    let encoded = encode(&draft)?;

    // Read back what we are about to write. It costs nothing next to the payload walk, and
    // it catches an encoder that produced a file our own parser would reject.
    let round_trip = Metainfo::from_bytes(&encoded.bytes, dst)?;
    if round_trip.info_hash != encoded.info_hash
        || round_trip.pieces != draft.pieces
        || round_trip.files.len() != draft_file_count(&draft)
    {
        return Err(Error::TorrentEncode {
            detail: "the torrent we encoded does not read back as the one we meant to write".into(),
        });
    }

    let temp = TempOutput::stage(source, dst, opts.overwrite)?;
    std::fs::write(temp.path(), &encoded.bytes).map_err(|e| Error::io(temp.path(), e))?;

    Ok(Created {
        path: temp.commit()?,
        info_hash: encoded.info_hash,
        name,
        piece_length,
        pieces: draft.pieces.len(),
        files: round_trip.files,
        total_length,
        excluded,
    })
}

fn draft_file_count(draft: &Draft) -> usize {
    match &draft.content {
        Content::Single { .. } => 1,
        Content::Multi { files } => files.len(),
    }
}

/// The torrent's `name`, which is the folder's own name — or the file's, for a single-file
/// torrent.
fn torrent_name(source: &Path) -> Result<String> {
    let raw = source
        .canonicalize()
        .map_err(|e| Error::io(source, e))?
        .file_name()
        .map(|n| n.to_owned())
        .ok_or_else(|| Error::TorrentEncode {
            detail: format!("{} has no name to give the torrent", source.display()),
        })?;
    raw.into_string().map_err(|bad| Error::TorrentEncode {
        detail: format!(
            "{:?} is not valid UTF-8, and torrent names must be (BEP 3)",
            bad.to_string_lossy()
        ),
    })
}

/// Walk the folder into a file list, in the order the stream will use.
///
/// Symlinks are refused rather than followed: following one pulls data from outside the
/// folder into a torrent the user believes describes the folder.
fn collect(
    root: &Path,
    include_all: bool,
    excluded: &mut Vec<(PathBuf, Skipped)>,
) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    walk(
        root,
        root,
        &mut Vec::new(),
        include_all,
        &mut files,
        excluded,
    )?;

    // Byte-wise over the joined path, which is what `mktorrent` does — checked against it
    // rather than assumed, because a different order is a different infohash and our
    // torrent would then never deduplicate against anyone else's.
    files.sort_by(|a, b| {
        a.path
            .join("/")
            .into_bytes()
            .cmp(&b.path.join("/").into_bytes())
    });
    Ok(files)
}

fn walk(
    root: &Path,
    dir: &Path,
    prefix: &mut Vec<String>,
    include_all: bool,
    files: &mut Vec<SourceFile>,
    excluded: &mut Vec<(PathBuf, Skipped)>,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| Error::io(dir, e))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::io(dir, e))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut found_anything = false;
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| Error::io(&path, e))?;

        if kind.is_symlink() {
            return Err(Error::TorrentEncode {
                detail: format!(
                    "{} is a symbolic link; following it could put data from outside \
                     {} into the torrent",
                    path.display(),
                    root.display()
                ),
            });
        }

        let name = entry
            .file_name()
            .into_string()
            .map_err(|bad| Error::TorrentEncode {
                detail: format!(
                    "{:?} is not valid UTF-8, and torrent paths must be (BEP 3)",
                    bad.to_string_lossy()
                ),
            })?;

        if kind.is_dir() {
            prefix.push(name);
            walk(root, &path, prefix, include_all, files, excluded)?;
            prefix.pop();
            found_anything = true;
            continue;
        }

        if !include_all && is_noise(&name) {
            excluded.push((path, Skipped::Noise));
            continue;
        }

        let mut components = prefix.clone();
        components.push(name);
        files.push(SourceFile {
            path: components,
            disk: path,
            length: entry.metadata().map_err(|e| Error::io(dir, e))?.len(),
        });
        found_anything = true;
    }

    // An empty directory has no representation in a v1 torrent. Say so; do not drop it
    // and let the user wonder later why it did not come back.
    if !found_anything && dir != root {
        excluded.push((dir.to_path_buf(), Skipped::EmptyDirectory));
    }
    Ok(())
}

fn is_noise(name: &str) -> bool {
    NOISE.contains(&name)
        || name.starts_with("._")
        || name.ends_with(".torrent")
        // Our own staging files, which should never exist at rest anyway.
        || (name.starts_with('.') && name.ends_with(".part"))
}

/// The smallest power of two in range that keeps the piece count within `TARGET_PIECES`.
pub fn auto_piece_length(total_length: u64) -> u64 {
    let mut len = MIN_PIECE_LENGTH;
    while len < MAX_PIECE_LENGTH && total_length.div_ceil(len) > TARGET_PIECES {
        len *= 2;
    }
    len
}

fn validate_piece_length(n: u64) -> Result<()> {
    if !(MIN_PIECE_LENGTH..=MAX_PIECE_LENGTH).contains(&n) || !n.is_power_of_two() {
        // BEP 3 does not strictly require a power of two, but clients in the wild reject
        // anything else, and a torrent nobody can load is worse than an argument error.
        return Err(Error::TorrentEncode {
            detail: format!(
                "piece length {n} is not a power of two between {MIN_PIECE_LENGTH} and \
                 {MAX_PIECE_LENGTH} bytes"
            ),
        });
    }
    Ok(())
}

/// One sequential pass over the concatenated stream, SHA-1 per piece.
fn hash_pieces(
    files: &[SourceFile],
    piece_length: u64,
    total_length: u64,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<Vec<[u8; 20]>> {
    let spans = build_spans(files.iter().map(|f| f.length));
    let count = total_length.div_ceil(piece_length);
    let total_pieces = u32::try_from(count).map_err(|_| Error::TorrentEncode {
        detail: format!("{count} pieces is more than a torrent can hold"),
    })?;

    let mut reader = SpanReader::default();
    let mut buf = vec![0u8; READ_BUF];
    let mut pieces = Vec::with_capacity(total_pieces as usize);

    for index in 0..u64::from(total_pieces) {
        let piece_start = index * piece_length;
        let piece_end = (piece_start + piece_length).min(total_length);

        let mut hasher = Sha1::new();
        for span in spans_overlapping(&spans, piece_start, piece_end) {
            let from = piece_start.max(span.start);
            let to = piece_end.min(span.end);
            if to == from {
                continue;
            }
            let file = &files[span.index];
            reader.read_into(
                &mut hasher,
                span.index,
                &file.disk,
                from - span.start,
                to - from,
                &mut buf,
            )?;
        }
        pieces.push(hasher.finalize().into());
        if !progress(index as u32 + 1, total_pieces) {
            return Err(Error::Cancelled);
        }
    }
    Ok(pieces)
}

fn now_utc_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
