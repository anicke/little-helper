//! Little Helper desktop application — milestone M3, see `docs/gui.md`.
//!
//! G1: scaffold and file table only. A folder becomes a `WorkingSet` (`lh-core::scan`) the
//! same way `lh scan` would use it; there is no job queue yet (`docs/gui.md` §4 G2), so
//! every operation here is read-only.

use iced::widget::{Column, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Subscription, Task};
use lh_core::analysis::{self, Sbe};
use lh_core::scan::{self, WorkingSet};
use lh_core::tools::{Discovery, Registry, ToolId};
use std::path::{Path, PathBuf};

struct App {
    path_input: String,
    working_set: Option<WorkingSet>,
    tools: Registry,
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    PathInputChanged(String),
    BrowsePressed,
    FolderPicked(Option<PathBuf>),
    ScanPressed,
    PathDropped(PathBuf),
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        (
            App {
                path_input: String::new(),
                working_set: None,
                tools: Registry::discover(),
                error: None,
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

    let table = file_table(app.working_set.as_ref());

    let tools_panel = tools_panel(&app.tools);

    container(
        column![path_bar, error, table, tools_panel]
            .spacing(12)
            .padding(12),
    )
    .into()
}

fn file_table(working_set: Option<&WorkingSet>) -> Element<'_, Message> {
    let Some(set) = working_set else {
        return text("Drop a folder here, or use Browse / Scan.").into();
    };

    let header = row![
        text("Name").width(Length::FillPortion(4)),
        text("Format").width(Length::FillPortion(1)),
        text("Duration").width(Length::FillPortion(1)),
        text("Rate/Bits/Ch").width(Length::FillPortion(2)),
        text("SBE").width(Length::FillPortion(2)),
    ]
    .spacing(8);

    let mut rows = Column::new().spacing(4).push(header);
    for file in &set.files {
        let info = &file.stream_info;
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
            ]
            .spacing(8),
        );
    }
    for (path, reason) in &set.skipped {
        rows = rows.push(text(format!("{} — skipped: {reason}", path.display())));
    }

    scrollable(rows).height(Length::FillPortion(3)).into()
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

fn subscription(_app: &App) -> Subscription<Message> {
    iced::event::listen_with(|event, _status, _window| match event {
        iced::Event::Window(iced::window::Event::FileDropped(path)) => {
            Some(Message::PathDropped(path))
        }
        _ => None,
    })
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
