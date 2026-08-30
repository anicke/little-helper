//! The registry of external reference binaries.
//!
//! Principle 3 splits the work in two. Read-only analysis — probing, checksums, SBE,
//! verification — is pure Rust, because nobody cares what *read* a file, only that the
//! answer is right. Anything that *produces* a file people will trade goes through the
//! reference tool instead, so the FLAC vendor string reads `reference libFLAC x.y.z`.
//!
//! This module is the second half of that split, and it is a headline feature rather than
//! plumbing: it is the traceability story. Every tool is discovered once, its own version
//! output and the SHA-256 of its binary are captured, and every operation that runs one
//! records tool, version, hash and the exact argv (Principle 2, and see [`Provenance`]).

pub mod runner;

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use runner::{Agent, Provenance, run};

/// An external binary we know how to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolId {
    Flac,
    Metaflac,
    Shntool,
}

impl ToolId {
    /// Discovery order everywhere: the required tool first.
    pub const ALL: [ToolId; 3] = [Self::Flac, Self::Metaflac, Self::Shntool];

    pub fn name(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Metaflac => "metaflac",
            Self::Shntool => "shntool",
        }
    }

    /// What we would use it for. This goes into the "not found" message, so the user
    /// learns what they lose rather than merely that a file is absent (Principle 5).
    pub fn purpose(self) -> &'static str {
        match self {
            Self::Flac => "encoding WAV to FLAC",
            Self::Metaflac => "tag editing",
            Self::Shntool => "SHN support",
        }
    }

    /// Whether v0.1 needs it. `flac` alone: it is how the vendor string stays
    /// `reference libFLAC`. The other two belong to deferred features.
    pub fn is_required(self) -> bool {
        matches!(self, Self::Flac)
    }

    /// The environment variable that overrides discovery, e.g. `LH_FLAC`.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Flac => "LH_FLAC",
            Self::Metaflac => "LH_METAFLAC",
            Self::Shntool => "LH_SHNTOOL",
        }
    }

    /// Not everything spells it `--version`; shntool wants `-v`.
    fn version_args(self) -> &'static [&'static str] {
        match self {
            Self::Shntool => &["-v"],
            _ => &["--version"],
        }
    }

    fn file_name(self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.name())
        } else {
            self.name().to_string()
        }
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Where a tool came from. Shown to the user, because "the one I built myself" and "the
/// one you shipped" are answers a trader is entitled to tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// Pointed at by the user through the tool's environment variable. Always wins.
    Configured,
    /// A sidecar shipped next to our own executable.
    Bundled,
    /// Found on `PATH`.
    Path,
}

impl ToolSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Bundled => "bundled",
            Self::Path => "PATH",
        }
    }
}

/// A discovered binary, interrogated once at startup.
#[derive(Debug, Clone)]
pub struct Tool {
    pub id: ToolId,
    pub path: PathBuf,
    pub source: ToolSource,
    /// The first line of the tool's own version output, verbatim. We do not parse it:
    /// the point is to record what it said, not what we understood it to mean.
    pub version: String,
    /// Hex SHA-256 of the binary, so a user can confirm they are running the build we
    /// published — or prove they are running their own.
    pub sha256: String,
}

/// What discovery found, including the ways it can fail. A tool that is present but
/// unusable is a different problem from one that is absent, and says so.
#[derive(Debug, Clone)]
pub enum Discovery {
    Found(Tool),
    /// Absent. Carries everywhere we looked, so the user can fix it.
    NotFound {
        searched: Vec<String>,
    },
    /// Present but unusable — unreadable, or it would not report a version.
    Unusable {
        path: PathBuf,
        reason: String,
    },
}

/// Explicit paths for tools the user has pointed at themselves.
pub type ToolPaths = BTreeMap<ToolId, PathBuf>;

/// Every tool we know about and what became of it.
#[derive(Debug, Clone)]
pub struct Registry {
    entries: BTreeMap<ToolId, Discovery>,
}

impl Registry {
    /// Look for every known tool. Runs each one once for its version and hashes its
    /// binary, so this costs a few milliseconds — do it at startup, not per file.
    pub fn discover() -> Self {
        Self::discover_with(&ToolPaths::new())
    }

    /// Discovery with explicit paths for some tools — the "point at your own build"
    /// case. An override that is not there is an error rather than a reason to fall
    /// back: a user who names a binary means that binary (Principle 5).
    pub fn discover_with(overrides: &ToolPaths) -> Self {
        Self {
            entries: ToolId::ALL
                .into_iter()
                .map(|id| (id, discover(id, overrides.get(&id).map(PathBuf::as_path))))
                .collect(),
        }
    }

