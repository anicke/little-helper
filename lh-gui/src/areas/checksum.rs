use crate::*;
use iced::Element;
use iced::widget::{button, column, row, text, text_input};
use lh_core::checksum::ChecksumKind;

/// Checksum → Create (`docs/gui-shell.md` §6, S3): the digest-per-file computation
/// `Operation::Checksum` already did in G2, a kind picker (unchanged since S1), and the
/// output path that turns those digests into a written `ChecksumFile`
/// (`App::run_checksum_create`).
pub(crate) fn checksum_create_panel(app: &App) -> Element<'_, Message> {
    let kinds = row([ChecksumKind::Ffp, ChecksumKind::Md5, ChecksumKind::St5]
        .into_iter()
        .map(|k| kind_button(k, app.checksum_kind == k)))
    .spacing(4);
    let output = text_input("Output file (.ffp/.md5/.st5)", &app.checksum_output)
        .on_input(Message::ChecksumOutputChanged);
    let browse = button("Browse...")
        .on_press(Message::ChecksumOutputBrowsePressed)
        .style(button::secondary);
    let run =
        button("Run").on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button("Cancel")
        .on_press(Message::CancelPressed)
        .style(button::secondary);

    column![
        row![text("Kind:"), kinds].spacing(8),
        row![output, browse].spacing(8),
        row![run, cancel].spacing(8),
    ]
    .spacing(8)
    .into()
}

pub(crate) fn kind_button(kind: ChecksumKind, selected: bool) -> Element<'static, Message> {
    button(text(kind.label()))
        .on_press(Message::ChecksumKindSelected(kind))
        .style(style::choice(selected))
        .into()
}

/// Checksum → Check (`docs/gui-shell.md` §6, S3): Browse or drop a `.ffp`/`.md5`/`.st5`,
/// see what `App::pick_checksum_file` parsed from it, then Check against the files beside
/// it. The per-entry table is [`file_rows_panel`], not here — same split as
/// [`torrent_check_panel`]/[`file_rows_panel`].
pub(crate) fn checksum_check_panel(app: &App) -> Element<'_, Message> {
    let label = match &app.checksum_check_path {
        Some(p) => p.display().to_string(),
        None => {
            "No checksum file chosen — Browse or drop a .ffp/.md5/.st5 on the window.".to_string()
        }
    };
    let browse = button("Browse...")
        .on_press(Message::ChecksumCheckBrowsePressed)
        .style(button::secondary);

    let info: Element<'_, Message> = match (&app.checksum_check_kind, &app.checksum_check_file) {
        (Some(kind), Some(file)) => text(format!(
            "{} entries, kind {}",
            file.entries.len(),
            kind.label()
        ))
        .into(),
        _ => text("").into(),
    };

    let run = button("Check").on_press_maybe(
        app.checksum_check_file
            .is_some()
            .then_some(Message::ChecksumCheckPressed),
    );
    let cancel = button("Cancel")
        .on_press(Message::CancelPressed)
        .style(button::secondary);

    column![
        text("Check checksum file"),
        row![text(label), browse].spacing(8),
        info,
        row![run, cancel].spacing(8),
    ]
    .spacing(8)
    .into()
}
