//! The GUI's one shared job queue and its `JobOutcome` — `docs/gui.md` §2.
//!
//! `job::Queue<T>` never imports an operation's result type (`docs/job-queue.md` §2); this
//! is where that coupling terminates for `lh-gui`. G2 wires the three operations that need
//! no per-run options: verify, checksum (any of the three kinds), sbe. Convert and the
//! torrent panels join this enum in G3/G4, per `docs/gui.md`'s milestone table — not added
//! ahead of a real caller.

use lh_core::analysis::{Sbe, Verification};
use lh_core::checksum::ChecksumKind;
use lh_core::job::{Event, JobId};
use std::hash::{Hash, Hasher};

/// One closed enum the GUI's single queue needs. `Sbe` carries the value directly, not a
/// `Result` — `analysis::sbe` is infallible (it takes an already-probed `StreamInfo`, not a
/// path), unlike the sketch in `docs/gui.md` §2, which guessed `Result<Sbe>` before this was
/// checked against the real signature. See the G2 notes at the bottom of that doc.
#[derive(Debug)]
pub enum JobOutcome {
    Verify(lh_core::Result<Verification>),
    Checksum(ChecksumKind, lh_core::Result<[u8; 16]>),
    Sbe(Sbe),
}

/// The `Subscription::run_with` data, hashed by a stable id only — `docs/gui.md` §G0/§2.
/// `id` must stay constant across `view` calls for this to be one logical subscription
/// rather than one Iced tears down and restarts every frame.
pub struct QueueEvents {
    pub id: u64,
    pub rx: crossbeam_channel::Receiver<Event<JobOutcome>>,
}

impl Hash for QueueEvents {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// What `App::handle_job_event` folds into a `JobEntry`'s status — a `Clone` stand-in for
/// `Event<JobOutcome>`, since Iced widgets (`text_input`, `pick_list`, ...) require
/// `Message: Clone` everywhere it is used, not just where a `Message::Job` is built, and
/// `JobOutcome`'s `lh_core::Result`s carry `lh_core::Error`, which is not `Clone` (it wraps
/// `std::io::Error`, `claxon::Error`, ...). The subscription in `main.rs` renders each
/// `Event<JobOutcome>` into one of these — via [`render`] for the terminal case — the
/// moment it comes off the queue's channel, so the raw error never has to cross into
/// Iced's `Message` type. See the G2 notes in `docs/gui.md`.
#[derive(Debug, Clone)]
pub enum JobUpdate {
    Started {
        id: JobId,
        label: String,
    },
    Progress {
        id: JobId,
        done: u32,
        total: u32,
    },
    Finished {
        id: JobId,
        result: Result<String, String>,
    },
    Cancelled {
        id: JobId,
    },
}

impl From<Event<JobOutcome>> for JobUpdate {
    fn from(event: Event<JobOutcome>) -> Self {
        match event {
            Event::Started { id, label } => JobUpdate::Started { id, label },
            Event::Progress { id, done, total } => JobUpdate::Progress { id, done, total },
            Event::Finished { id, output, .. } => JobUpdate::Finished {
                id,
                result: render(&output),
            },
            Event::Cancelled { id, .. } => JobUpdate::Cancelled { id },
        }
    }
}

/// One line of rendered outcome for the file table / job-queue panel, in the same shape
/// `lh-cli`'s batch commands print (`cmd_verify`/`cmd_sbe`/`cmd_checksum` in
/// `lh-cli/src/main.rs`) — this is that same rendering, as a `String` for a widget instead
/// of a `println!`.
fn render(outcome: &JobOutcome) -> Result<String, String> {
    match outcome {
        JobOutcome::Verify(Ok(Verification::Ok)) => Ok("OK".to_string()),
        JobOutcome::Verify(Ok(Verification::Md5Mismatch { stored, computed })) => Err(format!(
            "MISMATCH stored {} computed {}",
            hex::encode(stored),
            hex::encode(computed)
        )),
        JobOutcome::Verify(Ok(Verification::NoStoredMd5 { computed })) => Ok(format!(
            "NO MD5 (decoded cleanly; computed {})",
            hex::encode(computed)
        )),
        JobOutcome::Verify(Err(e)) => Err(e.to_string()),
        JobOutcome::Checksum(kind, Ok(digest)) => {
            Ok(format!("{} {}", kind.label(), hex::encode(digest)))
        }
        JobOutcome::Checksum(kind, Err(e)) => Err(format!("{}: {e}", kind.label())),
        JobOutcome::Sbe(Sbe::Aligned) => Ok("ALIGNED".to_string()),
        JobOutcome::Sbe(Sbe::Misaligned { remainder_frames }) => {
            Err(format!("SBE (+{remainder_frames} frames)"))
        }
        JobOutcome::Sbe(Sbe::NotApplicable { reason }) => Ok(format!("N/A ({reason})")),
    }
}
