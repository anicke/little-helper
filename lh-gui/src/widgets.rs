use crate::*;
use iced::widget::{Column, button, checkbox, column, container, row, scrollable, table, text};
use iced::{Element, Length};
use lh_core::analysis::{self, Sbe};
use lh_core::display;
use lh_core::job::JobId;
use lh_core::model::AudioFile;
use std::collections::BTreeMap;

/// The left rail — `Area::RAIL`'s group headers and rows, in TLH's own menu order
/// (`docs/gui-shell.md` §3). Group headers are plain text, not buttons: TLH's own
/// `&Format` opens a menu but performs nothing, and a header that looks clickable and is
/// not is worse than one that plainly is not.
pub(crate) fn rail(app: &App) -> Element<'_, Message> {
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
pub(crate) fn rail_row(label: &str, area: Area, selected: bool) -> Element<'_, Message> {
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

pub(crate) fn run_cancel_row(app: &App) -> Element<'_, Message> {
    let run =
        button("Run").on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button("Cancel").on_press(Message::CancelPressed);
    row![run, cancel].spacing(8).into()
}

/// The dock (`docs/gui-shell.md` §4): a header that is always the aggregate `N of M done`
/// plus Cancel, and a `Jobs | Log` toggle that switches only the body — the two bodies
/// want the same vertical space, and the aggregate line must never be one click away.
pub(crate) fn dock(app: &App) -> Element<'_, Message> {
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

pub(crate) fn dock_tab_button(label: &str, tab: DockTab, selected: bool) -> Element<'_, Message> {
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
pub(crate) const SELECT_COLUMN: Length = Length::Fixed(24.0);

/// S4 (`docs/gui-shell.md` §7/§9): the hand-rolled `Column` of `row!`s from S1 replaced by
/// `iced::widget::table`, new in 0.14 and left out of S1 deliberately so a regression there
/// would be visibly the table's fault, not the shell move's. Rows are `AudioFile` itself —
/// it is already `Clone` — so every column view closure gets the full record, which is what
/// makes room for the encoder vendor string TLH's `lh info` has always had nowhere to put.
pub(crate) fn file_table(app: &App) -> Element<'_, Message> {
    let Some(set) = app.working_set.as_ref() else {
        return text("Drop a folder here, or use Browse / Scan.").into();
    };

    // Select-all reflects the current selection rather than being remembered separately
    // (S2, `docs/gui-shell.md` §9): checked only once every file is, so toggling it off
    // after a partial selection clears the rest instead of leaving it stuck checked.
    let all_selected =
        !set.files.is_empty() && set.files.iter().all(|f| app.selected.contains(&f.path));

    let columns = vec![
        table::column(
            checkbox(all_selected).on_toggle(Message::SelectAllToggled),
            |file: AudioFile| {
                let path = file.path.clone();
                checkbox(app.selected.contains(&file.path))
                    .on_toggle(move |checked| Message::FileToggled(path.clone(), checked))
            },
        )
        .width(SELECT_COLUMN),
        table::column(text("Name"), |file: AudioFile| text(file.file_name()))
            .width(Length::FillPortion(4)),
        table::column(text("Format"), |file: AudioFile| text(file.format.name()))
            .width(Length::FillPortion(1)),
        table::column(text("Rate/Bits/Ch"), |file: AudioFile| {
            let info = &file.stream_info;
            text(format!(
                "{} Hz / {}-bit / {}ch",
                info.sample_rate, info.bits_per_sample, info.channels
            ))
        })
        .width(Length::FillPortion(2)),
        table::column(text("Duration"), |file: AudioFile| {
            text(
                file.stream_info
                    .duration_secs()
                    .map(display::duration_short)
                    .unwrap_or_else(|| "?".to_string()),
            )
        })
        .width(Length::FillPortion(1)),
        table::column(text("Encoder"), |file: AudioFile| {
            text(file.encoder.clone().unwrap_or_else(|| "—".to_string()))
        })
        .width(Length::FillPortion(3)),
        table::column(text("SBE"), |file: AudioFile| {
            text(sbe_label(&analysis::sbe(&file.stream_info)))
        })
        .width(Length::FillPortion(2)),
        table::column(text("Status"), |file: AudioFile| {
            let status = app
                .latest_job_by_path
                .get(&file.path)
                .and_then(|id| app.jobs.get(id))
                .map(|entry| status_label(&entry.status))
                .unwrap_or_else(|| "—".to_string());
            text(status)
        })
        .width(Length::FillPortion(3)),
    ];

    let mut content = Column::new()
        .spacing(4)
        .push(table::table(columns, set.files.iter().cloned()));
    for (path, reason) in &set.skipped {
        content = content.push(text(format!("{} — skipped: {reason}", path.display())));
    }

    scrollable(content).height(Length::FillPortion(3)).into()
}

/// The log/audit pane — `Provenance::render()` text from every finished job that produced
/// one, oldest first, plus an Export button that writes them to a text file the user
/// picks (`docs/gui.md` §2). Not cleared between runs, same as the job-queue panel.
/// The log/audit pane's body — `Provenance::render()` text from every finished job that
/// produced one, oldest first, plus an Export button (`docs/gui.md` §2). The `Jobs | Log`
/// header lives in [`dock`], not here — the aggregate line applies to Jobs only.
pub(crate) fn log_panel(log: &[String]) -> Element<'_, Message> {
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
pub(crate) fn job_queue_panel(jobs: &BTreeMap<JobId, JobEntry>) -> Element<'_, Message> {
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

pub(crate) fn sbe_label(sbe: &Sbe) -> String {
    match sbe {
        Sbe::Aligned => "aligned".to_string(),
        Sbe::Misaligned { remainder_frames } => format!("misaligned ({remainder_frames} frames)"),
        Sbe::NotApplicable { reason } => format!("n/a ({reason})"),
    }
}
