//! Little Helper desktop application — milestone M3, see `docs/gui.md`.
//!
//! G2 wired in the job queue: one long-lived `job::Queue<JobOutcome>` (`docs/gui.md`
//! §1/§2) lives for the app's whole life; the operation panel submits jobs against every
//! file in the working set, and the subscription in `subscription()` folds their events
//! into per-row status and the job-queue panel, the same way `lh-cli`'s `run_batch` folds
//! them into printed lines.
//!
//! G3 adds convert (both directions, through the same queue, with real progress and a
//! real cancel — J2) and the log/audit pane: every finished job that produced a
//! `Provenance` (today, only convert) appends its rendered text to `App::log`, exportable
//! to a text file via `Message::ExportLogPressed`.
//!
//! G4 adds the torrent panels (`docs/torrent-creation.md` C5, `docs/torrent-verification.md`
//! T4): create submits one job for the whole working-set folder, through the same queue and
//! real piece progress; check parses a `.torrent` immediately (Browse or drop) and, on
//! Check, submits a job whose finished `TorrentReport` fills a per-file results table —
//! the first `JobUpdate::Finished` payload beyond a single status line.

mod job;

use iced::widget::{
    Column, button, checkbox, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Element, Length, Subscription, Task};
use job::JobOutcome;
use lh_core::analysis::{self, Sbe};
use lh_core::checksum::{self, ChecksumFile, ChecksumKind, Entry};
use lh_core::convert::{self, Conversion, EncodeOpts};
use lh_core::job::{JobId, Queue};
use lh_core::model::AudioFormat;
use lh_core::scan::{self, WorkingSet};
use lh_core::tools::{Discovery, Registry, ToolId};
use lh_core::torrent::{
    CreateOpts, Metainfo, Passkeys, TrackerList, check_sizes, check_with_progress,
    create_with_progress, default_output, resolve,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One of the operations `lh-cli` already exposes. G2 wired verify/checksum/sbe, which
/// need no per-run options beyond the file itself; G3 adds convert, which needs a
/// direction (`ConvertTarget`) and reads `App::convert_overwrite`, the one option it does
/// need. Torrent create and check (G4) stay out of this enum: they act on a whole folder or
/// a dropped `.torrent`, not on the working set's per-file selection this enum drives, so
/// `run_torrent_create`/`run_torrent_check` are their own methods with their own panels
/// rather than two more variants that would not fit `run_operation`'s per-file loop.
///
/// S1 (`docs/gui-shell.md` §5) stopped this being a user-facing `pick_list` value: it is
/// now built from `App::area` plus area-local state (`convert_target`, `checksum_kind`)
/// right before a Run press, and passed into `run_operation` as an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Verify,
    Checksum(ChecksumKind),
    Sbe,
    Convert(ConvertTarget),
}

/// Convert's direction. Not `AudioFormat` — that also names `Shn`/`Ape`/`Wv`/`Tta`, which
/// `lh_core::convert` cannot produce, and this picker should only ever offer a choice that
/// works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConvertTarget {
    Wav,
    Flac,
}

impl std::fmt::Display for ConvertTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConvertTarget::Wav => "FLAC → WAV",
            ConvertTarget::Flac => "WAV → FLAC",
        })
    }
}

/// `ChecksumKind` (`lh_core::checksum`) has no `Display` of its own and the orphan rule
/// keeps one from being added here — a plain label function instead.
fn checksum_kind_label(kind: ChecksumKind) -> &'static str {
    match kind {
        ChecksumKind::Ffp => "FFP",
        ChecksumKind::Md5 => "MD5",
        ChecksumKind::St5 => "ST5",
    }
}

/// The application's areas — `docs/gui-shell.md` §3, in the original TLH menu's own order.
/// One is visible at a time in the area pane (§4); the rail (`rail()`) lists every one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Area {
    Files,
    Convert,
    ChecksumCreate,
    ChecksumCheck,
    TorrentCreate,
    TorrentCheck,
    Verify,
    Sbe,
    Binaries,
    About,
}

impl Area {
    /// The rail's rows, grouped exactly as `docs/gui-shell.md` §3 lays them out: `None`
    /// starts a new group with the given header; `Some` continues the previous one.
    const RAIL: &'static [(Option<&'static str>, Area, &'static str)] = &[
        (None, Area::Files, "Files"),
        (Some("FORMAT"), Area::Convert, "Convert"),
        (Some("CHECKSUM"), Area::ChecksumCreate, "Create"),
        (None, Area::ChecksumCheck, "Check"),
        (Some("TORRENT"), Area::TorrentCreate, "Create"),
        (None, Area::TorrentCheck, "Check"),
        (Some("ANALYSIS"), Area::Verify, "Verify"),
        (None, Area::Sbe, "SBE"),
        (Some(""), Area::Binaries, "Binaries"),
        (None, Area::About, "About"),
    ];
}

/// The dock's two bodies (`docs/gui-shell.md` §4) — the header (aggregate progress, Cancel)
/// is always shown; this picks which body fills the rest of the dock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockTab {
    Jobs,
    Log,
}

/// A submitted job's last-known state, keyed by `JobId` in `App::jobs`. A single
/// long-lived queue means `JobId::index()`'s dense-submission-order guarantee does not
/// hold here (`docs/job-queue.md` §7, `docs/gui.md` §5 open question 2) — the file table
/// instead looks up each row's latest job through `App::latest_job_by_path`.
enum JobStatus {
    Running { done: u32, total: u32 },
    Done(String),
    Failed(String),
    Cancelled,
}

struct JobEntry {
    label: String,
    status: JobStatus,
}

/// Tracks a Checksum → Create run while its per-file digest jobs are still in flight
/// (`docs/gui-shell.md` §6, S3). `order` fixes the entries' final order to submission
/// (scan) order rather than whichever job the queue's worker pool happens to finish
/// first — `lh-cli`'s own `run_batch` re-sorts back into submission order for the same
/// reason (`lh-cli/src/main.rs`'s `run_batch`), and a `.ffp` a user diffs run to run should
/// not reorder itself just because the OS scheduled threads differently.
struct ChecksumCreateBatch {
    kind: ChecksumKind,
    output: PathBuf,
    order: Vec<(JobId, String)>,
    digests: HashMap<JobId, [u8; 16]>,
    pending: HashSet<JobId>,
}

/// Tracks a Checksum → Check run while its per-entry comparison jobs are still in flight —
/// the same shape as [`ChecksumCreateBatch`], but accumulating `FileRow`s for
/// `App::checksum_check_rows` instead of `ChecksumFile::Entry`s to write. Order does not
/// matter here: the result is a table for reading, not a file another tool re-parses.
struct ChecksumCheckBatch {
    pending: HashSet<JobId>,
    rows: Vec<job::FileRow>,
}

fn status_label(status: &JobStatus) -> String {
    match status {
        JobStatus::Running { done, total } if *total > 0 => format!("running ({done}/{total})"),
        JobStatus::Running { .. } => "running".to_string(),
        JobStatus::Done(s) => s.clone(),
        JobStatus::Failed(s) => format!("FAILED: {s}"),
        JobStatus::Cancelled => "cancelled".to_string(),
    }
}

struct App {
    path_input: String,
    working_set: Option<WorkingSet>,
    /// The folder (or file) `working_set` was scanned from — G4's torrent-create panel
    /// makes a torrent *for* this, since a `WorkingSet` itself carries no root
    /// (`lh_core::scan::WorkingSet` doc). `None` after a failed scan, same as
    /// `working_set`.
    working_root: Option<PathBuf>,
    /// The file table's checkbox column (`docs/gui-shell.md` §5, S2) — which of
    /// `working_set`'s files `run_operation` submits jobs for. Filled with every path on
    /// each `scan` (scanning selects all — the common case is "do this to the show").
    /// Path-keyed rather than an index or a parallel `Vec<bool>` because
    /// `App::latest_job_by_path` already keys the table's other per-row state by path, and
    /// one convention beats two. GUI state only: `lh_core::scan::WorkingSet` gains no
    /// `selected` field (Principle 4). Torrent → Create reads `working_root`, not this — it
    /// makes a torrent for the whole folder on disk, never a filtered subset (§4).
    selected: HashSet<PathBuf>,
    tools: Registry,
    error: Option<String>,
    queue: Queue<JobOutcome>,
    /// The rail's current selection (`docs/gui-shell.md` §5). Defaults to `Area::Files`,
    /// the area the window opens on in place of TLH's own blank `tsNone`.
    area: Area,
    /// Convert's direction picker — area-local state `run_operation`'s caller reads to
    /// build `Operation::Convert` (§5). Replaces `Operation::ALL`'s flat `pick_list`.
    convert_target: ConvertTarget,
    /// Checksum → Create's kind picker, same role as `convert_target` for `Operation::Checksum`.
    checksum_kind: ChecksumKind,
    /// Checksum → Create's output path — the field that turns the per-file digests
    /// `Operation::Checksum` already computed into an actual `.ffp`/`.md5`/`.st5`
    /// (`docs/gui-shell.md` §6, S3).
    checksum_output: String,
    /// Set while a Checksum → Create run's digest jobs are still in flight; `None`
    /// otherwise, including before the first Run and after the file has been written.
    checksum_create_batch: Option<ChecksumCreateBatch>,
    /// Set by Browse or a checksum-file drop (`Message::PathDropped` routes by extension,
    /// same as `.torrent`) — parsed immediately via `App::pick_checksum_file` so the panel
    /// shows the entry count and kind before Check ever runs, the same shape as
    /// `torrent_check_path`/`torrent_check_meta`.
    checksum_check_path: Option<PathBuf>,
    /// Inferred from `checksum_check_path`'s extension, the same way `cmd_check` infers it
    /// (`lh-cli/src/main.rs`) — `None` when the extension names none of `.ffp`/`.md5`/`.st5`.
    checksum_check_kind: Option<ChecksumKind>,
    /// The parsed checksum file, once a valid one has been picked.
    checksum_check_file: Option<ChecksumFile>,
    /// Set while a Checksum → Check run's per-entry jobs are still in flight.
    checksum_check_batch: Option<ChecksumCheckBatch>,
    /// The last finished check's per-entry rows (`docs/gui-shell.md` §6's "per-file results
    /// table reusing G4's `JobUpdate` boundary") — same convention as `torrent_check_rows`:
    /// replaced wholesale on completion, not cleared between runs otherwise.
    checksum_check_rows: Vec<job::FileRow>,
    /// Convert's one option (`lh-cli`'s `--force`) — whether to overwrite an output that
    /// already exists. Split from `torrent_overwrite` in S1: the two checkboxes were one
    /// field only because convert and torrent-create were on screen together (G3/G4);
    /// separate areas make the sharing a bug waiting to happen (`docs/gui-shell.md` §5).
    convert_overwrite: bool,
    /// Torrent → Create's "overwrite an existing `.torrent`" option — see `convert_overwrite`.
    torrent_overwrite: bool,
    /// Which body the dock (§4) shows below its always-visible aggregate-progress header.
    dock_tab: DockTab,
    jobs: BTreeMap<JobId, JobEntry>,
    latest_job_by_path: HashMap<PathBuf, JobId>,
    /// The log/audit pane: `Provenance::render()` text from every finished job that
    /// produced one, oldest first (`docs/gui.md` §2, §5 open question 4 — resolved for G3
    /// as "the rendered strings from every finished job, in order," no new `report/`
    /// module).
    log: Vec<String>,
    /// `TrackerList::load()` and `Passkeys::load()`, read once at boot like `tools`
    /// (`Registry::discover()`) — the create panel's tracker picker is a read of this, no
    /// new plumbing, same shape as the Tools panel (`docs/gui.md` §2).
    trackers: TrackerList,
    passkeys: Passkeys,
    /// Comma-separated ids (from `trackers`, shown for reference) or bare announce URLs —
    /// `lh-cli`'s repeated `--tracker ID|URL` as one field, since Iced 0.14 has no built-in
    /// multi-line text input and a scrollable list of per-tracker checkboxes buys nothing
    /// a comma list does not already give a v0.1 user (`docs/gui.md` §2's "no new regions
    /// invented" principle applied to a widget, not just a layout).
    torrent_tracker_input: String,
    torrent_private: bool,
    torrent_source: String,
    torrent_comment: String,
    /// Set by Browse or a `.torrent` drop (`Message::PathDropped` routes by extension);
    /// parsed immediately via `App::pick_torrent` so the panel shows name/infohash/counts
    /// before Check ever runs, the same information `lh torrent info` prints.
    torrent_check_path: Option<PathBuf>,
    torrent_check_meta: Option<Metainfo>,
    torrent_check_against: String,
    torrent_check_quick: bool,
    /// The last finished check's per-file rows (`docs/torrent-verification.md` T4's file
    /// table). Replaced wholesale on each `JobUpdate::Finished` that carries one; not
    /// cleared between runs otherwise, same convention as `jobs` and `log`.
    torrent_check_rows: Vec<job::FileRow>,
}

