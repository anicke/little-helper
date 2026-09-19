use crate::*;
use iced::Element;
use iced::widget::{button, checkbox, pick_list, row, text};
use iced_fonts::lucide;

pub(crate) fn convert_panel(app: &App) -> Element<'_, Message> {
    let direction = pick_list(
        &[ConvertTarget::Flac, ConvertTarget::Wav][..],
        Some(app.convert_target),
        Message::ConvertTargetSelected,
    );
    let overwrite = checkbox(app.convert_overwrite)
        .label("Overwrite existing outputs")
        .on_toggle(Message::ConvertOverwriteToggled);
    let run = button(labelled(lucide::play(), "Run"))
        .on_press_maybe(app.working_set.is_some().then_some(Message::RunPressed));
    let cancel = button(labelled(lucide::circle_x(), "Cancel"))
        .on_press(Message::CancelPressed)
        .style(button::secondary);

    row![text("Direction:"), direction, overwrite, run, cancel]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
}
