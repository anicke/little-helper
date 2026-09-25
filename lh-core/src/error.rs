use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: not a recognized audio file")]
    UnknownFormat { path: PathBuf },

    #[error("{path}: {detail}")]
    Malformed { path: PathBuf, detail: String },

    /// A format we know about but have not implemented yet. Principle 5: say so precisely.
    #[error("{path}: {format} support requires {tool}, which is not available yet (see PLAN.md)")]
    Unsupported {
        path: PathBuf,
        format: &'static str,
        tool: &'static str,
    },

    #[error("{path}: FLAC decode failed: {source}")]
    Flac {
        path: PathBuf,
        #[source]
        source: claxon::Error,
    },

    #[error("{path}: FLAC metadata read failed: {source}")]
    FlacMeta {
        path: PathBuf,
        #[source]
        source: metaflac::Error,
    },

    #[error("{path}: not valid bencode: {source}")]
    Bencode {
        path: PathBuf,
        #[source]
        source: bendy::decoding::Error,
    },

    #[error("{path}: not a usable torrent: {detail}")]
    Torrent { path: PathBuf, detail: String },

    /// Encoding has no file to blame yet — the torrent does not exist until it succeeds.
    #[error("cannot write a torrent: {detail}")]
    TorrentEncode { detail: String },

    /// Torrent paths are attacker-controlled; this is the zip-slip class of bug.
    #[error("{path}: refusing unsafe path in torrent: {detail}")]
    UnsafeTorrentPath { path: PathBuf, detail: String },

    /// A tracker we cannot turn into an announce URL that would work. Principle 5: name
    /// it, say what we know, and never quietly write a URL nobody will ever connect to.
    #[error("tracker {id}: {detail}")]
    UnusableTracker { id: String, detail: String },

    /// Principle 5: a WAV in a show folder is a normal thing to find, so say which format
    /// it is and what that format cannot hold, rather than "tagging failed".
    #[error("{path}: {format} carries no Vorbis comments; only FLAC can hold etree tags")]
    NotTaggable { path: PathBuf, format: &'static str },

    /// docs/tagging.md §1, contract point 2. A rename or a tag edit must not be able to
    /// change audio, and this is that promise failing rather than being trusted: it is a bug
    /// in us, and it stops the rest of the run instead of repeating across a show.
    #[error(
        "{path}: audio MD5 changed from {before} to {after} — a metadata write must never \
         touch audio; refusing to go on"
    )]
    AudioChanged {
        path: PathBuf,
        before: String,
        after: String,
    },

    /// Principle 1: v0.1 modifies nothing in place, so an existing output is a stop.
    #[error("{path} already exists; refusing to overwrite it")]
    OutputExists { path: PathBuf },

    /// Principle 5: name the tool, say what it was for, and list where we looked.
    #[error("{purpose} requires {tool}, which was not found (looked in {searched})")]
    ToolNotFound {
        tool: &'static str,
        purpose: &'static str,
        searched: String,
    },

    #[error("{tool} at {path} cannot be used: {detail}")]
    ToolUnusable {
        tool: &'static str,
        path: PathBuf,
        detail: String,
    },

    /// The tool ran and refused. `detail` is the tool's own words, not a paraphrase.
    #[error("{tool} failed ({status}): {detail}\n  argv: {argv}")]
    ToolFailed {
        tool: &'static str,
        status: String,
        argv: String,
        detail: String,
    },

    /// A sample clip that would not fit, or did not fit, the size it was meant to stay
    /// under (docs/sample.md §1). `detail` says which, and what would fit.
    #[error("{path}: {detail}")]
    SampleTooLarge { path: PathBuf, detail: String },

    #[error("{path}:{line}: malformed {kind} entry: {detail}")]
    ChecksumSyntax {
        path: PathBuf,
        line: usize,
        kind: &'static str,
        detail: String,
    },

    /// A job (see `job` module) was asked to stop before it produced anything durable.
    /// Not a bug — the caller asked for this — but distinct from every other failure so
    /// it can be told apart rather than reported as though something went wrong.
    #[error("cancelled before it finished")]
    Cancelled,
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
    pub fn malformed(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Error::Malformed {
            path: path.into(),
            detail: detail.into(),
        }
    }
    pub fn torrent(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Error::Torrent {
            path: path.into(),
            detail: detail.into(),
        }
    }
    pub fn unsafe_path(path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Error::UnsafeTorrentPath {
            path: path.into(),
            detail: detail.into(),
        }
    }
}