#[derive(Debug, Clone)]
enum Message {
    PathInputChanged(String),
    BrowsePressed,
    FolderPicked(Option<PathBuf>),
    ScanPressed,
    PathDropped(PathBuf),
    FileToggled(PathBuf, bool),
    SelectAllToggled(bool),
    AreaSelected(Area),
    ConvertTargetSelected(ConvertTarget),
    ChecksumKindSelected(ChecksumKind),
    ChecksumOutputChanged(String),
    ChecksumOutputBrowsePressed,
    ChecksumOutputPicked(Option<PathBuf>),
    ChecksumCheckBrowsePressed,
    ChecksumFilePicked(Option<PathBuf>),
    ChecksumCheckPressed,
    ConvertOverwriteToggled(bool),
    TorrentOverwriteToggled(bool),
    DockTabSelected(DockTab),
    RunPressed,
    CancelPressed,
    Job(job::JobUpdate),
    ExportLogPressed,
    LogExportPathPicked(Option<PathBuf>),
    TorrentTrackerInputChanged(String),
    TorrentPrivateToggled(bool),
    TorrentSourceChanged(String),
    TorrentCommentChanged(String),
    TorrentCreatePressed,
    TorrentCheckBrowsePressed,
    TorrentFilePicked(Option<PathBuf>),
    TorrentCheckAgainstChanged(String),
    TorrentCheckAgainstBrowsePressed,
    TorrentCheckAgainstPicked(Option<PathBuf>),
    TorrentCheckQuickToggled(bool),
    TorrentCheckPressed,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        (
            App {
                path_input: String::new(),
                working_set: None,
                working_root: None,
                selected: HashSet::new(),
                tools: Registry::discover(),
                error: None,
                queue: Queue::new(),
                area: Area::Files,
                convert_target: ConvertTarget::Flac,
                checksum_kind: ChecksumKind::Ffp,
                checksum_output: String::new(),
                checksum_create_batch: None,
                checksum_check_path: None,
                checksum_check_kind: None,
                checksum_check_file: None,
                checksum_check_batch: None,
                checksum_check_rows: Vec::new(),
                convert_overwrite: false,
                torrent_overwrite: false,
                dock_tab: DockTab::Jobs,
                jobs: BTreeMap::new(),
                latest_job_by_path: HashMap::new(),
                log: Vec::new(),
                // A malformed user tracker list falls back to the bundled one rather than
                // failing boot outright — `lh-cli` can afford to exit 2 on this, a GUI
                // that never opens for a bad `tracker.lst` cannot (`PLAN.md` §1 Principle 1
                // is about outputs, not about refusing to start over one bad input file).
                trackers: TrackerList::load().unwrap_or_else(|_| TrackerList::bundled()),
                passkeys: Passkeys::load().unwrap_or_default(),
                torrent_tracker_input: String::new(),
                torrent_private: false,
                torrent_source: String::new(),
                torrent_comment: String::new(),
                torrent_check_path: None,
                torrent_check_meta: None,
                torrent_check_against: ".".to_string(),
                torrent_check_quick: false,
                torrent_check_rows: Vec::new(),
            },
            Task::none(),
        )
    }

    /// Scans `root` (a folder, or a single file — `scan::scan` handles both, walking just
    /// the one entry in the file case) and replaces the working set. Runs on the update
    /// thread: a working set is a show or a few shows (`PLAN.md` §4), not an archive, so a
    /// synchronous walk is imperceptible — no queue is warranted for this (`docs/gui.md`
    /// §1).
    fn scan(&mut self, root: &Path) {
        match scan::scan(root, true) {
            Ok(set) => {
                self.error = None;
                // Scanning selects all (§5) — the common case is "do this to the show",
                // and a fresh scan replaces whatever selection the previous working set had.
                self.selected = set.files.iter().map(|f| f.path.clone()).collect();
                self.working_set = Some(set);
                self.working_root = Some(root.to_path_buf());
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.working_set = None;
                self.working_root = None;
                self.selected.clear();
            }
        }
    }

    /// A `.torrent` chosen via Browse or dropped on the window — parsed immediately so the
    /// check panel can show name/infohash/counts before Check ever runs, the same
    /// information `lh torrent info` prints. Does not touch `torrent_check_rows`: those
    /// belong to the *previous* torrent's check, if any, and clearing them on a mere pick
    /// would lose a result the user has not asked to discard.
    fn pick_torrent(&mut self, path: PathBuf) {
        match Metainfo::read(&path) {
            Ok(meta) => {
                self.error = None;
                self.torrent_check_meta = Some(meta);
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.torrent_check_meta = None;
            }
        }
        self.torrent_check_path = Some(path);
    }

    /// A checksum file chosen via Browse or dropped on the window — parsed immediately, the
    /// same convention as [`pick_torrent`]. The kind is inferred from the extension exactly
    /// as `cmd_check` infers it (`lh-cli/src/main.rs`); an extension that names none of
    /// `.ffp`/`.md5`/`.st5` is an error here for the same reason it is a `bail!` there
    /// (`docs/gui-shell.md` §10 Q4 — the original asks via `frmTypeChecksumFile`, unscoped
    /// for S3).
    fn pick_checksum_file(&mut self, path: PathBuf) {
        let kind = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("ffp") => Some(ChecksumKind::Ffp),
            Some("md5") => Some(ChecksumKind::Md5),
            Some("st5") => Some(ChecksumKind::St5),
            _ => None,
        };
        self.checksum_check_kind = kind;
        self.checksum_check_file = None;
        match kind {
            Some(kind) => match ChecksumFile::read(kind, &path) {
                Ok(file) => {
                    self.error = None;
                    self.checksum_check_file = Some(file);
                }
                Err(e) => self.error = Some(e.to_string()),
            },
            None => {
                self.error = Some(format!(
                    "cannot tell what kind of checksum file this is from {:?}; expected .ffp, .md5 or .st5",
                    path.extension().and_then(|e| e.to_str()).unwrap_or("")
                ));
            }
        }
        self.checksum_check_path = Some(path);
    }

    /// Submits one job to the shared queue that makes a torrent for `working_root`
    /// (`docs/torrent-creation.md` C5) — a single job, like `lh torrent create`'s own
    /// queue-of-one (`lh-cli`'s `cmd_torrent_create`), not one per file: the payload is
    /// hashed as one sequential stream regardless of how many files it spans.
    ///
    /// Resolving trackers can fail (an unknown id, a tracker `lh-core` knows is broken) —
    /// checked here, synchronously, before anything is submitted, exactly where `lh-cli`
    /// checks it, so a bad tracker spec never becomes a job the queue has to fail instead.
    fn run_torrent_create(&mut self) {
        let Some(root) = self.working_root.clone() else {
            self.error = Some("scan a folder first".to_string());
            return;
        };
        let source = match root.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("{}: {e}", root.display()));
                return;
            }
        };
        let specs: Vec<String> = self
            .torrent_tracker_input
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let chosen = match resolve(&specs, &self.trackers, &self.passkeys) {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        for warning in &chosen.warnings {
            self.log.push(format!("warning: {warning}"));
        }

        let dst = match default_output(&source) {
            Some(d) => d,
            None => {
                self.error = Some(format!(
                    "{} has no name to write a torrent beside",
                    source.display()
                ));
                return;
            }
        };
        let source_tag = if self.torrent_source.trim().is_empty() {
            chosen.source.clone()
        } else {
            Some(self.torrent_source.trim().to_string())
        };
        let comment = (!self.torrent_comment.trim().is_empty())
            .then(|| self.torrent_comment.trim().to_string());
        let opts = CreateOpts {
            announce: chosen.tiers.clone(),
            private: self.torrent_private || chosen.private,
            source: source_tag,
            comment,
            overwrite: self.torrent_overwrite,
            ..CreateOpts::default()
        };

        self.error = None;
        self.queue.cancel_token().reset();
        let label = format!("torrent create: {}", source.display());
        let id = self.queue.submit(label.clone(), move |p| {
            JobOutcome::TorrentCreate(
                create_with_progress(&source, &dst, &opts, &mut |done, total| {
                    p.report(done, total);
                    !p.is_cancelled()
                })
                .map(Box::new),
            )
        });
        self.jobs.insert(
            id,
            JobEntry {
                label,
                status: JobStatus::Running { done: 0, total: 0 },
            },
        );
    }

    /// Submits one job that checks `torrent_check_path` against `torrent_check_against`
    /// (`docs/torrent-verification.md` T4). `check_with_progress`'s progress callback
    /// (`lh-core/src/torrent/verify.rs`) has no cancellation checkpoint the way
    /// `create_with_progress`'s does — Cancel still calls `Queue::cancel()`, but a check
    /// already streaming pieces runs to completion; see the G4 notes.
    fn run_torrent_check(&mut self) {
        let Some(torrent_path) = self.torrent_check_path.clone() else {
            self.error = Some("choose a .torrent file first".to_string());
            return;
        };
        let meta = match Metainfo::read(&torrent_path) {
            Ok(m) => m,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let against = PathBuf::from(self.torrent_check_against.trim());
        let quick = self.torrent_check_quick;

        self.error = None;
        self.queue.cancel_token().reset();
        let label = format!("torrent check: {}", meta.name);
        let id = self.queue.submit(label.clone(), move |p| {
            let result = if quick {
                check_sizes(&meta, &torrent_path, &against)
            } else {
                check_with_progress(&meta, &torrent_path, &against, &mut |done, total| {
                    p.report(done, total);
                })
            };
            JobOutcome::TorrentCheck(result.map(Box::new))
        });
        self.jobs.insert(
            id,
            JobEntry {
                label,
                status: JobStatus::Running { done: 0, total: 0 },
            },
        );
    }

    /// Submits one job per file in the working set for `operation` — passed in by the
    /// caller (each area's Run button; §5 of `docs/gui-shell.md`) rather than read off
    /// `self`, now that no single field holds "the" operation. Resets the queue's
    /// `CancelToken` first: `Queue::submit` checks that same shared token for every job for
    /// the queue's whole life (`lh-core/src/job/mod.rs`), so without a reset here, a single
    /// Cancel press would silently stop every future Run from ever executing a job — a gap
    /// only a long-lived queue like this one's can hit (`CancelToken::reset`'s doc,
    /// `docs/gui.md`'s G2 notes).
    ///
    /// A convert to FLAC needs the reference `flac` binary; discovered once at boot
    /// (`self.tools`), not re-discovered per run. Missing it fails the whole Run up front,
    /// the same as `lh-cli`'s own `cmd_convert` — before converting half a show, not after.
    ///
    /// Returns every `(JobId, label)` it actually submitted, in the same order `set.files`
    /// iterates — `run_checksum_create` (S3) is the one caller that needs this, to fix a
    /// `.ffp`/`.md5`/`.st5`'s entry order to submission order rather than whichever job the
    /// queue's worker pool happens to finish first.
    fn run_operation(&mut self, operation: Operation) -> Vec<(JobId, String)> {
        let Some(set) = &self.working_set else {
            return Vec::new();
        };
        if let Operation::Convert(ConvertTarget::Flac) = operation
            && let Err(e) = self.tools.require(ToolId::Flac)
        {
            self.error = Some(e.to_string());
            return Vec::new();
        }
        self.error = None;
        self.queue.cancel_token().reset();
        let overwrite = self.convert_overwrite;
        // Cloned once per Run rather than borrowed: each submitted job needs its own
        // owned `Tool` to move into its closure, and discovery already happened at boot.
        let flac_tool = match operation {
            Operation::Convert(ConvertTarget::Flac) => Some(
                self.tools
                    .require(ToolId::Flac)
                    .expect("checked above")
                    .clone(),
            ),
            _ => None,
        };
        let mut submitted = Vec::new();
        for file in &set.files {
            // Working-set areas act on the ticked rows only (`docs/gui-shell.md` §4, S2) —
            // an unticked file gets no job at all, not a skipped one, same treatment as the
            // already-in-target-format check right below.
            if !self.selected.contains(&file.path) {
                continue;
            }
            // A file already in the target format is a silent no-op in `lh-cli`'s own
            // `cmd_convert` (`ConvertOutcome::Skipped`, printed `SKIPPED ... (already
            // {want})`), not a failure — matched here by not submitting a job for it at
            // all, rather than inventing a `JobOutcome::Convert(Err(...))` that would show
            // up as FAILED in the job-queue panel for a file nothing was wrong with.
            if let Operation::Convert(target) = operation {
                let want = match target {
                    ConvertTarget::Wav => AudioFormat::Wav,
                    ConvertTarget::Flac => AudioFormat::Flac,
                };
                if file.format == want {
                    continue;
                }
            }
            let path = file.path.clone();
            let info = file.stream_info.clone();
            let label = file.file_name();
            let row_path = path.clone();
            let id = match operation {
                Operation::Verify => self.queue.submit(label.clone(), move |_p| {
                    JobOutcome::Verify(analysis::verify(&path))
                }),
                Operation::Checksum(kind) => self.queue.submit(label.clone(), move |_p| {
                    JobOutcome::Checksum(kind, checksum::compute(kind, &path))
                }),
                Operation::Sbe => self.queue.submit(label.clone(), move |_p| {
                    JobOutcome::Sbe(analysis::sbe(&info))
                }),
                Operation::Convert(ConvertTarget::Wav) => {
                    self.queue.submit(label.clone(), move |p| {
                        JobOutcome::Convert(convert_to_wav(&path, overwrite, p))
                    })
                }
                Operation::Convert(ConvertTarget::Flac) => {
                    let tool = flac_tool.clone().expect("discovered above");
                    self.queue.submit(label.clone(), move |p| {
                        JobOutcome::Convert(convert_to_flac(&path, &tool, overwrite, p))
                    })
                }
            };
            self.latest_job_by_path.insert(row_path, id);
            submitted.push((id, label.clone()));
            self.jobs.insert(
                id,
                JobEntry {
                    label,
                    status: JobStatus::Running { done: 0, total: 0 },
                },
            );
        }
        submitted
    }

    /// Checksum → Create (`docs/gui-shell.md` §6, S3): the per-file digest jobs
    /// `run_operation` already submits for `Operation::Checksum`, plus a batch that writes
    /// a `ChecksumFile` once every one of them has reported. Requires an output path up
    /// front — with none chosen there is nothing S3 adds over what G2 already did (the
    /// per-file digest shown as a status line), so this fails before submitting anything,
    /// the same "checked synchronously, before any job" convention `run_torrent_create`
    /// uses for its tracker spec.
    fn run_checksum_create(&mut self) {
        if self.checksum_output.trim().is_empty() {
            self.error = Some("choose an output file first".to_string());
            return;
        }
        let output = PathBuf::from(self.checksum_output.trim());
        let kind = self.checksum_kind;
        let order = self.run_operation(Operation::Checksum(kind));
        if order.is_empty() {
            // Nothing selected, or nothing in the working set — same "no job, not even
            // considered" treatment `run_operation` already gives an empty selection; a
            // batch with nothing pending would just write an empty file for no reason.
            return;
        }
        let pending = order.iter().map(|(id, _)| *id).collect();
        self.checksum_create_batch = Some(ChecksumCreateBatch {
            kind,
            output,
            order,
            digests: HashMap::new(),
            pending,
        });
    }

    /// Checksum → Check (`docs/gui-shell.md` §6, S3): one job per entry in the checksum
    /// file already parsed into `checksum_check_file`, each comparing `checksum::compute`
    /// against the entry's stored digest against the file beside the checksum file itself
    /// — the same directory `cmd_check` resolves each entry against
    /// (`lh-cli/src/main.rs`'s `cmd_check`).
    fn run_checksum_check(&mut self) {
        let Some(path) = self.checksum_check_path.clone() else {
            self.error = Some("choose a checksum file first".to_string());
            return;
        };
        let Some(kind) = self.checksum_check_kind else {
            return;
        };
        let Some(file) = self.checksum_check_file.clone() else {
            return;
        };
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        self.error = None;
        self.queue.cancel_token().reset();
        let mut pending = HashSet::new();
        for entry in &file.entries {
            let target = dir.join(&entry.file_name);
            let expected = entry.digest;
            let label = entry.file_name.clone();
            let id = self.queue.submit(label.clone(), move |_p| {
                let status = if !target.exists() {
                    job::ChecksumEntryStatus::Missing
                } else {
                    match checksum::compute(kind, &target) {
                        Ok(actual) if actual == expected => job::ChecksumEntryStatus::Ok,
                        Ok(actual) => job::ChecksumEntryStatus::Mismatch { expected, actual },
                        Err(e) => job::ChecksumEntryStatus::Failed(e.to_string()),
                    }
                };
                JobOutcome::ChecksumCheck(status)
            });
            pending.insert(id);
            self.jobs.insert(
                id,
                JobEntry {
                    label,
                    status: JobStatus::Running { done: 0, total: 0 },
                },
            );
        }
        self.checksum_check_batch = Some(ChecksumCheckBatch {
            pending,
            rows: Vec::new(),
        });
    }

    /// Progresses a Checksum → Create batch by one job's outcome, and writes the
    /// `ChecksumFile` once every submitted job has reported. `digest` is `None` for a job
    /// that failed or was cancelled — its file contributes no entry, matching `lh-cli`'s
    /// own `cmd_checksum`, but still counts toward the batch finishing.
    fn progress_checksum_create(&mut self, id: JobId, digest: Option<[u8; 16]>) {
        let Some(batch) = &mut self.checksum_create_batch else {
            return;
        };
        if !batch.pending.remove(&id) {
            return;
        }
        if let Some(digest) = digest {
            batch.digests.insert(id, digest);
        }
        if !batch.pending.is_empty() {
            return;
        }
        let batch = self.checksum_create_batch.take().expect("checked above");
        let mut out = ChecksumFile::new(batch.kind);
        for (id, file_name) in &batch.order {
            if let Some(digest) = batch.digests.get(id) {
                out.entries.push(Entry {
                    file_name: file_name.clone(),
                    digest: *digest,
                });
            }
        }
        match out.write(&batch.output) {
            Ok(()) => self.log.push(format!(
                "wrote {} {} entries to {}",
                out.entries.len(),
                batch.kind.label(),
                batch.output.display(),
            )),
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    /// Progresses a Checksum → Check batch by one job's row, filling `checksum_check_rows`
    /// once every submitted job has reported — same shape as `progress_checksum_create`,
    /// `row` is `None` for a job that was cancelled before producing one.
    fn progress_checksum_check(&mut self, id: JobId, row: Option<job::FileRow>) {
        let Some(batch) = &mut self.checksum_check_batch else {
            return;
        };
        if !batch.pending.remove(&id) {
            return;
        }
        if let Some(row) = row {
            batch.rows.push(row);
        }
        if batch.pending.is_empty() {
            let batch = self.checksum_check_batch.take().expect("checked above");
            self.checksum_check_rows = batch.rows;
        }
    }

    fn handle_job_event(&mut self, event: job::JobUpdate) {
        match event {
            job::JobUpdate::Started { id, label } => {
                self.jobs.entry(id).or_insert(JobEntry {
                    label,
                    status: JobStatus::Running { done: 0, total: 0 },
                });
            }
            job::JobUpdate::Progress { id, done, total } => {
                if let Some(entry) = self.jobs.get_mut(&id) {
                    entry.status = JobStatus::Running { done, total };
                }
            }
            job::JobUpdate::Finished {
                id,
                result,
                provenance,
                torrent_check,
                checksum_entry,
                checksum_check_row,
            } => {
                if let Some(entry) = self.jobs.get_mut(&id) {
                    entry.status = match result {
                        Ok(s) => JobStatus::Done(s),
                        Err(s) => JobStatus::Failed(s),
                    };
                }
                if let Some(text) = provenance {
                    self.log.push(text);
                }
                if let Some(rows) = torrent_check {
                    self.torrent_check_rows = rows;
                }
                self.progress_checksum_create(id, checksum_entry.map(|(_, digest)| digest));
                self.progress_checksum_check(id, checksum_check_row);
            }
            job::JobUpdate::Cancelled { id } => {
                if let Some(entry) = self.jobs.get_mut(&id) {
                    entry.status = JobStatus::Cancelled;
                }
                self.progress_checksum_create(id, None);
                self.progress_checksum_check(id, None);
            }
        }
    }
}

/// [`Operation::Convert`]`(`[`ConvertTarget::Wav`]`)`'s job body — `job::Progress<T>` is
/// the queue's channel back to the GUI (`docs/gui.md` §2), so both directions' real
/// per-file progress and cancellation (J2) reach the job-queue panel exactly the way
/// `lh-cli`'s own `cmd_convert` reaches its progress bar. `run_operation` never submits
/// this for a file already in WAV — that is a no-op, not a job.
fn convert_to_wav(
    path: &Path,
    overwrite: bool,
    p: &lh_core::job::Progress<JobOutcome>,
) -> lh_core::Result<Box<Conversion>> {
    let dst = destination_for(path, "wav")?;
    convert::to_wav_with_progress(path, &dst, overwrite, &mut |done, total| {
        p.report(done, total);
        !p.is_cancelled()
    })
    .map(Box::new)
}

/// [`convert_to_wav`]'s FLAC counterpart. `run_operation` never submits this for a file
/// already in FLAC, same as above.
fn convert_to_flac(
    path: &Path,
    tool: &lh_core::tools::Tool,
    overwrite: bool,
    p: &lh_core::job::Progress<JobOutcome>,
) -> lh_core::Result<Box<Conversion>> {
    let dst = destination_for(path, "flac")?;
    convert::to_flac_cancellable(
        path,
        &dst,
        tool,
        &EncodeOpts::default(),
        overwrite,
        &mut || !p.is_cancelled(),
    )
    .map(Box::new)
}

fn destination_for(path: &Path, extension: &str) -> lh_core::Result<PathBuf> {
    convert::destination(path, extension, None)
        .ok_or_else(|| lh_core::Error::malformed(path, "has no file name to work from"))
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::PathInputChanged(s) => app.path_input = s,
        Message::BrowsePressed => {
            return Task::perform(
                async { rfd::AsyncFileDialog::new().pick_folder().await },
                |handle| Message::FolderPicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::FolderPicked(Some(path)) => {
            app.path_input = path.display().to_string();
            app.scan(&path);
        }
        Message::FolderPicked(None) => {}
        Message::ScanPressed => {
            let path = PathBuf::from(app.path_input.trim());
            app.scan(&path);
        }
        Message::PathDropped(path) => {
            // A dropped `.torrent` goes to the torrent check panel, a `.ffp`/`.md5`/`.st5`
            // to the checksum check panel (S3), and anything else is a folder (or file) to
            // scan, same as Browse/Scan — the window has one drop target, not one per panel
            // (`docs/gui.md` §G0's window-wide drag-and-drop).
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            match ext.as_deref() {
                Some("torrent") => app.pick_torrent(path),
                Some("ffp") | Some("md5") | Some("st5") => app.pick_checksum_file(path),
                _ => {
                    app.path_input = path.display().to_string();
                    app.scan(&path);
                }
            }
        }
        Message::FileToggled(path, checked) => {
            if checked {
                app.selected.insert(path);
            } else {
                app.selected.remove(&path);
            }
        }
        Message::SelectAllToggled(checked) => {
            if checked {
                if let Some(set) = &app.working_set {
                    app.selected = set.files.iter().map(|f| f.path.clone()).collect();
                }
            } else {
                app.selected.clear();
            }
        }
        Message::AreaSelected(area) => app.area = area,
        Message::ConvertTargetSelected(t) => app.convert_target = t,
        Message::ChecksumKindSelected(k) => app.checksum_kind = k,
        Message::ChecksumOutputChanged(s) => app.checksum_output = s,
        Message::ChecksumOutputBrowsePressed => {
            let default_name = format!("checksum.{}", app.checksum_kind.extension());
            return Task::perform(
                rfd::AsyncFileDialog::new()
                    .set_file_name(default_name)
                    .save_file(),
                |handle| Message::ChecksumOutputPicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::ChecksumOutputPicked(Some(path)) => {
            app.checksum_output = path.display().to_string();
        }
        Message::ChecksumOutputPicked(None) => {}
        Message::ChecksumCheckBrowsePressed => {
            return Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("checksum", &["ffp", "md5", "st5"])
                        .pick_file()
                        .await
                },
                |handle| Message::ChecksumFilePicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::ChecksumFilePicked(Some(path)) => app.pick_checksum_file(path),
        Message::ChecksumFilePicked(None) => {}
        Message::ChecksumCheckPressed => app.run_checksum_check(),
        Message::ConvertOverwriteToggled(v) => app.convert_overwrite = v,
        Message::TorrentOverwriteToggled(v) => app.torrent_overwrite = v,
        Message::DockTabSelected(tab) => app.dock_tab = tab,
        // Only the four working-set areas (§4) have a Run button; the rail can only ever
        // select an area whose panel emitted this, so the other areas are unreachable here
        // rather than mis-submitting an operation nobody asked for.
        Message::RunPressed => match app.area {
            Area::Convert => {
                app.run_operation(Operation::Convert(app.convert_target));
            }
            Area::ChecksumCreate => app.run_checksum_create(),
            Area::Verify => {
                app.run_operation(Operation::Verify);
            }
            Area::Sbe => {
                app.run_operation(Operation::Sbe);
            }
            _ => {}
        },
        Message::CancelPressed => app.queue.cancel(),
        Message::Job(event) => app.handle_job_event(event),
        Message::ExportLogPressed => {
            return Task::perform(
                rfd::AsyncFileDialog::new()
                    .set_file_name("little-helper-log.txt")
                    .save_file(),
                |handle| Message::LogExportPathPicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::LogExportPathPicked(Some(path)) => {
            if let Err(e) = std::fs::write(&path, app.log.join("\n")) {
                app.error = Some(format!("writing {}: {e}", path.display()));
            }
        }
        Message::LogExportPathPicked(None) => {}
        Message::TorrentTrackerInputChanged(s) => app.torrent_tracker_input = s,
        Message::TorrentPrivateToggled(v) => app.torrent_private = v,
        Message::TorrentSourceChanged(s) => app.torrent_source = s,
        Message::TorrentCommentChanged(s) => app.torrent_comment = s,
        Message::TorrentCreatePressed => app.run_torrent_create(),
        Message::TorrentCheckBrowsePressed => {
            return Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("torrent", &["torrent"])
                        .pick_file()
                        .await
                },
                |handle| Message::TorrentFilePicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::TorrentFilePicked(Some(path)) => app.pick_torrent(path),
        Message::TorrentFilePicked(None) => {}
        Message::TorrentCheckAgainstChanged(s) => app.torrent_check_against = s,
        Message::TorrentCheckAgainstBrowsePressed => {
            return Task::perform(
                async { rfd::AsyncFileDialog::new().pick_folder().await },
                |handle| Message::TorrentCheckAgainstPicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::TorrentCheckAgainstPicked(Some(path)) => {
            app.torrent_check_against = path.display().to_string();
        }
        Message::TorrentCheckAgainstPicked(None) => {}
        Message::TorrentCheckQuickToggled(v) => app.torrent_check_quick = v,
        Message::TorrentCheckPressed => app.run_torrent_check(),
    }
    Task::none()
}

/// S1's shape (`docs/gui-shell.md` §4): a fixed-width rail, a content pane that is a
/// global path bar over one area's own controls (plus the shared file table, for the
/// working-set areas), and a dock pinned to the bottom that stays visible across every
/// area. No panel's own logic changes here — each one just moves under `match app.area`.
fn view(app: &App) -> Element<'_, Message> {
    let path_bar = row![
        text_input("Folder to scan...", &app.path_input)
            .on_input(Message::PathInputChanged)
            .on_submit(Message::ScanPressed),
        button("Browse...").on_press(Message::BrowsePressed),
        button("Scan").on_press(Message::ScanPressed),
    ]
    .spacing(8);

    let error: Element<'_, Message> = text(app.error.as_deref().unwrap_or("")).into();

    let content = column![path_bar, error, area_pane(app)]
        .spacing(12)
        .padding(12)
        .height(Length::Fill);

    let body = row![
        container(rail(app)).width(Length::Fixed(140.0)).padding(8),
        container(content).width(Length::Fill),
    ]
    .height(Length::FillPortion(3));

    container(column![body, dock(app)]).into()
}

/// One area's controls, matching `Area` (`docs/gui-shell.md` §5). Working-set areas
/// (Files, Convert, Checksum → Create, Verify, SBE) append the shared file table below
/// their controls; document areas (Torrent → Check) show their own result view instead;
/// static areas (Binaries, About) show neither.
fn area_pane(app: &App) -> Element<'_, Message> {
    match app.area {
        Area::Files => file_table(app),
        Area::Convert => column![convert_panel(app), file_table(app)]
            .spacing(12)
            .into(),
        Area::ChecksumCreate => column![checksum_create_panel(app), file_table(app)]
            .spacing(12)
            .into(),
        Area::ChecksumCheck => column![
            checksum_check_panel(app),
            file_rows_panel("Checksum check results", &app.checksum_check_rows),
        ]
        .spacing(12)
        .into(),
        Area::TorrentCreate => torrent_create_panel(app),
        Area::TorrentCheck => column![
            torrent_check_panel(app),
            file_rows_panel("Torrent check results", &app.torrent_check_rows),
        ]
        .spacing(12)
        .into(),
        Area::Verify => column![run_cancel_row(app), file_table(app)]
            .spacing(12)
            .into(),
        Area::Sbe => column![run_cancel_row(app), file_table(app)]
            .spacing(12)
            .into(),
        Area::Binaries => tools_panel(&app.tools),
        Area::About => about_panel(),
    }
}

/// The left rail — `Area::RAIL`'s group headers and rows, in TLH's own menu order
/// (`docs/gui-shell.md` §3). Group headers are plain text, not buttons: TLH's own
/// `&Format` opens a menu but performs nothing, and a header that looks clickable and is
/// not is worse than one that plainly is not.
fn rail(app: &App) -> Element<'_, Message> {
    let mut col = Column::new().spacing(2);
    for (header, area, label) in Area::RAIL {
        match header {
            Some("") => {
                col = col.push(container(text("")).height(Length::Fixed(8.0)));
            }
            Some(h) => col = col.push(text(*h).size(12)),
            None => {}
        }
        col = col.push(rail_row(label, *area, app.area == *area));
    }
    scrollable(col).into()
}

/// One rail row. The selected row is styled, not merely remembered
/// (`docs/gui-shell.md` §4) — `button::secondary` for the current area, `button::text`
/// (no visible chrome) for every other one.
fn rail_row(label: &str, area: Area, selected: bool) -> Element<'_, Message> {
    button(text(label))
        .width(Length::Fill)
        .on_press(Message::AreaSelected(area))
        .style(move |theme, status| {
            if selected {
                button::secondary(theme, status)
            } else {
                button::text(theme, status)
            }
        })
        .into()
}

fn run_cancel_row(app: &App) -> Element<'_, Message> {
    let run =
        button("Run").on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button("Cancel").on_press(Message::CancelPressed);
    row![run, cancel].spacing(8).into()
}

fn convert_panel(app: &App) -> Element<'_, Message> {
    let direction = pick_list(
        &[ConvertTarget::Flac, ConvertTarget::Wav][..],
        Some(app.convert_target),
        Message::ConvertTargetSelected,
    );
    let overwrite = checkbox(app.convert_overwrite)
        .label("Overwrite existing outputs")
        .on_toggle(Message::ConvertOverwriteToggled);
    let run =
        button("Run").on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button("Cancel").on_press(Message::CancelPressed);

    row![text("Direction:"), direction, overwrite, run, cancel]
        .spacing(8)
        .into()
}

/// Checksum → Create (`docs/gui-shell.md` §6, S3): the digest-per-file computation
/// `Operation::Checksum` already did in G2, a kind picker (unchanged since S1), and the
/// output path that turns those digests into a written `ChecksumFile`
/// (`App::run_checksum_create`).
fn checksum_create_panel(app: &App) -> Element<'_, Message> {
    let kinds = row(
        [ChecksumKind::Ffp, ChecksumKind::Md5, ChecksumKind::St5]
            .into_iter()
            .map(|k| kind_button(k, app.checksum_kind == k)),
    )
    .spacing(4);
    let output = text_input("Output file (.ffp/.md5/.st5)", &app.checksum_output)
        .on_input(Message::ChecksumOutputChanged);
    let browse = button("Browse...").on_press(Message::ChecksumOutputBrowsePressed);
    let run =
        button("Run").on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button("Cancel").on_press(Message::CancelPressed);

    column![
        row![text("Kind:"), kinds].spacing(8),
        row![output, browse].spacing(8),
        row![run, cancel].spacing(8),
    ]
    .spacing(8)
    .into()
}

fn kind_button(kind: ChecksumKind, selected: bool) -> Element<'static, Message> {
    button(text(checksum_kind_label(kind)))
        .on_press(Message::ChecksumKindSelected(kind))
        .style(move |theme, status| {
            if selected {
                button::secondary(theme, status)
            } else {
                button::text(theme, status)
            }
        })
        .into()
}

/// Checksum → Check (`docs/gui-shell.md` §6, S3): Browse or drop a `.ffp`/`.md5`/`.st5`,
/// see what `App::pick_checksum_file` parsed from it, then Check against the files beside
/// it. The per-entry table is [`file_rows_panel`], not here — same split as
/// [`torrent_check_panel`]/[`file_rows_panel`].
fn checksum_check_panel(app: &App) -> Element<'_, Message> {
    let label = match &app.checksum_check_path {
        Some(p) => p.display().to_string(),
        None => {
            "No checksum file chosen — Browse or drop a .ffp/.md5/.st5 on the window."
                .to_string()
        }
    };
    let browse = button("Browse...").on_press(Message::ChecksumCheckBrowsePressed);

    let info: Element<'_, Message> = match (&app.checksum_check_kind, &app.checksum_check_file) {
        (Some(kind), Some(file)) => text(format!(
            "{} entries, kind {}",
            file.entries.len(),
            kind.label()
        ))
        .into(),
        _ => text("").into(),
    };

    let run = button("Check")
        .on_press_maybe(app.checksum_check_file.is_some().then_some(Message::ChecksumCheckPressed));
    let cancel = button("Cancel").on_press(Message::CancelPressed);

    column![
        text("Check checksum file"),
        row![text(label), browse].spacing(8),
        info,
        row![run, cancel].spacing(8),
    ]
    .spacing(8)
    .into()
}

