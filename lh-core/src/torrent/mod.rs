//! Checking a local fileset against a `.torrent`, without a BitTorrent client.
//!
//! See `docs/torrent-verification.md`. The shape of this module is dictated by one fact:
//! pieces do not align with files. Everything is concatenated into a single stream before
//! being cut into pieces, so per-file status is derived rather than measured.

pub mod layout;
pub mod metainfo;
pub mod report;

pub use layout::{check_sizes, join_checked, resolve_root};
pub use metainfo::{Metainfo, TorrentFile};
pub use report::{FileOutcome, FileStatus, TorrentReport, Verdict};
