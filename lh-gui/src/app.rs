use crate::*;
use iced::Task;
use job::JobOutcome;
use lh_core::analysis::{self};
use lh_core::checksum::{self, ChecksumFile, ChecksumKind, Entry};
use lh_core::convert::{self, Conversion, EncodeOpts};
use lh_core::job::{JobId, Queue};
use lh_core::model::AudioFormat;
use lh_core::scan::{self};
use lh_core::tools::{Registry, ToolId};
use lh_core::torrent::{
    CreateOpts, Metainfo, Passkeys, TrackerList, check, check_sizes, create, default_output,
    resolve,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

impl App {
    pub(crate) fn boot() -> (Self, Task<Message>) {
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
                theme_mode: iced::theme::Mode::None,
            },
            iced::system::theme().map(Message::ThemeModeChanged),
        )
    }

    /// Scans `root` (a folder, or a single file — `scan::scan` handles both, walking just
    /// the one entry in the file case) and replaces the working set. Runs on the update
    /// thread: a working set is a show or a few shows (`PLAN.md` §4), not an archive, so a
    /// synchronous walk is imperceptible — no queue is warranted for this (`docs/gui.md`
    /// §1).
    pub(crate) fn scan(&mut self, root: &Path) {
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
    pub(crate) fn pick_torrent(&mut self, path: PathBuf) {
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
    pub(crate) fn pick_checksum_file(&mut self, path: PathBuf) {
        let kind = ChecksumKind::from_path(&path);
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
    pub(crate) fn run_torrent_create(&mut self) {
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
                create(&source, &dst, &opts, &mut |done, total| {
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
    /// (`docs/torrent-verification.md` T4). `check`'s progress callback
    /// (`lh-core/src/torrent/verify.rs`) polls the same cancellation checkpoint `create`'s
    /// does (`docs/architecture-cleanup.md` A3), so Cancel stops a check already streaming
    /// pieces instead of letting it run to completion.
    pub(crate) fn run_torrent_check(&mut self) {
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
                check(&meta, &torrent_path, &against, &mut |done, total| {
                    p.report(done, total);
                    !p.is_cancelled()
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
    pub(crate) fn run_operation(&mut self, operation: Operation) -> Vec<(JobId, String)> {
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
    pub(crate) fn run_checksum_create(&mut self) {
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
    pub(crate) fn run_checksum_check(&mut self) {
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
            let base_dir = dir.clone();
            let entry = entry.clone();
            let label = entry.file_name.clone();
            let id = self.queue.submit(label.clone(), move |_p| {
                JobOutcome::ChecksumCheck(checksum::check_entry(kind, &base_dir, &entry))
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
    pub(crate) fn progress_checksum_create(&mut self, id: JobId, digest: Option<[u8; 16]>) {
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
    pub(crate) fn progress_checksum_check(&mut self, id: JobId, row: Option<job::FileRow>) {
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

    pub(crate) fn handle_job_event(&mut self, event: job::JobUpdate) {
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
pub(crate) fn convert_to_wav(
    path: &Path,
    overwrite: bool,
    p: &lh_core::job::Progress<JobOutcome>,
) -> lh_core::Result<Box<Conversion>> {
    let dst = convert::destination(path, "wav", None)?;
    convert::to_wav(path, &dst, overwrite, &mut |done, total| {
        p.report(done, total);
        !p.is_cancelled()
    })
    .map(Box::new)
}

/// [`convert_to_wav`]'s FLAC counterpart. `run_operation` never submits this for a file
/// already in FLAC, same as above.
pub(crate) fn convert_to_flac(
    path: &Path,
    tool: &lh_core::tools::Tool,
    overwrite: bool,
    p: &lh_core::job::Progress<JobOutcome>,
) -> lh_core::Result<Box<Conversion>> {
    let dst = convert::destination(path, "flac", None)?;
    convert::to_flac(
        path,
        &dst,
        tool,
        &EncodeOpts::default(),
        overwrite,
        &mut |done, total| {
            p.report(done, total);
            !p.is_cancelled()
        },
    )
    .map(Box::new)
}
