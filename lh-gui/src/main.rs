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

mod app;
mod areas;
mod job;
mod widgets;

#[cfg(test)]
mod tests;

use areas::*;
use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Length, Subscription, Task};
use job::JobOutcome;
use lh_core::checksum::{ChecksumFile, ChecksumKind};
use lh_core::job::{JobId, Queue};
use lh_core::scan::WorkingSet;
use lh_core::tools::Registry;
use lh_core::torrent::{Metainfo, Passkeys, TrackerList};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use widgets::*;

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