fn about_panel() -> Element<'static, Message> {
    column![
        text("Little Helper"),
        text(format!("v{}", env!("CARGO_PKG_VERSION"))),
    ]
    .spacing(4)
    .into()
}

/// The dock (`docs/gui-shell.md` §4): a header that is always the aggregate `N of M done`
/// plus Cancel, and a `Jobs | Log` toggle that switches only the body — the two bodies
/// want the same vertical space, and the aggregate line must never be one click away.
fn dock(app: &App) -> Element<'_, Message> {
    let total = app.jobs.len();
    let done = app
        .jobs
        .values()
        .filter(|e| !matches!(e.status, JobStatus::Running { .. }))
        .count();

    let jobs_tab = dock_tab_button("Jobs", DockTab::Jobs, app.dock_tab == DockTab::Jobs);
    let log_tab = dock_tab_button("Log", DockTab::Log, app.dock_tab == DockTab::Log);
    let header = row![
        text(format!("Jobs: {done} of {total} done")),
        jobs_tab,
        log_tab,
        button("Cancel").on_press(Message::CancelPressed),
    ]
    .spacing(8);

    let dock_body = match app.dock_tab {
        DockTab::Jobs => job_queue_panel(&app.jobs),
        DockTab::Log => log_panel(&app.log),
    };

    container(column![header, dock_body].spacing(4).padding(8))
        .height(Length::FillPortion(2))
        .into()
}