    /// A registry containing exactly one tool, for a command that needs only that one.
    pub fn discover_one(id: ToolId) -> Self {
        Self {
            entries: BTreeMap::from([(id, discover(id, None))]),
        }
    }

    /// The tool, or an error that names it, says what it was for and lists where we
    /// looked (Principle 5).
    pub fn require(&self, id: ToolId) -> Result<&Tool> {
        match self.entries.get(&id) {
            Some(Discovery::Found(t)) => Ok(t),
            Some(Discovery::Unusable { path, reason }) => Err(Error::ToolUnusable {
                tool: id.name(),
                path: path.clone(),
                detail: reason.clone(),
            }),
            Some(Discovery::NotFound { searched }) => Err(Error::ToolNotFound {
                tool: id.name(),
                purpose: id.purpose(),
                searched: searched.join(", "),
            }),
            None => Err(Error::ToolNotFound {
                tool: id.name(),
                purpose: id.purpose(),
                searched: "nowhere — this tool was not looked for".into(),
            }),
        }
    }

    /// Every entry, in `ToolId` order.
    pub fn entries(&self) -> impl Iterator<Item = (ToolId, &Discovery)> {
        self.entries.iter().map(|(id, d)| (*id, d))
    }

    /// Required tools that are not usable. Empty means the install is complete.
    pub fn missing_required(&self) -> impl Iterator<Item = ToolId> + '_ {
        self.entries
            .iter()
            .filter(|(id, d)| id.is_required() && !matches!(d, Discovery::Found(_)))
            .map(|(id, _)| *id)
    }
}

fn discover(id: ToolId, override_path: Option<&Path>) -> Discovery {
    let (path, source) = match locate(id, override_path) {
        Ok(found) => found,
        Err(searched) => return Discovery::NotFound { searched },
    };
    let version = match capture_version(id, &path) {
        Ok(v) => v,
        Err(reason) => return Discovery::Unusable { path, reason },
    };
    let sha256 = match hash_file(&path) {
        Ok(h) => h,
        Err(e) => {
            return Discovery::Unusable {
                path,
                reason: e.to_string(),
            };
        }
    };
    Discovery::Found(Tool {
        id,
        path,
        source,
        version,
        sha256,
    })
}

/// User-configured path, then bundled sidecar, then `PATH` — the order in §3 of PLAN.md.
/// On failure, returns everywhere we looked so the message can say so.
fn locate(
    id: ToolId,
    override_path: Option<&Path>,
) -> std::result::Result<(PathBuf, ToolSource), Vec<String>> {
    // A configured path is the whole search. Quietly falling back to some other `flac`
    // when the named one is absent would make the provenance record a lie.
    let configured = match override_path {
        Some(p) => Some((p.to_path_buf(), p.display().to_string())),
        None => std::env::var_os(id.env_var()).map(|v| {
            let p = PathBuf::from(v);
            let label = format!("{} (from {})", p.display(), id.env_var());
            (p, label)
        }),
    };
    if let Some((path, label)) = configured {
        return if path.is_file() {
            Ok((path, ToolSource::Configured))
        } else {
            Err(vec![label])
        };
    }

    let mut searched = Vec::new();
    if let Some(dir) = sidecar_dir() {
        let path = dir.join(id.file_name());
        searched.push(path.display().to_string());
        if path.is_file() {
            return Ok((path, ToolSource::Bundled));
        }
    }

    match which::which(id.name()) {
        Ok(path) => Ok((path, ToolSource::Path)),
        Err(_) => {
            searched.push(format!("{} on PATH", id.name()));
            Err(searched)
        }
    }
}

/// Sidecars ship in a `tools/` directory beside our own executable, so a portable
/// install carries its own reference binaries.
fn sidecar_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("tools"))
}

/// Ask the tool what it is. Stdin is closed: a binary that turns out not to be the tool
/// we expected must fail rather than sit waiting for input.
fn capture_version(id: ToolId, path: &Path) -> std::result::Result<String, String> {
    let output = Command::new(path)
        .args(id.version_args())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run it: {e}"))?;

    // Version output goes to stdout for some tools and stderr for others; take whichever
    // spoke. We record the line verbatim rather than parsing a version number out of it.
    let line = first_line(&output.stdout).or_else(|| first_line(&output.stderr));
    line.ok_or_else(|| {
        format!(
            "ran `{} {}` but it printed no version",
            path.display(),
            id.version_args().join(" ")
        )
    })
}

fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn hash_file(path: &Path) -> Result<String> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut r = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}
