//! The GUI's one shared job queue and its `JobOutcome` — `docs/gui.md` §2.
//!
//! `job::Queue<T>` never imports an operation's result type (`docs/job-queue.md` §2); this
//! is where that coupling terminates for `lh-gui`. G2 wired the three operations that need
//! no per-run options: verify, checksum (any of the three kinds), sbe. G3 adds convert —
//! the first operation with real (done, total) progress and a real cancel, and the first
//! that produces a `Provenance` for the log pane. G4 adds the two torrent operations
//! (`docs/torrent-creation.md` C5, `docs/torrent-verification.md` T4): create is one job
//! per Run, like convert; check is the first operation whose result the GUI needs to show
//! as its own per-file table, not just one status line, so it grows `JobUpdate::Finished`
//! a field for that rather than overloading the log pane (`docs/gui.md`'s G3 open question
//! 4 resolved that pane as "audit text only").

use lh_core::analysis::{Sbe, Verification};
use lh_core::checksum::ChecksumKind;
use lh_core::convert::Conversion;
use lh_core::job::{Event, JobId};
use lh_core::torrent::{Created, FileStatus, TorrentReport};
use std::hash::{Hash, Hasher};

/// One closed enum the GUI's single queue needs. `Sbe` carries the value directly, not a
/// `Result` — `analysis::sbe` is infallible (it takes an already-probed `StreamInfo`, not a
/// path), unlike the sketch in `docs/gui.md` §2, which guessed `Result<Sbe>` before this was
/// checked against the real signature. See the G2 notes at the bottom of that doc.
///
/// `Convert` boxes its `Conversion` for the same reason `lh-cli`'s own `ConvertOutcome`
/// does (`lh-cli/src/main.rs`): it is by far the largest variant here (a `Provenance` with
/// an `Agent::Tool`'s argv `Vec<String>` inside it), and it is moved through the queue's
/// channel once per file.
#[derive(Debug)]
pub enum JobOutcome {
    Verify(lh_core::Result<Verification>),
    Checksum(ChecksumKind, lh_core::Result<[u8; 16]>),
    Sbe(Sbe),
    Convert(lh_core::Result<Box<Conversion>>),
    TorrentCreate(lh_core::Result<Box<Created>>),
    TorrentCheck(lh_core::Result<Box<TorrentReport>>),
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
        /// The full audit-trail text (`Provenance::render()`) for the log pane, when this
        /// outcome produced one — only a written file has provenance to show; verify,
        /// checksum and sbe read a file but do not produce one. Torrent create writes a
        /// file too, but `Created` carries no `Provenance` (it runs no external tool and
        /// nothing built one for it before G4 — see the G4 notes); this stays `None` for
        /// both new operations rather than inventing one lh-core does not produce.
        provenance: Option<String>,
        /// A `TorrentCheck`'s per-file results, for the check panel's own table
        /// (`docs/torrent-verification.md` T4) — `render()` alone only has room for one
        /// summary line, which is what the job-queue panel and log use.
        torrent_check: Option<Vec<TorrentFileRow>>,
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
                result: render(&output),
                provenance: provenance_of(&output),
                torrent_check: torrent_check_rows(&output),
                id,
            },
            Event::Cancelled { id, .. } => JobUpdate::Cancelled { id },
        }
    }
}

/// One row of a finished `TorrentCheck`'s per-file table — the same information
/// `lh-cli`'s `cmd_torrent_check` prints, as data instead of `println!`s.
#[derive(Debug, Clone)]
pub struct TorrentFileRow {
    /// Displayed relative to the torrent's root, like `lh-cli`'s own `EXTRA` line does.
    pub path: String,
    pub label: &'static str,
    /// Empty when the status needs no elaboration (`OK`, `MISSING`, ...).
    pub detail: String,
}

/// The log pane's entry for a finished job, when it wrote one — `docs/gui.md` §2's Log /
/// audit pane bullet. Only `Convert` produces a `Provenance` today (verify/checksum/sbe
/// are read-only, per `PLAN.md` §1 Principle 3); torrent create also writes a file but
/// `lh_core::torrent::Created` carries no `Provenance` field, so G4 has nothing to render
/// here either — see the G4 notes in `docs/gui.md`.
fn provenance_of(outcome: &JobOutcome) -> Option<String> {
    match outcome {
        JobOutcome::Convert(Ok(c)) => Some(c.provenance.render()),
        _ => None,
    }
}