fn dock_tab_button(label: &str, tab: DockTab, selected: bool) -> Element<'_, Message> {
    button(text(label))
        .on_press(Message::DockTabSelected(tab))
        .style(move |theme, status| {
            if selected {
                button::secondary(theme, status)
            } else {
                button::text(theme, status)
            }
        })
        .into()
}

/// The checkbox column width, shared by the select-all header and every row's own
/// checkbox so the two line up.
const SELECT_COLUMN: Length = Length::Fixed(24.0);

fn file_table(app: &App) -> Element<'_, Message> {
    let Some(set) = app.working_set.as_ref() else {
        return text("Drop a folder here, or use Browse / Scan.").into();
    };

    // Select-all reflects the current selection rather than being remembered separately
    // (S2, `docs/gui-shell.md` §9): checked only once every file is, so toggling it off
    // after a partial selection clears the rest instead of leaving it stuck checked.
    let all_selected =
        !set.files.is_empty() && set.files.iter().all(|f| app.selected.contains(&f.path));
    let header = row![
        container(checkbox(all_selected).on_toggle(Message::SelectAllToggled))
            .width(SELECT_COLUMN),
        text("Name").width(Length::FillPortion(4)),
        text("Format").width(Length::FillPortion(1)),
        text("Duration").width(Length::FillPortion(1)),
        text("Rate/Bits/Ch").width(Length::FillPortion(2)),
        text("SBE").width(Length::FillPortion(2)),
        text("Status").width(Length::FillPortion(3)),
    ]
    .spacing(8);

    let mut rows = Column::new().spacing(4).push(header);
    for file in &set.files {
        let info = &file.stream_info;
        let status = app
            .latest_job_by_path
            .get(&file.path)
            .and_then(|id| app.jobs.get(id))
            .map(|entry| status_label(&entry.status))
            .unwrap_or_else(|| "—".to_string());
        let path = file.path.clone();
        let ticked = checkbox(app.selected.contains(&file.path))
            .on_toggle(move |checked| Message::FileToggled(path.clone(), checked));
        rows = rows.push(
            row![
                container(ticked).width(SELECT_COLUMN),
                text(file.file_name()).width(Length::FillPortion(4)),
                text(file.format.name()).width(Length::FillPortion(1)),
                text(format_duration(info.duration_secs())).width(Length::FillPortion(1)),
                text(format!(
                    "{} Hz / {}-bit / {}ch",
                    info.sample_rate, info.bits_per_sample, info.channels
                ))
                .width(Length::FillPortion(2)),
                text(sbe_label(&analysis::sbe(info))).width(Length::FillPortion(2)),
                text(status).width(Length::FillPortion(3)),
            ]
            .spacing(8),
        );
    }
    for (path, reason) in &set.skipped {
        rows = rows.push(text(format!("{} — skipped: {reason}", path.display())));
    }

    scrollable(rows).height(Length::FillPortion(3)).into()
}

