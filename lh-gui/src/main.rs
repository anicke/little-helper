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

mod job;

use iced::widget::{
    Column, button, checkbox, column, container, pick_list, row, scrollable, text, text_input,
};
use iced::{Element, Length, Subscription, Task};
use job::JobOutcome;
use lh_core::analysis::{self, Sbe};
use lh_core::checksum::{self, ChecksumKind};
use lh_core::convert::{self, Conversion, EncodeOpts};
use lh_core::job::{JobId, Queue};
use lh_core::model::AudioFormat;
use lh_core::scan::{self, WorkingSet};
use lh_core::tools::{Discovery, Registry, ToolId};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// One of the operations `lh-cli` already exposes. G2 wired verify/checksum/sbe, which
/// need no per-run options beyond the file itself; G3 adds convert, which needs a
/// direction (`ConvertTarget`) and reads `App::overwrite`, the one option it does need.
/// The torrent panels need a tracker list, so they still wait for G4 rather than growing
/// this into a catch-all now.
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

impl Operation {
    const ALL: [Operation; 7] = [
        Operation::Verify,
        Operation::Checksum(ChecksumKind::Ffp),
        Operation::Checksum(ChecksumKind::Md5),
        Operation::Checksum(ChecksumKind::St5),
        Operation::Sbe,
        Operation::Convert(ConvertTarget::Wav),
        Operation::Convert(ConvertTarget::Flac),
    ];
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Operation::Verify => "Verify",
            Operation::Checksum(ChecksumKind::Ffp) => "FFP checksum",
            Operation::Checksum(ChecksumKind::Md5) => "MD5 checksum",
            Operation::Checksum(ChecksumKind::St5) => "ST5 checksum",
            Operation::Sbe => "SBE",
            Operation::Convert(ConvertTarget::Wav) => "Convert to WAV",
            Operation::Convert(ConvertTarget::Flac) => "Convert to FLAC",
        })
    }
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
    tools: Registry,
    error: Option<String>,
    queue: Queue<JobOutcome>,
    operation: Operation,
    /// Convert's one option (`lh-cli`'s `--force`) — whether to overwrite an output that
    /// already exists. Read only by `Operation::Convert`; harmless while any other
    /// operation is selected.
    overwrite: bool,
    jobs: BTreeMap<JobId, JobEntry>,
    latest_job_by_path: HashMap<PathBuf, JobId>,
    /// The log/audit pane: `Provenance::render()` text from every finished job that
    /// produced one, oldest first (`docs/gui.md` §2, §5 open question 4 — resolved for G3
    /// as "the rendered strings from every finished job, in order," no new `report/`
    /// module).
    log: Vec<String>,
}

#[derive(Debug, Clone)]
enum Message {
    PathInputChanged(String),
    BrowsePressed,
    FolderPicked(Option<PathBuf>),
    ScanPressed,
    PathDropped(PathBuf),
    OperationSelected(Operation),
    OverwriteToggled(bool),
    RunPressed,
    CancelPressed,
    Job(job::JobUpdate),
    ExportLogPressed,
    LogExportPathPicked(Option<PathBuf>),
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        (
            App {
                path_input: String::new(),
                working_set: None,
                tools: Registry::discover(),
                error: None,
                queue: Queue::new(),
                operation: Operation::Verify,
                overwrite: false,
                jobs: BTreeMap::new(),
                latest_job_by_path: HashMap::new(),
                log: Vec::new(),
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
                self.working_set = Some(set);
            }
            Err(e) => {
                self.error = Some(e.to_string());
                self.working_set = None;
            }
        }
    }

    /// Submits one job per file in the working set for the selected operation. Resets the
    /// queue's `CancelToken` first: `Queue::submit` checks that same shared token for every
    /// job for the queue's whole life (`lh-core/src/job/mod.rs`), so without a reset here, a
    /// single Cancel press would silently stop every future Run from ever executing a job —
    /// a gap only a long-lived queue like this one's can hit (`CancelToken::reset`'s doc,
    /// `docs/gui.md`'s G2 notes).
    ///
    /// A convert to FLAC needs the reference `flac` binary; discovered once at boot
    /// (`self.tools`), not re-discovered per run. Missing it fails the whole Run up front,
    /// the same as `lh-cli`'s own `cmd_convert` — before converting half a show, not after.
    fn run_operation(&mut self) {
        let Some(set) = &self.working_set else {
            return;
        };
        if let Operation::Convert(ConvertTarget::Flac) = self.operation
            && let Err(e) = self.tools.require(ToolId::Flac)
        {
            self.error = Some(e.to_string());
            return;
        }
        self.error = None;
        self.queue.cancel_token().reset();
        let operation = self.operation;
        let overwrite = self.overwrite;
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
        for file in &set.files {
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
            self.jobs.insert(
                id,
                JobEntry {
                    label,
                    status: JobStatus::Running { done: 0, total: 0 },
                },
            );
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
            }
            job::JobUpdate::Cancelled { id } => {
                if let Some(entry) = self.jobs.get_mut(&id) {
                    entry.status = JobStatus::Cancelled;
                }
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
            app.path_input = path.display().to_string();
            app.scan(&path);
        }
        Message::OperationSelected(op) => app.operation = op,
        Message::OverwriteToggled(v) => app.overwrite = v,
        Message::RunPressed => app.run_operation(),
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
    }
    Task::none()
}

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

    let table = file_table(app);
    let operations = operation_panel(app);
    let jobs_panel = job_queue_panel(&app.jobs);
    let log = log_panel(&app.log);
    let tools_panel = tools_panel(&app.tools);

    container(
        column![
            path_bar,
            error,
            table,
            operations,
            jobs_panel,
            log,
            tools_panel
        ]
        .spacing(12)
        .padding(12),
    )
    .into()
}

