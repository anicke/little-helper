//! Parsing `.torrent` files (BEP 3), and computing the infohash.

use crate::error::{Error, Result};
use bendy::decoding::{Decoder, Object};
use sha1::{Digest, Sha1};
use std::path::Path;

/// Guards against a hostile file claiming an absurd piece size. Real torrents run from
/// 16 KiB to 16 MiB; anything past a gibibyte is not a torrent we want to allocate for.
const MAX_PIECE_LENGTH: u64 = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    /// Validated path components, relative to the torrent root.
    pub path: Vec<String>,
    pub length: u64,
    /// BEP 47 padding: zero bytes contributed to the stream that never exist on disk.
    pub is_pad: bool,
}

impl TorrentFile {
    pub fn display_path(&self) -> String {
        self.path.join("/")
    }
}

#[derive(Debug, Clone)]
pub struct Metainfo {
    /// SHA-1 of the bencoded `info` dictionary, taken from its original bytes.
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    /// Every file in stream order, padding included. Padding is flagged, never hidden.
    pub files: Vec<TorrentFile>,
    pub total_length: u64,
    /// Single-file torrents have no root directory; `name` is the file itself.
    pub is_single_file: bool,
    /// BEP 27. Lives inside `info`, so it is part of the infohash: a torrent cannot be
    /// made private after the fact, only made into a different torrent.
    pub private: bool,
    /// Some private sites require this to make the infohash uniquely theirs.
    pub source: Option<String>,
    /// Tracker tiers (BEP 12). Clients choose at random within a tier and fall through
    /// between tiers, so the nesting carries meaning and is kept rather than flattened.
    pub announce: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
}

impl Metainfo {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        Self::from_bytes(&bytes, path)
    }

    pub fn info_hash_hex(&self) -> String {
        hex::encode(self.info_hash)
    }

    /// Files that actually exist on disk — padding excluded.
    pub fn real_files(&self) -> impl Iterator<Item = &TorrentFile> {
        self.files.iter().filter(|f| !f.is_pad)
    }

    /// Every tracker, tiers flattened, for callers that only want to list them.
    pub fn trackers(&self) -> impl Iterator<Item = &str> {
        self.announce.iter().flatten().map(String::as_str)
    }

    pub fn from_bytes(bytes: &[u8], origin: &Path) -> Result<Self> {
        let mut decoder = Decoder::new(bytes);
        let object = decoder
            .next_object()
            .map_err(|source| Error::Bencode {
                path: origin.into(),
                source,
            })?
            .ok_or_else(|| Error::torrent(origin, "file is empty"))?;
        let mut top = object
            .try_into_dictionary()
            .map_err(|source| Error::Bencode {
                path: origin.into(),
                source,
            })?;

        let mut announce: Option<String> = None;
        let mut tiers: Vec<Vec<String>> = Vec::new();
        let mut comment = None;
        let mut created_by = None;
        let mut creation_date = None;
        let mut info = None;

        while let Some((key, value)) = top.next_pair().map_err(|source| Error::Bencode {
            path: origin.into(),
            source,
        })? {
            match key {
                b"announce" => announce = Some(text(value, origin)?),
                b"announce-list" => {
                    // A list of tiers, each a list of URLs.
                    let mut list = value.try_into_list().map_err(|source| Error::Bencode {
                        path: origin.into(),
                        source,
                    })?;
                    while let Some(tier) = list.next_object().map_err(|source| Error::Bencode {
                        path: origin.into(),
                        source,
                    })? {
                        let mut urls = tier.try_into_list().map_err(|source| Error::Bencode {
                            path: origin.into(),
                            source,
                        })?;
                        let mut group = Vec::new();
                        while let Some(url) =
                            urls.next_object().map_err(|source| Error::Bencode {
                                path: origin.into(),
                                source,
                            })?
                        {
                            add_tracker(&mut group, text(url, origin)?);
                        }
                        if !group.is_empty() {
                            tiers.push(group);
                        }
                    }
                }
                b"comment" => comment = Some(text(value, origin)?),
                b"created by" => created_by = Some(text(value, origin)?),
                b"creation date" => creation_date = Some(integer(value, origin, "creation date")?),
                b"info" => info = Some(parse_info(value, origin)?),
                _ => {}
            }
        }

        let info = info.ok_or_else(|| Error::torrent(origin, "no info dictionary"))?;

        // BEP 12 says `announce` should also appear in `announce-list`, and real torrents
        // put it there. Only prepend it as its own tier when it is genuinely absent, so a
        // well-formed torrent does not grow a duplicate tracker on the way in.
        if let Some(url) = announce.filter(|u| !u.is_empty())
            && !tiers.iter().flatten().any(|t| *t == url)
        {
            tiers.insert(0, vec![url]);
        }

        Ok(Self {
            info_hash: info.info_hash,
            name: info.name,
            piece_length: info.piece_length,
            pieces: info.pieces,
            files: info.files,
            total_length: info.total_length,
            is_single_file: info.is_single_file,
            private: info.private,
            source: info.source,
            announce: tiers,
            comment,
            created_by,
            creation_date,
        })
    }
}

