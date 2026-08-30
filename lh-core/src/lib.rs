//! Core engine for Little Helper.
//!
//! No UI and no CLI live here (Principle 4): everything the GUI can do, the CLI can do,
//! because both drive this crate.
//!
//! Two rules shape the module layout (Principle 3):
//!
//! * Read-only analysis — probing, checksums, SBE, verification — is pure Rust. Nobody
//!   cares what *read* a file, only that the answer is right.
//! * Anything that *produces* a file people will trade goes through the reference tool,
//!   so the FLAC vendor string stays `reference libFLAC x.y.z`. That lives in `tools`.

pub mod analysis;
pub mod checksum;
pub mod config;
pub mod convert;
pub mod error;
pub mod format;
pub mod job;
pub mod model;
pub mod output;
pub mod scan;
pub mod tools;
pub mod torrent;

pub use error::{Error, Result};
pub use model::{AudioFile, AudioFormat, StreamInfo};
