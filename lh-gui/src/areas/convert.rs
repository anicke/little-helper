use crate::*;
use iced::Element;
use iced::widget::{button, checkbox, pick_list, row, text};

pub(crate) fn convert_panel(app: &App) -> Element<'_, Message> {
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
