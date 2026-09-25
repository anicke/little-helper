use crate::*;
use iced::Element;
use iced::widget::{Column, column, text};
use lh_core::tools::{Discovery, Registry, ToolId};

pub(crate) fn about_panel() -> Element<'static, Message> {
    column![
        text("Lossless Little Helper"),
        text(format!("v{}", env!("CARGO_PKG_VERSION"))),
    ]
    .spacing(4)
    .into()
}

pub(crate) fn tools_panel(tools: &Registry) -> Element<'_, Message> {
    // Labelled "Binaries" here, matching the rail row (`docs/gui-shell.md` §3) — TLH's own
    // "Tools" menu means repair (Fix SBEs, Strip header, Create skt), a different, v0.2
    // thing. `Registry`, `ToolId` and `lh tools` keep their names; this is a GUI label.
    let mut list = Column::new().spacing(4).push(text("Binaries"));
    for (id, discovery) in tools.entries() {
        list = list.push(text(tool_line(id, discovery)));
    }
    list.into()
}

pub(crate) fn tool_line(id: ToolId, discovery: &Discovery) -> String {
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
