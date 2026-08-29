//! Reading and writing `.ffp`, `.md5` and `.st5` sidecar files.

use super::ChecksumKind;
use crate::error::{Error, Result};
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub file_name: String,
    pub digest: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumFile {
    pub kind: ChecksumKind,
    pub entries: Vec<Entry>,
}

impl ChecksumFile {
    pub fn new(kind: ChecksumKind) -> Self {
        Self {
            kind,
            entries: Vec::new(),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            let hex = hex::encode(e.digest);
            match self.kind {
                // "name:hash"
                ChecksumKind::Ffp => {
                    let _ = writeln!(out, "{}:{}", e.file_name, hex);
                }
                // md5sum-compatible: "hash  name" (two spaces).
                ChecksumKind::Md5 | ChecksumKind::St5 => {
                    let _ = writeln!(out, "{}  {}", hex, e.file_name);
                }
            }
        }
        out
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.render()).map_err(|e| Error::io(path, e))
    }

    pub fn read(kind: ChecksumKind, path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::parse(kind, &text, path)
    }

    /// Tolerant of what is actually out in the wild: CRLF, a UTF-8 BOM, `;`/`#` comment
    /// lines, blank lines, and both `hash *name` (binary) and `hash  name` (text) forms.
    pub fn parse(kind: ChecksumKind, text: &str, origin: &Path) -> Result<Self> {
        let mut entries = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim_start_matches('\u{feff}').trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            let lineno = i + 1;
            let entry = match kind {
                ChecksumKind::Ffp => parse_ffp_line(line, lineno, kind, origin)?,
                ChecksumKind::Md5 | ChecksumKind::St5 => {
                    parse_md5_line(line, lineno, kind, origin)?
                }
            };
            entries.push(entry);
        }
        Ok(Self { kind, entries })
    }
}

fn syntax(origin: &Path, line: usize, kind: ChecksumKind, detail: impl Into<String>) -> Error {
    Error::ChecksumSyntax {
        path: origin.to_path_buf(),
        line,
        kind: kind.label(),
        detail: detail.into(),
    }
}

fn digest_from_hex(s: &str, line: usize, kind: ChecksumKind, origin: &Path) -> Result<[u8; 16]> {
    let bytes = hex::decode(s)
        .map_err(|_| syntax(origin, line, kind, format!("{s:?} is not hexadecimal")))?;
    let arr: [u8; 16] = bytes
        .try_into()
        .map_err(|_| syntax(origin, line, kind, "digest is not 16 bytes"))?;
    Ok(arr)
}

/// `name:hash` — the filename may itself contain colons, so split at the last one.
fn parse_ffp_line(line: &str, lineno: usize, kind: ChecksumKind, origin: &Path) -> Result<Entry> {
    let (name, hex) = line
        .rsplit_once(':')
        .ok_or_else(|| syntax(origin, lineno, kind, "expected \"name:hash\""))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(syntax(origin, lineno, kind, "empty filename"));
    }
    Ok(Entry {
        file_name: name.to_string(),
        digest: digest_from_hex(hex.trim(), lineno, kind, origin)?,
    })
}

/// `hash  name` or `hash *name`.
fn parse_md5_line(line: &str, lineno: usize, kind: ChecksumKind, origin: &Path) -> Result<Entry> {
    let (hex, rest) = line
        .split_once(char::is_whitespace)
        .ok_or_else(|| syntax(origin, lineno, kind, "expected \"hash  name\""))?;
    let digest = digest_from_hex(hex.trim(), lineno, kind, origin)?;
    let name = rest
        .trim_start()
        .strip_prefix('*')
        .unwrap_or(rest.trim_start());
    let name = name.trim();
    if name.is_empty() {
        return Err(syntax(origin, lineno, kind, "empty filename"));
    }
    Ok(Entry {
        file_name: name.to_string(),
        digest,
    })
}
