//! Writing `.torrent` files, and the one rule that governs it.
//!
//! Verification computes the infohash from the `info` dictionary's *original* bytes, never
//! by re-encoding what it parsed. Creation is the same rule pointed the other way:
//!
//! > Compute the infohash from the bytes we are about to write, never from a second
//! > encoding of the same data.
//!
//! So `info` is encoded exactly once, into a buffer; that buffer is hashed, and the same
//! buffer is spliced verbatim into the outer dictionary. Encoding it twice — once for the
//! hash and once for the file — would let a one-byte difference give the torrent an
//! advertised identity that is not its real one, and nothing downstream would notice.
//!
//! Canonical form is not optional: keys sorted by raw byte value, no leading zeros in
//! integers. `bendy` enforces both, and `emit_and_sort_dict` sorts for us rather than
//! trusting us to write the keys in order — which holds right up until someone inserts a
//! new key in the wrong place and silently changes the infohash of every torrent we make.
//!
//! SHA-1 here for the same reason as on the read side: BitTorrent v1 specifies it.

use super::metainfo::TorrentFile;
use crate::error::{Error, Result};
use bendy::encoding::{AsString, Encoder, Error as BencodeError, SingleItemEncoder, ToBencode};
use sha1::{Digest, Sha1};

/// What the torrent describes: one file, or a directory of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// `info.length`. `name` is the file itself, and there is no root directory.
    Single { length: u64 },
    /// `info.files`, in stream order. Padding included and flagged, as when parsing.
    Multi { files: Vec<TorrentFile> },
}

/// Everything needed to write a torrent. The piece hashes are an input: computing them is
/// a walk over the payload, which belongs to the caller, not to the encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    pub content: Content,
    /// BEP 27. Inside `info`, so it changes the infohash.
    pub private: bool,
    /// Inside `info` too, for the private sites that require it.
    pub source: Option<String>,
    /// Tracker tiers (BEP 12). Empty means a trackerless torrent, which is legal.
    pub announce: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    /// Epoch seconds, UTC. The original Trader's Little Helper shipped this as local time
    /// and had to fix it; the type does not stop us repeating that, so the name says it.
    pub creation_date: Option<i64>,
}

/// A torrent that has been encoded but not yet written.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    /// SHA-1 of the `info` dictionary as encoded inside `bytes`, not of a second encoding.
    pub info_hash: [u8; 20],
}

impl Draft {
    pub fn total_length(&self) -> u64 {
        match &self.content {
            Content::Single { length } => *length,
            Content::Multi { files } => files.iter().map(|f| f.length).sum(),
        }
    }
}

/// Encode a draft into the bytes of a `.torrent`, with the infohash taken from those bytes.
pub fn encode(draft: &Draft) -> Result<Encoded> {
    validate(draft)?;
    let info = encode_info(draft).map_err(bad)?;
    let info_hash: [u8; 20] = Sha1::digest(&info).into();
    let bytes = encode_outer(draft, &info).map_err(bad)?;
    Ok(Encoded { bytes, info_hash })
}

/// Encode only the `info` dictionary. Exposed because the infohash is a question people ask
/// on its own, and because it is what the round-trip test hashes.
pub fn info_bytes(draft: &Draft) -> Result<Vec<u8>> {
    validate(draft)?;
    encode_info(draft).map_err(bad)
}

/// Refuse to write anything our own parser would then reject. The alternative is producing
/// a file that looks fine until someone tries to seed it.
fn validate(draft: &Draft) -> Result<()> {
    let bad = |detail: String| Err(Error::TorrentEncode { detail });

    if draft.name.is_empty() {
        return bad("a torrent needs a name".into());
    }
    if draft.piece_length == 0 {
        return bad("piece length cannot be zero".into());
    }

    let total = draft.total_length();
    if total == 0 {
        return bad(format!("{} has no data in it to seed", draft.name));
    }
    let expected = total.div_ceil(draft.piece_length) as usize;
    if draft.pieces.len() != expected {
        return bad(format!(
            "{} piece hashes for {total} bytes at {} per piece; expected {expected}",
            draft.pieces.len(),
            draft.piece_length
        ));
    }

    if let Content::Multi { files } = &draft.content {
        if files.is_empty() {
            return bad(format!("{} lists no files", draft.name));
        }
        for f in files {
            if f.path.is_empty() {
                return bad("a file entry has an empty path".into());
            }
        }
    }
    Ok(())
}