fn file_table(app: &App) -> Element<'_, Message> {
    let Some(set) = app.working_set.as_ref() else {
        return text("Drop a folder here, or use Browse / Scan.").into();
    };

    let header = row![
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
        rows = rows.push(
            row![
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

fn operation_panel(app: &App) -> Element<'_, Message> {
    let picker = pick_list(
        &Operation::ALL[..],
        Some(app.operation),
        Message::OperationSelected,
    );
    let overwrite = checkbox(app.overwrite)
        .label("Overwrite existing outputs")
        .on_toggle(Message::OverwriteToggled);
    let run =
        button("Run").on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button("Cancel").on_press(Message::CancelPressed);

    row![text("Operation:"), picker, overwrite, run, cancel]
        .spacing(8)
        .into()
}

/// The log/audit pane — `Provenance::render()` text from every finished job that produced
/// one, oldest first, plus an Export button that writes them to a text file the user
/// picks (`docs/gui.md` §2). Not cleared between runs, same as the job-queue panel.
fn log_panel(log: &[String]) -> Element<'_, Message> {
    let export = button("Export log...")
        .on_press_maybe((!log.is_empty()).then_some(Message::ExportLogPressed));
    let mut list = Column::new()
        .spacing(4)
        .push(row![text("Log"), export].spacing(8));
    for entry in log {
        for line in entry.lines() {
            list = list.push(text(line.to_string()));
        }
    }
    scrollable(list).height(Length::FillPortion(2)).into()
}

/// Aggregate `N of M done` plus one line per job, oldest first (`BTreeMap<JobId, _>` order
/// — `docs/gui.md` §4) — the job-queue panel `PLAN.md` §4 names. Unlike `lh-cli`'s batch
/// commands, entries are not cleared between runs: the queue is long-lived (`docs/gui.md`
/// §1), so a second Run's jobs simply join the first's in this same list.
fn job_queue_panel(jobs: &BTreeMap<JobId, JobEntry>) -> Element<'_, Message> {
    let total = jobs.len();
    let done = jobs
        .values()
        .filter(|e| !matches!(e.status, JobStatus::Running { .. }))
        .count();

    let mut list = Column::new()
        .spacing(4)
        .push(text(format!("Jobs: {done} of {total} done")));
    for entry in jobs.values() {
        list = list.push(text(format!(
            "{}: {}",
            entry.label,
            status_label(&entry.status)
        )));
    }

    scrollable(list).height(Length::FillPortion(2)).into()
}

fn tools_panel(tools: &Registry) -> Element<'_, Message> {
    let mut list = Column::new().spacing(4).push(text("Tools"));
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
    iced::application(App::boot, update, view)
        .subscription(subscription)
        .title("Little Helper")
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
        app.operation = Operation::Verify;
        app.run_operation();

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
        app.operation = Operation::Sbe;
        app.run_operation();

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
        app.operation = Operation::Convert(ConvertTarget::Wav);
        app.run_operation();

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
        app.operation = Operation::Convert(ConvertTarget::Flac);
        app.run_operation();

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
        app.operation = Operation::Convert(ConvertTarget::Wav);
        app.run_operation();

        assert!(
            app.jobs.is_empty(),
            "no job should have been submitted for a file already in the target format, got {:?}",
            app.jobs.keys().collect::<Vec<_>>()
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
}