/// `docs/torrent-creation.md` C5: folder (`App::working_root`, already scanned above) →
/// trackers → create, with piece progress through the same job-queue panel every other
/// operation uses. Pre-flight (C4) is postponed there and stays out of this panel too.
fn torrent_create_panel(app: &App) -> Element<'_, Message> {
    let mut known = Column::new()
        .spacing(2)
        .push(text("Known trackers (id, name, health):"));
    for t in app.trackers.all() {
        known = known.push(text(format!("{}  {}  {}", t.id, t.name, t.health.label())));
    }

    let tracker_input = text_input(
        "Tracker ids or URLs, comma-separated (blank = trackerless)",
        &app.torrent_tracker_input,
    )
    .on_input(Message::TorrentTrackerInputChanged);
    let private = checkbox(app.torrent_private)
        .label("Private (BEP 27)")
        .on_toggle(Message::TorrentPrivateToggled);
    let source = text_input("Source tag (optional)", &app.torrent_source)
        .on_input(Message::TorrentSourceChanged);
    let comment = text_input("Comment (optional)", &app.torrent_comment)
        .on_input(Message::TorrentCommentChanged);
    let overwrite = checkbox(app.torrent_overwrite)
        .label("Overwrite existing .torrent")
        .on_toggle(Message::TorrentOverwriteToggled);
    let create = button("Create torrent").on_press_maybe(
        app.working_root
            .is_some()
            .then_some(Message::TorrentCreatePressed),
    );

    column![
        text("Create torrent"),
        scrollable(known).height(Length::Fixed(80.0)),
        tracker_input,
        row![private, source, comment, overwrite, create].spacing(8),
    ]
    .spacing(8)
    .into()
}