fn encode_info(draft: &Draft) -> std::result::Result<Vec<u8>, BencodeError> {
    // `pieces` is one flat byte string of 20-byte hashes, not a list of them.
    let mut pieces = Vec::with_capacity(draft.pieces.len() * 20);
    for p in &draft.pieces {
        pieces.extend_from_slice(p);
    }

    let mut encoder = Encoder::new();
    encoder.emit_and_sort_dict(|e| {
        if let Content::Multi { files } = &draft.content {
            e.emit_pair_with(b"files", |e| {
                e.emit_list(|list| {
                    for f in files {
                        list.emit_with(|e| {
                            // `emit_unsorted_dict` is the sorting one on a value encoder.
                            e.emit_unsorted_dict(|e| {
                                if f.is_pad {
                                    // BEP 47: the authoritative marker for padding.
                                    e.emit_pair(b"attr", "p")?;
                                }
                                e.emit_pair(b"length", f.length)?;
                                e.emit_pair_with(b"path", |e| {
                                    e.emit_list(|p| {
                                        for component in &f.path {
                                            p.emit_str(component)?;
                                        }
                                        Ok(())
                                    })
                                })
                            })
                        })?;
                    }
                    Ok(())
                })
            })?;
        }
        if let Content::Single { length } = &draft.content {
            e.emit_pair(b"length", *length)?;
        }
        e.emit_pair(b"name", &draft.name)?;
        e.emit_pair(b"piece length", draft.piece_length)?;
        e.emit_pair(b"pieces", AsString(&pieces))?;
        if draft.private {
            e.emit_pair(b"private", 1)?;
        }
        if let Some(source) = &draft.source {
            e.emit_pair(b"source", source)?;
        }
        Ok(())
    })?;
    encoder.get_output()
}

/// The outer dictionary, with `info` spliced in as the exact bytes we already hashed.
fn encode_outer(draft: &Draft, info: &[u8]) -> std::result::Result<Vec<u8>, BencodeError> {
    let mut dict = RawDict::default();

    // BEP 3's single tracker is the first URL of the first tier; BEP 12's list carries all
    // of them. A trackerless torrent has neither key, which is legal and is what you want
    // for a DHT-only or private-archive torrent.
    if let Some(first) = draft.announce.first().and_then(|tier| tier.first()) {
        dict.put(b"announce", first)?;
    }
    if !draft.announce.is_empty() {
        let tiers = encoded(|e| {
            e.emit_list(|list| {
                for tier in &draft.announce {
                    list.emit_with(|e| {
                        e.emit_list(|urls| {
                            for url in tier {
                                urls.emit_str(url)?;
                            }
                            Ok(())
                        })
                    })?;
                }
                Ok(())
            })
        })?;
        dict.put_raw(b"announce-list", tiers);
    }
    if let Some(comment) = &draft.comment {
        dict.put(b"comment", comment)?;
    }
    if let Some(created_by) = &draft.created_by {
        dict.put(b"created by", created_by)?;
    }
    if let Some(date) = draft.creation_date {
        dict.put(b"creation date", date)?;
    }
    dict.put_raw(b"info", info.to_vec());

    Ok(dict.finish())
}

/// A bencode dictionary assembled from values that are already encoded.
///
/// The outer dictionary has to carry `info` as the exact byte sequence the infohash was
/// taken over, and bendy has no way to splice pre-encoded bytes into a dictionary it is
/// building. So this one is assembled here. The values still come from bendy, and the keys
/// are *sorted* rather than trusted to have been written in order — the same guarantee
/// `emit_and_sort_dict` gives, for the same reason.
#[derive(Default)]
struct RawDict {
    pairs: Vec<(&'static [u8], Vec<u8>)>,
}

impl RawDict {
    fn put<E: ToBencode>(
        &mut self,
        key: &'static [u8],
        value: E,
    ) -> std::result::Result<(), BencodeError> {
        let bytes = encoded(|e| value.encode(e))?;
        self.put_raw(key, bytes);
        Ok(())
    }

    fn put_raw(&mut self, key: &'static [u8], encoded_value: Vec<u8>) {
        debug_assert!(
            !self.pairs.iter().any(|(k, _)| *k == key),
            "duplicate key in a bencode dictionary"
        );
        self.pairs.push((key, encoded_value));
    }

    fn finish(mut self) -> Vec<u8> {
        self.pairs.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = vec![b'd'];
        for (key, value) in &self.pairs {
            out.extend_from_slice(key.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(key);
            out.extend_from_slice(value);
        }
        out.push(b'e');
        out
    }
}

/// Encode one value on its own.
fn encoded<F>(value_cb: F) -> std::result::Result<Vec<u8>, BencodeError>
where
    F: FnOnce(SingleItemEncoder) -> std::result::Result<(), BencodeError>,
{
    let mut encoder = Encoder::new();
    encoder.emit_with(value_cb)?;
    encoder.get_output()
}

fn bad(source: BencodeError) -> Error {
    Error::TorrentEncode {
        detail: source.to_string(),
    }
}
