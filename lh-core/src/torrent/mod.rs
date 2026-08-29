//! Checking a local fileset against a `.torrent`, without a BitTorrent client.
//!
//! See `docs/torrent-verification.md`. The shape of this module is dictated by one fact:
//! pieces do not align with files. Everything is concatenated into a single stream before
//! being cut into pieces, so per-file status is derived rather than measured.

pub mod metainfo;

pub use metainfo::{Metainfo, TorrentFile};