/// `docs/torrent-verification.md` T4: drop or Browse a `.torrent`, see what
/// `App::pick_torrent` parsed from it, then Check against a folder. The per-file table is
/// [`file_rows_panel`], not here — it needs a finished job's rows, not just
/// the metainfo this panel already has before Check is ever pressed.
fn torrent_check_panel(app: &App) -> Element<'_, Message> {
    let torrent_label = match &app.torrent_check_path {
        Some(p) => p.display().to_string(),
        None => "No .torrent chosen — Browse or drop one on the window.".to_string(),
    };
    let browse = button("Browse .torrent...").on_press(Message::TorrentCheckBrowsePressed);

    let info: Element<'_, Message> = match &app.torrent_check_meta {
        Some(meta) => text(format!(
            "{}  {}  {} files  {} pieces of {}",
            meta.name,
            meta.info_hash_hex(),
            meta.real_files().count(),
            meta.pieces.len(),
            format_bytes(meta.piece_length),
        ))
        .into(),
        None => text("").into(),
    };

    let against = text_input("Folder to check against", &app.torrent_check_against)
        .on_input(Message::TorrentCheckAgainstChanged);
    let against_browse =
        button("Browse folder...").on_press(Message::TorrentCheckAgainstBrowsePressed);
    let quick = checkbox(app.torrent_check_quick)
        .label("Quick (sizes only)")
        .on_toggle(Message::TorrentCheckQuickToggled);
    let run = button("Check").on_press_maybe(
        app.torrent_check_path
            .is_some()
            .then_some(Message::TorrentCheckPressed),
    );

    column![
        text("Check torrent"),
        row![text(torrent_label), browse].spacing(8),
        info,
        row![against, against_browse, quick, run].spacing(8),
    ]
    .spacing(8)
    .into()
}

/// The last finished check's per-file status — `docs/torrent-verification.md` T4's "file
/// table with status". Empty until a check has actually finished once.
/// The last finished run's per-file rows — [`job::FileRow`]'s shape, one caller for
/// `Torrent → Check` (G4) and one for `Checksum → Check` (S3, `docs/gui-shell.md` §6: "that
/// table is not new work either ... this is the second caller that pattern was waiting
/// for"). Empty until a check of that kind has actually finished once.
fn file_rows_panel(title: &str, rows: &[job::FileRow]) -> Element<'static, Message> {
    if rows.is_empty() {
        return text("").into();
    }
    let mut list = Column::new().spacing(4).push(text(title.to_string()));
    for r in rows {
        let line = if r.detail.is_empty() {
            format!("{:<11} {}", r.label, r.path)
        } else {
            format!("{:<11} {}  ({})", r.label, r.path, r.detail)
        };
        list = list.push(text(line));
    }
    scrollable(list).height(Length::FillPortion(2)).into()
}

/// The log/audit pane — `Provenance::render()` text from every finished job that produced
/// one, oldest first, plus an Export button that writes them to a text file the user
/// picks (`docs/gui.md` §2). Not cleared between runs, same as the job-queue panel.
/// The log/audit pane's body — `Provenance::render()` text from every finished job that
/// produced one, oldest first, plus an Export button (`docs/gui.md` §2). The `Jobs | Log`
/// header lives in [`dock`], not here — the aggregate line applies to Jobs only.
fn log_panel(log: &[String]) -> Element<'_, Message> {
    let export = button("Export log...")
        .on_press_maybe((!log.is_empty()).then_some(Message::ExportLogPressed));
    let mut list = Column::new().spacing(4).push(row![export]);
    for entry in log {
        for line in entry.lines() {
            list = list.push(text(line.to_string()));
        }
    }
    scrollable(list).height(Length::Fill).into()
}

/// One line per job, oldest first (`BTreeMap<JobId, _>` order — `docs/gui.md` §4) — the
/// job-queue panel `PLAN.md` §4 names. The aggregate `N of M done` line lives in [`dock`]'s
/// header, not here, since it must stay visible even while the Log tab is showing. Unlike
/// `lh-cli`'s batch commands, entries are not cleared between runs: the queue is
/// long-lived (`docs/gui.md` §1), so a second Run's jobs simply join the first's here.
fn job_queue_panel(jobs: &BTreeMap<JobId, JobEntry>) -> Element<'_, Message> {
    let mut list = Column::new().spacing(4);
    for entry in jobs.values() {
        list = list.push(text(format!(
            "{}: {}",
            entry.label,
            status_label(&entry.status)
        )));
    }

    scrollable(list).height(Length::Fill).into()
}

fn tools_panel(tools: &Registry) -> Element<'_, Message> {
    // Labelled "Binaries" here, matching the rail row (`docs/gui-shell.md` §3) — TLH's own
    // "Tools" menu means repair (Fix SBEs, Strip header, Create skt), a different, v0.2
    // thing. `Registry`, `ToolId` and `lh tools` keep their names; this is a GUI label.
    let mut list = Column::new().spacing(4).push(text("Binaries"));
    for (id, discovery) in tools.entries() {
        list = list.push(text(tool_line(id, discovery)));
    }
    list.into()
}

fn tool_line(id: ToolId, discovery: &Discovery) -> String {
    match discovery {
        Discovery::Found(tool) => format!(
            "{}: {} ({}) — {}",
            id.name(),
            tool.path.display(),
            tool.source.label(),
            tool.version
        ),
        Discovery::NotFound { searched } => {
            format!("{}: not found (looked: {})", id.name(), searched.join(", "))
        }
        Discovery::Unusable { path, reason } => {
            format!("{}: {} — unusable: {reason}", id.name(), path.display())
        }
    }
}

/// Duplicated from `lh-cli`'s own `format_bytes` rather than lifted into `lh-core`: pure
/// display formatting, not a correctness-sensitive detail like `convert::destination` or
/// `torrent::default_output` — `format_duration` right below is already the same kind of
/// duplicate.
pub(crate) fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn format_duration(secs: Option<f64>) -> String {
    match secs {
        Some(s) => {
            let total = s.round() as u64;
            format!("{}:{:02}", total / 60, total % 60)
        }
        None => "?".to_string(),
    }
}

fn sbe_label(sbe: &Sbe) -> String {
    match sbe {
        Sbe::Aligned => "aligned".to_string(),
        Sbe::Misaligned { remainder_frames } => format!("misaligned ({remainder_frames} frames)"),
        Sbe::NotApplicable { reason } => format!("n/a ({reason})"),
    }
}

/// The job-queue bridge (`docs/gui.md` §G0/§2) alongside the drag-and-drop listener G1
/// already had. `id: 0` is fixed because there is exactly one queue for the app's whole
/// life (`docs/gui.md` §1) — no per-job or per-batch id to keep synchronized across `view`
/// calls, which is exactly the failure mode §G0 flagged for a changing `Subscription` hash.
fn subscription(app: &App) -> Subscription<Message> {
    Subscription::batch([
        Subscription::run_with(
            job::QueueEvents {
                id: 0,
                rx: app.queue.events(),
            },
            |data: &job::QueueEvents| {
                let rx = data.rx.clone();
                iced::stream::channel(64, async move |mut output| {
                    use iced::futures::SinkExt;
                    for event in rx.iter() {
                        let _ = output.send(Message::Job(event.into())).await;
                    }
                })
            },
        ),
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::PathDropped(path))
            }
            _ => None,
        }),
    ])
}