fn add_tracker(list: &mut Vec<String>, url: String) {
    if !url.is_empty() && !list.contains(&url) {
        list.push(url);
    }
}

struct InfoParts {
    info_hash: [u8; 20],
    name: String,
    piece_length: u64,
    pieces: Vec<[u8; 20]>,
    files: Vec<TorrentFile>,
    total_length: u64,
    is_single_file: bool,
    private: bool,
    source: Option<String>,
}

fn parse_info(value: Object<'_, '_>, origin: &Path) -> Result<InfoParts> {
    let mut dict = value
        .try_into_dictionary()
        .map_err(|source| Error::Bencode {
            path: origin.into(),
            source,
        })?;

    let mut name = None;
    let mut piece_length = None;
    let mut pieces_raw: Option<&[u8]> = None;
    let mut single_length: Option<u64> = None;
    let mut files: Option<Vec<TorrentFile>> = None;
    let mut meta_version: Option<i64> = None;
    let mut private = false;
    let mut source = None;

    while let Some((key, value)) = dict.next_pair().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })? {
        match key {
            b"name" => name = Some(text(value, origin)?),
            b"piece length" => {
                piece_length = Some(unsigned(value, origin, "piece length")?);
            }
            b"pieces" => {
                pieces_raw = Some(value.try_into_bytes().map_err(|source| Error::Bencode {
                    path: origin.into(),
                    source,
                })?);
            }
            b"length" => single_length = Some(unsigned(value, origin, "length")?),
            b"files" => files = Some(parse_files(value, origin)?),
            b"meta version" => meta_version = Some(integer(value, origin, "meta version")?),
            // BEP 27 writes 1; treat any non-zero as private rather than only the literal.
            b"private" => private = integer(value, origin, "private")? != 0,
            b"source" => source = Some(text(value, origin)?),
            _ => {}
        }
    }

    // The infohash must come from the original bytes. Re-encoding what we parsed would
    // silently produce a different hash for any non-canonical torrent.
    let raw = dict.into_raw().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })?;
    let info_hash: [u8; 20] = Sha1::digest(raw).into();

    // Check this before the missing-key errors below: a pure v2 torrent has no `pieces`
    // at all, and "no pieces" would be a much worse thing to tell the user than "v2".
    // Hybrid torrents carry both and are handled as v1.
    if meta_version.is_some_and(|v| v >= 2) && pieces_raw.is_none() {
        return Err(Error::torrent(
            origin,
            "this is a BitTorrent v2 torrent, which is not supported yet",
        ));
    }

    let name = name.ok_or_else(|| Error::torrent(origin, "info dictionary has no name"))?;
    validate_component(&name, origin, "name")?;

    let piece_length = piece_length
        .ok_or_else(|| Error::torrent(origin, "info dictionary has no piece length"))?;
    if piece_length == 0 || piece_length > MAX_PIECE_LENGTH {
        return Err(Error::torrent(
            origin,
            format!("implausible piece length {piece_length}"),
        ));
    }

    let pieces_raw =
        pieces_raw.ok_or_else(|| Error::torrent(origin, "info dictionary has no pieces"))?;
    if pieces_raw.len() % 20 != 0 {
        return Err(Error::torrent(
            origin,
            format!("pieces is {} bytes, not a multiple of 20", pieces_raw.len()),
        ));
    }
    let pieces: Vec<[u8; 20]> = pieces_raw
        .chunks_exact(20)
        .map(|c| {
            let mut a = [0u8; 20];
            a.copy_from_slice(c);
            a
        })
        .collect();

    let (files, total_length, is_single_file) = match (single_length, files) {
        (Some(_), Some(_)) => {
            return Err(Error::torrent(
                origin,
                "info dictionary has both length and files",
            ));
        }
        (Some(length), None) => (
            vec![TorrentFile {
                path: vec![name.clone()],
                length,
                is_pad: false,
            }],
            length,
            true,
        ),
        (None, Some(files)) => {
            let mut total: u64 = 0;
            for f in &files {
                total = total.checked_add(f.length).ok_or_else(|| {
                    Error::torrent(origin, "total length overflows a 64-bit integer")
                })?;
            }
            (files, total, false)
        }
        (None, None) => {
            return Err(Error::torrent(
                origin,
                "info dictionary has neither length nor files",
            ));
        }
    };

    // The stream is the concatenation of every file, cut into pieces. If the piece count
    // does not match, the two halves of this file disagree and we cannot verify anything.
    let expected = total_length.div_ceil(piece_length) as usize;
    if pieces.len() != expected {
        return Err(Error::torrent(
            origin,
            format!(
                "{} pieces for {total_length} bytes at {piece_length} per piece; expected {expected}",
                pieces.len()
            ),
        ));
    }

    Ok(InfoParts {
        info_hash,
        name,
        piece_length,
        pieces,
        files,
        total_length,
        is_single_file,
        private,
        source,
    })
}

