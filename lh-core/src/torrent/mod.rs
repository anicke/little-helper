//! Checking a local fileset against a `.torrent`, without a BitTorrent client.
//!
//! See `docs/torrent-verification.md`. The shape of this module is dictated by one fact:
//! pieces do not align with files. Everything is concatenated into a single stream before
//! being cut into pieces, so per-file status is derived rather than measured.

pub mod encode;
pub mod layout;
pub mod metainfo;
pub mod report;
pub mod verify;

pub use encode::{Content, Draft, Encoded, encode, info_bytes};
pub use layout::{check_sizes, join_checked, resolve_root};
pub use metainfo::{Metainfo, TorrentFile};
pub use report::{FileOutcome, FileStatus, PieceCounts, TorrentReport, Verdict};
pub use verify::{check, check_with_progress};
