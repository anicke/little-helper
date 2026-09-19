use crate::*;
use iced::widget::{Column, button, checkbox, column, row, scrollable, text, text_input};
use iced::{Element, Length};
use iced_fonts::lucide;
use lh_core::display;

/// `docs/torrent-creation.md` C5: folder (`App::working_root`, already scanned above) →
/// trackers → create, with piece progress through the same job-queue panel every other
/// operation uses. Pre-flight (C4) is postponed there and stays out of this panel too.
pub(crate) fn torrent_create_panel(app: &App) -> Element<'_, Message> {
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
    let create = button(labelled(lucide::magnet(), "Create torrent")).on_press_maybe(
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
pub(crate) fn torrent_check_panel(app: &App) -> Element<'_, Message> {
    let torrent_label = match &app.torrent_check_path {
        Some(p) => p.display().to_string(),
        None => "No .torrent chosen — Browse or drop one on the window.".to_string(),
    };
    let browse = button(labelled(lucide::folder(), "Browse .torrent..."))
        .on_press(Message::TorrentCheckBrowsePressed)
        .style(button::secondary);

    let info: Element<'_, Message> = match &app.torrent_check_meta {
        Some(meta) => text(format!(
            "{}  {}  {} files  {} pieces of {}",
            meta.name,
            meta.info_hash_hex(),
            meta.real_files().count(),
            meta.pieces.len(),
            display::bytes(meta.piece_length),
        ))
        .into(),
        None => text("").into(),
    };

    let against = text_input("Folder to check against", &app.torrent_check_against)
        .on_input(Message::TorrentCheckAgainstChanged);
    let against_browse = button(labelled(lucide::folder(), "Browse folder..."))
        .on_press(Message::TorrentCheckAgainstBrowsePressed)
        .style(button::secondary);
    let quick = checkbox(app.torrent_check_quick)
        .label("Quick (sizes only)")
        .on_toggle(Message::TorrentCheckQuickToggled);
    let run = button(labelled(lucide::file_search(), "Check")).on_press_maybe(
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
pub(crate) fn file_rows_panel(title: &str, rows: &[job::FileRow]) -> Element<'static, Message> {
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