fn main() -> iced::Result {
    // Rail + table + dock has a floor below which it stops being usable
    // (`docs/gui-shell.md` §5) — TLH's own window is 634×407 and is not resizable smaller.
    iced::application(App::boot, update, view)
        .subscription(subscription)
        .title("Little Helper")
        .window(iced::window::Settings {
            min_size: Some(iced::Size::new(900.0, 600.0)),
            ..Default::default()
        })
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real fixture corpus M1 already built (`lh-core/tests/fixtures`), scanned
    /// through the exact function `App::scan` calls — proof the file table's data comes
    /// from real files, not just that the widget tree type-checks.
    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../lh-core/tests/fixtures")
    }

    #[test]
    fn scanning_the_fixture_corpus_finds_known_files_and_no_skips() {
        let set = scan::scan(&fixtures_dir(), false).expect("fixtures dir must scan");
        assert!(
            set.files.iter().any(|f| f.file_name() == "cdda-sbe.flac"),
            "expected cdda-sbe.flac among {:?}",
            set.files.iter().map(|f| f.file_name()).collect::<Vec<_>>()
        );
        assert!(
            set.skipped.is_empty(),
            "unexpected skips: {:?}",
            set.skipped
        );
    }

    #[test]
    fn a_known_misaligned_fixture_reports_sbe_through_the_same_path_the_table_uses() {
        let set = scan::scan(&fixtures_dir(), false).expect("fixtures dir must scan");
        let sbe_file = set
            .files
            .iter()
            .find(|f| f.file_name() == "cdda-sbe.flac")
            .expect("cdda-sbe.flac must be in the fixture corpus");
        let label = sbe_label(&analysis::sbe(&sbe_file.stream_info));
        assert!(
            label.starts_with("misaligned"),
            "cdda-sbe.flac should report misaligned SBE, got {label:?}"
        );
    }

    #[test]
    fn format_duration_matches_mm_ss() {
        assert_eq!(format_duration(Some(0.4)), "0:00");
        assert_eq!(format_duration(Some(65.6)), "1:06");
        assert_eq!(format_duration(None), "?");
    }

    /// Drains `rx` into `app` until every one of `total` submitted jobs has a terminal
    /// event (`Finished` or `Cancelled`) — the same loop `lh-cli`'s `run_batch` runs, minus
    /// the printing, and run directly against the real `Queue<JobOutcome>` rather than
    /// through Iced's `Subscription` machinery, which nothing outside a running window can
    /// drive.
    fn drain(
        app: &mut App,
        rx: &crossbeam_channel::Receiver<lh_core::job::Event<JobOutcome>>,
        total: usize,
    ) {
        let mut done = 0;
        while done < total {
            let event = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("queue closed with jobs still outstanding");
            let terminal = matches!(
                event,
                lh_core::job::Event::Finished { .. } | lh_core::job::Event::Cancelled { .. }
            );
            app.handle_job_event(event.into());
            if terminal {
                done += 1;
            }
        }
    }

    /// Real evidence the queue wiring — not just the widget tree — moves a real file
    /// through `analysis::verify` and back into `App` state: `cdda-aligned.flac` decodes
    /// clean and matches its own MD5, `wrong-md5.flac` decodes clean but does not, per the
    /// fixture corpus `lh-core/tests/fixtures` already establishes for `lh verify` (see
    /// `lh-cli`'s own `cmd_verify`, which these fixtures were built against).
    #[test]
    fn running_verify_through_the_real_queue_marks_ok_and_mismatch_files_correctly() {
        let (mut app, _) = App::boot();
        app.scan(&fixtures_dir());
        app.run_operation(Operation::Verify);

        let files: Vec<(String, PathBuf)> = app
            .working_set
            .as_ref()
            .expect("scan must populate a working set")
            .files
            .iter()
            .map(|f| (f.file_name(), f.path.clone()))
            .collect();
        let rx = app.queue.events();
        drain(&mut app, &rx, files.len());

        let status_for = |name: &str| {
            let path = &files.iter().find(|(n, _)| n == name).unwrap().1;
            let id = app.latest_job_by_path[path];
            status_label(&app.jobs[&id].status)
        };
        assert_eq!(status_for("cdda-aligned.flac"), "OK");
        let mismatch = status_for("wrong-md5.flac");
        assert!(
            mismatch.starts_with("FAILED: MISMATCH"),
            "wrong-md5.flac should report a verify mismatch, got {mismatch:?}"
        );
    }

    /// The real gap this milestone found: `Queue::submit` checks one `CancelToken` shared
    /// for the queue's whole life, so a Cancel press with nothing in flight would otherwise
    /// leave every *future* Run silently cancelling its jobs before they ever ran, since
    /// `lh-gui` (unlike every `lh-cli` batch, which builds a fresh `Queue` per invocation)
    /// keeps one `Queue` for the app's whole life (`docs/gui.md` §1). `run_operation`
    /// resets the token before submitting; this proves that reset actually lets jobs run
    /// rather than merely compiling.
    #[test]
    fn cancelling_with_nothing_in_flight_does_not_disable_the_next_run() {
        let (mut app, _) = App::boot();
        app.scan(&fixtures_dir());
        app.queue.cancel();
        app.run_operation(Operation::Sbe);

        let total = app.working_set.as_ref().unwrap().files.len();
        let rx = app.queue.events();
        drain(&mut app, &rx, total);

        for entry in app.jobs.values() {
            assert!(
                !matches!(entry.status, JobStatus::Cancelled),
                "{} was cancelled even though run_operation should have reset the token",
                entry.label
            );
        }
    }

    /// G3's real evidence: `Operation::Convert(ConvertTarget::Wav)` moves a real FLAC
    /// fixture through the queue, `to_wav_with_progress`, and back into `App` state —
    /// writing an actual `.wav` beside the source, reporting it "checked against source"
    /// (the fixture carries a STREAMINFO MD5), and appending its `Provenance::render()`
    /// text to `App::log`. Run against a copy in a tempdir rather than the fixtures dir
    /// itself, since a real write must not touch the read-only checked-in corpus.
    #[test]
    fn running_convert_to_wav_through_the_real_queue_writes_a_checked_file_and_logs_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cdda-aligned.flac");
        std::fs::copy(fixtures_dir().join("cdda-aligned.flac"), &src).unwrap();

        let (mut app, _) = App::boot();
        app.scan(dir.path());
        app.run_operation(Operation::Convert(ConvertTarget::Wav));

        let total = app.working_set.as_ref().unwrap().files.len();
        let rx = app.queue.events();
        drain(&mut app, &rx, total);

        let id = app.latest_job_by_path[&src];
        assert_eq!(
            status_label(&app.jobs[&id].status),
            "WROTE cdda-aligned.wav"
        );
        assert!(
            dir.path().join("cdda-aligned.wav").exists(),
            "convert should have written cdda-aligned.wav beside the source"
        );
        assert_eq!(
            app.log.len(),
            1,
            "one finished convert job should log one provenance entry, got {:?}",
            app.log
        );
        assert!(
            app.log[0].contains("FLAC → WAV"),
            "log entry should name the conversion, got {:?}",
            app.log[0]
        );
    }

    /// The other direction, through the reference `flac` binary discovered from
    /// `App::tools` — real evidence `run_operation`'s `flac_tool` plumbing actually reaches
    /// `to_flac_cancellable`, not just that it compiles. Skips (rather than failing) when
    /// `flac` is not installed, the same convention `lh-core/tests/convert.rs` uses.
    #[test]
    fn running_convert_to_flac_through_the_real_queue_writes_a_checked_file() {
        if Registry::discover_one(ToolId::Flac)
            .require(ToolId::Flac)
            .is_err()
        {
            eprintln!("skipping: flac not found");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cdda-aligned.wav");
        std::fs::copy(fixtures_dir().join("cdda-aligned.wav"), &src).unwrap();

        let (mut app, _) = App::boot();
        app.scan(dir.path());
        app.run_operation(Operation::Convert(ConvertTarget::Flac));

        let total = app.working_set.as_ref().unwrap().files.len();
        let rx = app.queue.events();
        drain(&mut app, &rx, total);

        let id = app.latest_job_by_path[&src];
        let status = status_label(&app.jobs[&id].status);
        assert_eq!(status, "WROTE cdda-aligned.flac", "got {status:?}");
        assert!(dir.path().join("cdda-aligned.flac").exists());
        assert!(app.log.iter().any(|e| e.contains("WAV → FLAC")));
    }

    /// The gap `run_operation`'s pre-filter closes: `lh-cli`'s `cmd_convert` treats a file
    /// already in the target format as a silent skip (`ConvertOutcome::Skipped`), not a
    /// failure. Converting a working set that is *already* WAV to WAV must not submit a
    /// job, and must not leave a FAILED row for a file nothing was wrong with.
    #[test]
    fn converting_to_the_format_a_file_is_already_in_submits_no_job() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cdda-aligned.wav");
        std::fs::copy(fixtures_dir().join("cdda-aligned.wav"), &src).unwrap();

        let (mut app, _) = App::boot();
        app.scan(dir.path());
        app.run_operation(Operation::Convert(ConvertTarget::Wav));

        assert!(
            app.jobs.is_empty(),
            "no job should have been submitted for a file already in the target format, got {:?}",
            app.jobs.keys().collect::<Vec<_>>()
        );
    }

    /// S2's core behaviour change (`docs/gui-shell.md` §9): `run_operation` reads
    /// `App::selected`, not every file in the working set. Untick one fixture, run Sbe
    /// (cheap, needs no external tool) over the rest, and confirm the unticked file never
    /// got a job at all — not a skipped one, the same "no job" treatment as a file already
    /// in a convert's target format.
    #[test]
    fn run_operation_submits_no_job_for_an_unticked_file() {
        let (mut app, _) = App::boot();
        app.scan(&fixtures_dir());

        let untick = app
            .working_set
            .as_ref()
            .unwrap()
            .files
            .iter()
            .find(|f| f.file_name() == "cdda-aligned.flac")
            .expect("cdda-aligned.flac must be in the fixture corpus")
            .path
            .clone();
        app.selected.remove(&untick);

        let total_selected = app.selected.len();
        app.run_operation(Operation::Sbe);

        let rx = app.queue.events();
        drain(&mut app, &rx, total_selected);

        assert!(
            !app.latest_job_by_path.contains_key(&untick),
            "cdda-aligned.flac was unticked and should not have gotten a job"
        );
        assert!(
            app.latest_job_by_path.len() == total_selected,
            "expected exactly one job per ticked file, got {}",
            app.latest_job_by_path.len()
        );
    }

    #[test]
    fn tool_discovery_renders_every_id_one_line_each() {
        let tools = Registry::discover();
        let lines: Vec<String> = tools.entries().map(|(id, d)| tool_line(id, d)).collect();
        assert_eq!(lines.len(), ToolId::ALL.len());
        for (line, id) in lines.iter().zip(ToolId::ALL) {
            assert!(
                line.starts_with(id.name()),
                "{line:?} should start with {}",
                id.name()
            );
        }
    }

    #[test]
    fn format_bytes_matches_kib_mib() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(16 * 1024), "16.0 KiB");
        assert_eq!(format_bytes(16 * 1024 * 1024), "16.0 MiB");
    }

    /// A folder with a couple of small synthetic files, not the read-only audio fixture
    /// corpus — torrent create/check do not care what the bytes are, and a real write
    /// (the `.torrent` itself) must not touch the checked-in corpus, same reasoning G3's
    /// convert tests already established.
    fn torrent_source_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("track1.bin"), vec![1u8; 40_000]).unwrap();
        std::fs::write(dir.path().join("track2.bin"), vec![2u8; 20_000]).unwrap();
        dir
    }

    /// G4's real evidence for C5: `App::run_torrent_create` moves a real folder through
    /// `create_with_progress` and back into `App` state, producing an actual `.torrent`
    /// beside the source with the piece count/length the payload implies.
    #[test]
    fn running_torrent_create_through_the_real_queue_writes_a_torrent() {
        let dir = torrent_source_dir();

        let (mut app, _) = App::boot();
        app.scan(dir.path());
        assert_eq!(
            app.working_root.as_deref(),
            Some(dir.path()),
            "scanning must record the root torrent create builds from"
        );
        app.run_torrent_create();

        let rx = app.queue.events();
        drain(&mut app, &rx, 1);

        assert_eq!(app.jobs.len(), 1);
        let status = status_label(&app.jobs.values().next().unwrap().status);
        assert!(
            status.starts_with("WROTE") && status.contains("2 files"),
            "got {status:?}"
        );
        let source = dir.path().canonicalize().unwrap();
        let torrent_path = lh_core::torrent::default_output(&source).unwrap();
        assert!(
            torrent_path.exists(),
            "expected a .torrent written at {}",
            torrent_path.display()
        );
    }

    /// S2's other half (`docs/gui-shell.md` §4, §9): Torrent → Create describes the folder
    /// as it exists on disk and must ignore the selection entirely — filtering by ticked
    /// rows would produce a `.torrent` whose file list did not match its directory, which is
    /// a broken torrent, not a subset. Deselect everything and confirm the torrent still
    /// names all real files.
    #[test]
    fn run_torrent_create_ignores_the_selection_entirely() {
        let dir = torrent_source_dir();

        let (mut app, _) = App::boot();
        app.scan(dir.path());
        app.selected.clear();
        app.run_torrent_create();

        let rx = app.queue.events();
        drain(&mut app, &rx, 1);

        let status = status_label(&app.jobs.values().next().unwrap().status);
        assert!(
            status.starts_with("WROTE") && status.contains("2 files"),
            "torrent create should have used every file on disk regardless of ticking, got {status:?}"
        );
    }

    /// G4's real evidence for T4: create a real torrent, then check it against its own
    /// source folder through `App::run_torrent_check` and the real queue — `Verdict::Complete`
    /// end to end, plus the per-file table `torrent_check_rows` feeds the results panel.
    #[test]
    fn running_torrent_check_through_the_real_queue_reports_complete_and_fills_the_table() {
        let dir = torrent_source_dir();
        let source = dir.path().canonicalize().unwrap();
        let torrent_path = lh_core::torrent::default_output(&source).unwrap();

        let (mut app, _) = App::boot();
        app.scan(dir.path());
        app.run_torrent_create();
        let rx = app.queue.events();
        drain(&mut app, &rx, 1);
        assert!(
            torrent_path.exists(),
            "setup: torrent create should have written {}",
            torrent_path.display()
        );

        app.torrent_check_path = Some(torrent_path);
        app.torrent_check_against = dir.path().display().to_string();
        app.run_torrent_check();
        drain(&mut app, &rx, 1);

        let check_status = status_label(&app.jobs.values().nth(1).unwrap().status);
        assert_eq!(check_status, "OK", "got {check_status:?}");
        assert_eq!(app.torrent_check_rows.len(), 2, "one row per real file");
        for row in &app.torrent_check_rows {
            assert_eq!(row.label, "OK", "{}: {}", row.path, row.detail);
        }
    }

    /// The synchronous check `run_torrent_create` does before submitting anything: an
    /// unresolvable tracker spec must fail up front, exactly like `lh-cli`'s own
    /// `cmd_torrent_create`, not become a job the queue has to fail instead.
    #[test]
    fn an_unresolvable_tracker_spec_is_rejected_before_any_job_is_submitted() {
        let dir = torrent_source_dir();
        let (mut app, _) = App::boot();
        app.scan(dir.path());
        app.torrent_tracker_input = "not-a-real-tracker-id".to_string();
        app.run_torrent_create();

        assert!(app.error.is_some(), "expected an error, got none");
        assert!(
            app.jobs.is_empty(),
            "no job should have been submitted for an unresolvable tracker"
        );
    }

    /// `Message::PathDropped` routes by extension (`update`'s own match arm) — a dropped
    /// `.torrent` must reach the check panel, not be handed to `App::scan` as though it
    /// were an audio folder.
    #[test]
    fn dropping_a_dot_torrent_file_is_routed_to_the_check_panel() {
        let torrent_path = PathBuf::from("/tmp/does-not-need-to-exist/show.torrent");
        let (mut app, _) = App::boot();
        // `pick_torrent` will fail to read it and set `app.error`; the routing itself is
        // what this test checks, not a successful parse.
        let _ = update(&mut app, Message::PathDropped(torrent_path.clone()));

        assert_eq!(app.torrent_check_path, Some(torrent_path));
        assert!(
            app.working_set.is_none(),
            "must not have been treated as a folder to scan"
        );
    }

    /// S3's real evidence for Checksum → Create (`docs/gui-shell.md` §6): select just the
    /// four fixtures `reference.ffp` has entries for, run Checksum → Create at FFP through
    /// the real queue, and confirm the written file's entries match the reference tool's
    /// own output exactly — the same oracle `lh-core/tests/corpus.rs`'s
    /// `ffp_matches_the_reference_tools` uses, one layer up through `App` state instead of
    /// calling `checksum::ffp` directly.
    #[test]
    fn running_checksum_create_through_the_real_queue_writes_a_checksum_file_matching_the_reference_ffp()
     {
        let reference = ChecksumFile::read(ChecksumKind::Ffp, &fixtures_dir().join("reference.ffp"))
            .expect("reference.ffp should parse");
        let reference_names: HashSet<String> = reference
            .entries
            .iter()
            .map(|e| e.file_name.clone())
            .collect();

        let (mut app, _) = App::boot();
        app.scan(&fixtures_dir());
        app.selected = app
            .working_set
            .as_ref()
            .unwrap()
            .files
            .iter()
            .filter(|f| reference_names.contains(&f.file_name()))
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(
            app.selected.len(),
            reference.entries.len(),
            "every reference.ffp name must be a real fixture"
        );

        let out_dir = tempfile::tempdir().unwrap();
        let output = out_dir.path().join("out.ffp");
        app.checksum_kind = ChecksumKind::Ffp;
        app.checksum_output = output.display().to_string();
        app.run_checksum_create();

        let total = app.selected.len();
        let rx = app.queue.events();
        drain(&mut app, &rx, total);

        let written = ChecksumFile::read(ChecksumKind::Ffp, &output).expect("output must parse");
        let written_map: HashMap<String, [u8; 16]> = written
            .entries
            .iter()
            .map(|e| (e.file_name.clone(), e.digest))
            .collect();
        let reference_map: HashMap<String, [u8; 16]> = reference
            .entries
            .iter()
            .map(|e| (e.file_name.clone(), e.digest))
            .collect();
        assert_eq!(written_map, reference_map);
    }

    /// S3's real evidence for Checksum → Check: the checked-in `reference.ffp`
    /// (`lh-core/tests/fixtures`) names real fixtures, so checking it against them end to
    /// end through the real queue must report OK for every entry.
    #[test]
    fn running_checksum_check_through_the_real_queue_against_the_reference_ffp_reports_ok_for_every_entry()
     {
        let (mut app, _) = App::boot();
        app.pick_checksum_file(fixtures_dir().join("reference.ffp"));
        assert_eq!(app.checksum_check_kind, Some(ChecksumKind::Ffp));
        let total = app.checksum_check_file.as_ref().unwrap().entries.len();
        assert!(total > 0, "reference.ffp is empty");

        app.run_checksum_check();
        let rx = app.queue.events();
        drain(&mut app, &rx, total);

        assert_eq!(app.checksum_check_rows.len(), total);
        for row in &app.checksum_check_rows {
            assert_eq!(row.label, "OK", "{}: {}", row.path, row.detail);
        }
    }

    /// Same, against `reference.st5` — a different kind (decoded-audio MD5, not the
    /// STREAMINFO one) and a different oracle (real shntool, `lh-core/tests/corpus.rs`'s
    /// own note on why this file exists), through the same Checksum → Check path.
    #[test]
    fn running_checksum_check_through_the_real_queue_against_the_reference_st5_reports_ok_for_every_entry()
     {
        let (mut app, _) = App::boot();
        app.pick_checksum_file(fixtures_dir().join("reference.st5"));
        assert_eq!(app.checksum_check_kind, Some(ChecksumKind::St5));
        let total = app.checksum_check_file.as_ref().unwrap().entries.len();
        assert!(total > 0, "reference.st5 is empty");

        app.run_checksum_check();
        let rx = app.queue.events();
        drain(&mut app, &rx, total);

        assert_eq!(app.checksum_check_rows.len(), total);
        for row in &app.checksum_check_rows {
            assert_eq!(row.label, "OK", "{}: {}", row.path, row.detail);
        }
    }

    /// The two failure paths `cmd_check` prints as `MISSING`/`MISMATCH`
    /// (`lh-cli/src/main.rs`) — a synthetic checksum file naming one file that does not
    /// exist beside it and one whose digest does not match the real file that does.
    #[test]
    fn checksum_check_reports_missing_and_mismatch_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"hello").unwrap();
        let content = format!("{}  a.bin\n{}  missing.bin\n", "0".repeat(32), "f".repeat(32));
        let checksum_path = dir.path().join("check.md5");
        std::fs::write(&checksum_path, &content).unwrap();

        let (mut app, _) = App::boot();
        app.pick_checksum_file(checksum_path);
        assert_eq!(app.checksum_check_kind, Some(ChecksumKind::Md5));
        app.run_checksum_check();

        let rx = app.queue.events();
        drain(&mut app, &rx, 2);

        let mut by_path: HashMap<String, &'static str> = app
            .checksum_check_rows
            .iter()
            .map(|r| (r.path.clone(), r.label))
            .collect();
        assert_eq!(by_path.remove("a.bin"), Some("MISMATCH"));
        assert_eq!(by_path.remove("missing.bin"), Some("MISSING"));
    }

    /// The extension-inference gap `docs/gui-shell.md` §10 Q4 leaves open: an extension
    /// naming none of `.ffp`/`.md5`/`.st5` is an error, matching `cmd_check`'s own `bail!`,
    /// not a silent guess.
    #[test]
    fn picking_a_checksum_file_with_an_unrecognized_extension_sets_an_error() {
        let (mut app, _) = App::boot();
        app.pick_checksum_file(PathBuf::from("/tmp/does-not-need-to-exist/show.txt"));

        assert!(app.error.is_some(), "expected an error, got none");
        assert_eq!(app.checksum_check_kind, None);
        assert!(app.checksum_check_file.is_none());
    }

    /// `Message::PathDropped` routes a `.ffp`/`.md5`/`.st5` to the checksum check panel,
    /// the same way it already routes a `.torrent` to the torrent check panel.
    #[test]
    fn dropping_a_checksum_file_is_routed_to_the_checksum_check_panel() {
        let path = PathBuf::from("/tmp/does-not-need-to-exist/reference.ffp");
        let (mut app, _) = App::boot();
        let _ = update(&mut app, Message::PathDropped(path.clone()));

        assert_eq!(app.checksum_check_path, Some(path));
        assert!(
            app.working_set.is_none(),
            "must not have been treated as a folder to scan"
        );
    }
}
