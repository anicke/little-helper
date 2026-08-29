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

    #[error("{path}:{line}: malformed {kind} entry: {detail}")]
    ChecksumSyntax {
        path: PathBuf,
        line: usize,
        kind: &'static str,
        detail: String,
    },
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
}
