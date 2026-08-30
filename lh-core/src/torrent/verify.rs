//! Verifying a local fileset against a torrent's piece hashes.
//!
//! The whole shape of this module comes from one fact: piece boundaries have nothing to do
//! with file boundaries. Every file is concatenated into one logical stream, and that stream
//! is cut into fixed-size pieces, so a piece routinely spans two files. Per-file status is
//! therefore *derived* from piece results, and when a shared piece fails the data genuinely
//! does not say which of its files is at fault.

use super::layout;
use super::metainfo::Metainfo;
use super::report::{FileStatus, PieceCounts, TorrentReport};
use super::stream::{READ_BUF, Span, SpanReader, build_spans, feed_zeros, spans_overlapping};
use crate::error::Result;
use sha1::{Digest, Sha1};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PieceOutcome {
    Ok,
    Failed,
    /// A file this piece covers is missing or the wrong size, so there is nothing to hash.
    /// This is not the same as failing, and must never be reported as corruption.
    Unverifiable,
}

pub fn check(meta: &Metainfo, torrent_path: &Path, given: &Path) -> Result<TorrentReport> {
    check_with_progress(meta, torrent_path, given, &mut |_, _| {})
}

/// `progress` is called with (pieces done, pieces total) as the stream is walked.
pub fn check_with_progress(
    meta: &Metainfo,
    torrent_path: &Path,
    given: &Path,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<TorrentReport> {
    // The size pre-check comes first: a file of the wrong length will fail hashing anyway,
    // and knowing that up front is what lets us mark its pieces unverifiable rather than
    // reading garbage and calling neighbouring files corrupt.
    let mut report = layout::check_sizes(meta, torrent_path, given)?;

    let spans = build_spans(meta.files.iter().map(|f| f.length));
    let readable: Vec<bool> = report
        .files
        .iter()
        .map(|f| matches!(f.status, FileStatus::SizeOk | FileStatus::Padding))
        .collect();

    let total_pieces = meta.pieces.len() as u32;
    let mut outcomes = vec![PieceOutcome::Unverifiable; meta.pieces.len()];
    let mut reader = SpanReader::default();
    let mut buf = vec![0u8; READ_BUF];

    for (piece_index, expected) in meta.pieces.iter().enumerate() {
        let piece_start = piece_index as u64 * meta.piece_length;
        let piece_end = (piece_start + meta.piece_length).min(meta.total_length);

        let mut hasher = Sha1::new();
        let mut usable = true;

        for span in spans_overlapping(&spans, piece_start, piece_end) {
            let from = piece_start.max(span.start);
            let to = piece_end.min(span.end);
            let len = to - from;
            if len == 0 {
                continue;
            }
            if !readable[span.index] {
                usable = false;
                break;
            }
            if meta.files[span.index].is_pad {
                // Padding contributes zero bytes to the stream and never exists on disk.
                feed_zeros(&mut hasher, len, &mut buf);
                continue;
            }
            let path = &report.files[span.index].path;
            match reader.read_into(
                &mut hasher,
                span.index,
                path,
                from - span.start,
                len,
                &mut buf,
            ) {
                Ok(()) => {}
                Err(e) => {
                    // A file that stat'd fine but will not read: record it against that file
                    // and treat every piece touching it as unverifiable.
                    report.files[span.index].status = FileStatus::Unreadable {
                        reason: e.to_string(),
                    };
                    usable = false;
                    break;
                }
            }
        }

        outcomes[piece_index] = if !usable {
            PieceOutcome::Unverifiable
        } else if hasher.finalize().as_slice() == expected.as_slice() {
            PieceOutcome::Ok
        } else {
            PieceOutcome::Failed
        };
        progress(piece_index as u32 + 1, total_pieces);
    }

    attribute(meta, &spans, &outcomes, &mut report);

    let mut counts = PieceCounts {
        total: total_pieces,
        ..Default::default()
    };
    for o in &outcomes {
        match o {
            PieceOutcome::Ok => counts.ok += 1,
            PieceOutcome::Failed => counts.failed += 1,
            PieceOutcome::Unverifiable => counts.unverifiable += 1,
        }
    }
    report.pieces = Some(counts);
    report.quick = false;
    Ok(report)
}

/// Turn per-piece results into per-file status.
///
/// The rule that matters: a failed piece lying wholly inside one file convicts that file,
/// while a failed piece shared between two files convicts neither. Naming one of them would
/// send somebody re-downloading the wrong file.
fn attribute(
    meta: &Metainfo,
    spans: &[Span],
    outcomes: &[PieceOutcome],
    report: &mut TorrentReport,
) {
    for (file_index, file) in meta.files.iter().enumerate() {
        if file.is_pad || report.files[file_index].status != FileStatus::SizeOk {
            continue;
        }
        let span = &spans[file_index];

        let mut exclusive_failed = Vec::new();
        let mut shared_failed: Option<(u32, Vec<usize>)> = None;
        let mut verified = 0u32;
        let mut unverifiable = 0u32;

        for (piece_index, outcome) in outcomes.iter().enumerate() {
            let piece_start = piece_index as u64 * meta.piece_length;
            let piece_end = (piece_start + meta.piece_length).min(meta.total_length);
            if piece_start >= span.end || piece_end <= span.start {
                continue;
            }
            match outcome {
                PieceOutcome::Ok => verified += 1,
                PieceOutcome::Unverifiable => unverifiable += 1,
                PieceOutcome::Failed => {
                    // Which other real files does this piece touch? Padding is ours, not
                    // the user's, so it never shares blame.
                    let others: Vec<usize> = spans_overlapping(spans, piece_start, piece_end)
                        .filter(|s| s.index != file_index && !meta.files[s.index].is_pad)
                        .filter(|s| s.end > s.start)
                        .map(|s| s.index)
                        .collect();
                    if others.is_empty() {
                        exclusive_failed.push(piece_index as u32);
                    } else if shared_failed.is_none() {
                        shared_failed = Some((piece_index as u32, others));
                    }
                }
            }
        }

        report.files[file_index].status = if !exclusive_failed.is_empty() {
            FileStatus::Corrupt {
                bad_pieces: exclusive_failed,
            }
        } else if let Some((piece, shared_with)) = shared_failed {
            FileStatus::Suspect { piece, shared_with }
        } else if unverifiable > 0 {
            FileStatus::Partial {
                verified,
                unverifiable,
            }
        } else {
            // Includes zero-length files, which no piece overlaps.
            FileStatus::Complete
        };
    }
}
