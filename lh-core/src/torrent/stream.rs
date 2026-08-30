//! The concatenated stream a torrent is cut from.
//!
//! Every file is joined, in listed order, into one logical byte stream, and *that* is cut
//! into fixed-size pieces. Piece boundaries have nothing to do with file boundaries. Both
//! halves of the feature walk this stream — verification to compare piece hashes, creation
//! to produce them — so the walk lives here once rather than twice.

use crate::error::{Error, Result};
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub(crate) const READ_BUF: usize = 64 * 1024;

/// Where one file sits in the concatenated stream.
pub(crate) struct Span {
    pub index: usize,
    pub start: u64,
    pub end: u64,
}

/// Lay the files end to end. Zero-length files produce an empty span, which overlaps no
/// piece and so is simply never read — the case the original TLH got wrong.
pub(crate) fn build_spans(lengths: impl IntoIterator<Item = u64>) -> Vec<Span> {
    let mut offset = 0u64;
    lengths
        .into_iter()
        .enumerate()
        .map(|(index, length)| {
            let span = Span {
                index,
                start: offset,
                end: offset + length,
            };
            offset += length;
            span
        })
        .collect()
}

pub(crate) fn spans_overlapping(
    spans: &[Span],
    start: u64,
    end: u64,
) -> impl Iterator<Item = &Span> {
    spans.iter().filter(move |s| s.start < end && s.end > start)
}

/// BEP 47 padding contributes zero bytes to the stream and never exists on disk.
pub(crate) fn feed_zeros(hasher: &mut Sha1, mut len: u64, buf: &mut [u8]) {
    buf.fill(0);
    while len > 0 {
        let n = len.min(buf.len() as u64) as usize;
        hasher.update(&buf[..n]);
        len -= n as u64;
    }
}

/// Keeps the current file open. Pieces and their segments are walked in stream order, so
/// one handle is all we ever need.
#[derive(Default)]
pub(crate) struct SpanReader {
    open: Option<(usize, File)>,
}

impl SpanReader {
    pub fn read_into(
        &mut self,
        hasher: &mut Sha1,
        index: usize,
        path: &Path,
        offset: u64,
        mut len: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        if self.open.as_ref().map(|(i, _)| *i) != Some(index) {
            let file = File::open(path).map_err(|e| Error::io(path, e))?;
            self.open = Some((index, file));
        }
        let (_, file) = self.open.as_mut().expect("just set");
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io(path, e))?;
        while len > 0 {
            let want = len.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..want])
                .map_err(|e| Error::io(path, e))?;
            hasher.update(&buf[..want]);
            len -= want as u64;
        }
        Ok(())
    }
}