fn parse_files(value: Object<'_, '_>, origin: &Path) -> Result<Vec<TorrentFile>> {
    let mut list = value.try_into_list().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })?;
    let mut out = Vec::new();

    while let Some(entry) = list.next_object().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })? {
        let mut dict = entry
            .try_into_dictionary()
            .map_err(|source| Error::Bencode {
                path: origin.into(),
                source,
            })?;

        let mut length = None;
        let mut path: Option<Vec<String>> = None;
        let mut is_pad = false;

        while let Some((key, value)) = dict.next_pair().map_err(|source| Error::Bencode {
            path: origin.into(),
            source,
        })? {
            match key {
                b"length" => length = Some(unsigned(value, origin, "file length")?),
                b"attr" => {
                    // BEP 47: "p" marks a padding file.
                    let attr = value.try_into_bytes().map_err(|source| Error::Bencode {
                        path: origin.into(),
                        source,
                    })?;
                    is_pad |= attr.contains(&b'p');
                }
                b"path" => {
                    let mut components = list_of_strings(value, origin)?;
                    if components.is_empty() {
                        return Err(Error::torrent(origin, "file entry has an empty path"));
                    }
                    for c in &components {
                        validate_component(c, origin, "path component")?;
                    }
                    components.shrink_to_fit();
                    path = Some(components);
                }
                _ => {}
            }
        }

        let length = length.ok_or_else(|| Error::torrent(origin, "file entry has no length"))?;
        let path = path.ok_or_else(|| Error::torrent(origin, "file entry has no path"))?;

        // BEP 47's `attr` is authoritative. These are the older naming conventions that
        // predate it — a heuristic, but the alternative is treating padding as a missing
        // file and reporting a good download as broken.
        if !is_pad {
            is_pad = path.first().is_some_and(|c| c == ".pad")
                || path
                    .last()
                    .is_some_and(|c| c.starts_with("_____padding_file"));
        }

        out.push(TorrentFile {
            path,
            length,
            is_pad,
        });
    }
    Ok(out)
}

/// Reject anything that could escape the torrent root when joined to a real directory.
///
/// Validation happens here, at parse time, so an unsafe `TorrentFile` can never be
/// constructed. Windows reserved device names are checked later, where the join happens.
fn validate_component(component: &str, origin: &Path, what: &str) -> Result<()> {
    let bad = |detail: String| Err(Error::unsafe_path(origin, detail));
    if component.is_empty() {
        return bad(format!("{what} is empty"));
    }
    if component == "." || component == ".." {
        return bad(format!("{what} is {component:?}"));
    }
    if component.contains('/') || component.contains('\\') {
        return bad(format!("{what} {component:?} contains a path separator"));
    }
    if component.contains('\0') {
        return bad(format!("{what} {component:?} contains a NUL byte"));
    }
    Ok(())
}

fn text(value: Object<'_, '_>, origin: &Path) -> Result<String> {
    let bytes = value.try_into_bytes().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn list_of_strings(value: Object<'_, '_>, origin: &Path) -> Result<Vec<String>> {
    let mut list = value.try_into_list().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })?;
    let mut out = Vec::new();
    while let Some(item) = list.next_object().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })? {
        out.push(text(item, origin)?);
    }
    Ok(out)
}

fn integer(value: Object<'_, '_>, origin: &Path, what: &str) -> Result<i64> {
    let raw = value.try_into_integer().map_err(|source| Error::Bencode {
        path: origin.into(),
        source,
    })?;
    raw.parse::<i64>()
        .map_err(|_| Error::torrent(origin, format!("{what} {raw:?} is not an integer")))
}

fn unsigned(value: Object<'_, '_>, origin: &Path, what: &str) -> Result<u64> {
    let n = integer(value, origin, what)?;
    u64::try_from(n).map_err(|_| Error::torrent(origin, format!("{what} is negative: {n}")))
}