/// A finished `TorrentCheck`'s per-file rows, for the check panel's own table. `None` for
/// every other outcome, and for a `TorrentCheck` that failed outright (nothing to show a
/// table of when the torrent itself would not even parse).
fn torrent_check_rows(outcome: &JobOutcome) -> Option<Vec<TorrentFileRow>> {
    match outcome {
        JobOutcome::TorrentCheck(Ok(report)) => Some(report_rows(report)),
        _ => None,
    }
}

/// Mirrors `lh-cli`'s `cmd_torrent_check` line-by-line: skip padding (it is not a file a
/// user can look at), show every real file's status with the same detail CLI prints in
/// parentheses, then the extras the torrent does not list.
fn report_rows(report: &TorrentReport) -> Vec<TorrentFileRow> {
    let mut rows = Vec::with_capacity(report.files.len() + report.extra_local.len());
    for outcome in &report.files {
        if outcome.status == FileStatus::Padding {
            continue;
        }
        let shown = outcome
            .path
            .strip_prefix(&report.root)
            .unwrap_or(&outcome.path);
        let detail = match &outcome.status {
            FileStatus::WrongSize { expected, actual } => {
                format!("expected {expected} bytes, found {actual}")
            }
            FileStatus::Unreadable { reason } => reason.clone(),
            FileStatus::Corrupt { bad_pieces } => pieces_phrase(bad_pieces),
            FileStatus::Suspect { piece, shared_with } => format!(
                "piece {piece} is shared with {} other file(s); either could be at fault",
                shared_with.len()
            ),
            FileStatus::Partial {
                verified,
                unverifiable,
            } => format!(
                "{verified} pieces verified, {unverifiable} unreadable because a \
                 neighbouring file is bad"
            ),
            _ => String::new(),
        };
        rows.push(TorrentFileRow {
            path: shown.display().to_string(),
            label: outcome.status.label(),
            detail,
        });
    }
    for extra in &report.extra_local {
        let shown = extra.strip_prefix(&report.root).unwrap_or(extra);
        rows.push(TorrentFileRow {
            path: shown.display().to_string(),
            label: "EXTRA",
            detail: String::new(),
        });
    }
    rows
}

fn pieces_phrase(pieces: &[u32]) -> String {
    if pieces.len() == 1 {
        format!("piece {}", pieces[0])
    } else {
        let list: Vec<String> = pieces.iter().map(u32::to_string).collect();
        format!("pieces {}", list.join(", "))
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
        JobOutcome::Convert(Ok(c)) => {
            let name = c
                .output
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.output.display().to_string());
            if c.checked_against_source {
                Ok(format!("WROTE {name}"))
            } else {
                // A weaker result than the usual one, and it says so rather than looking
                // the same — matches `lh-cli`'s own `report_conversion`.
                Ok(format!(
                    "WROTE {name} (unchecked: nothing in the source to compare against)"
                ))
            }
        }
        JobOutcome::Convert(Err(e)) => Err(e.to_string()),
        JobOutcome::TorrentCreate(Ok(created)) => Ok(format!(
            "WROTE {} ({} files, {} {} of {})",
            created.name,
            created.files.len(),
            created.pieces,
            if created.pieces == 1 {
                "piece"
            } else {
                "pieces"
            },
            crate::format_bytes(created.piece_length),
        )),
        JobOutcome::TorrentCreate(Err(e)) => Err(e.to_string()),
        JobOutcome::TorrentCheck(Ok(report)) => match report.verdict() {
            lh_core::torrent::Verdict::Complete => Ok("OK".to_string()),
            lh_core::torrent::Verdict::SizesMatch => Ok("SIZES OK".to_string()),
            lh_core::torrent::Verdict::Incomplete => {
                let n = report.needs_attention().count();
                Err(format!(
                    "{n} of {} file(s) need attention — see the check results below",
                    report.files.len()
                ))
            }
        },
        JobOutcome::TorrentCheck(Err(e)) => Err(e.to_string()),
    }
}
