//! Torrent info: what the workspace's torrent describes — `lh-tui torrent info`'s detail
//! lines (`lh-tui/src/screens/torrent_info.rs`) as a description list, and its files as a
//! table. No job: `Metainfo::read` is one in-process parse, done when the torrent is chosen.

use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::*;
use lh_core::display;
use lh_core::torrent::Metainfo;

pub struct Files {
    rows: Vec<(SharedString, SharedString)>,
}

impl Files {
    pub fn new() -> Self {
        Files { rows: Vec::new() }
    }

    pub fn load(&mut self, meta: Option<&Metainfo>) {
        self.rows = meta
            .map(|t| {
                t.real_files()
                    .map(|f| (display::bytes(f.length).into(), f.display_path().into()))
                    .collect()
            })
            .unwrap_or_default();
    }
}

impl TableDelegate for Files {
    fn columns_count(&self, _: &App) -> usize {
        2
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("size", "Size").width(px(110.)),
            _ => Column::new("file", "File").width(px(640.)),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (size, path) = &self.rows[row_ix];
        if col_ix == 0 {
            size.clone()
        } else {
            path.clone()
        }
    }
}

/// Mirrors `lh-cli`'s `cmd_torrent_info` line by line, as the TUI's does.
pub fn details(t: &Metainfo) -> DescriptionList {
    let real = t.real_files().count();
    let pad = t.files.len() - real;
    let mut list = DescriptionList::new()
        .columns(2)
        .bordered(true)
        .item("Name", t.name.clone(), 2)
        .item("Info hash", t.info_hash_hex(), 2)
        .item(
            "Pieces",
            format!("{} × {}", t.pieces.len(), display::bytes(t.piece_length)),
            1,
        )
        .item(
            "Total",
            format!(
                "{} ({} bytes)",
                display::bytes(t.total_length),
                t.total_length
            ),
            1,
        )
        .item(
            "Files",
            if pad > 0 {
                format!("{real} ({pad} padding)")
            } else {
                real.to_string()
            },
            1,
        )
        .item(
            "Private",
            if t.private {
                "yes (BEP 27; part of the infohash)"
            } else {
                "no"
            },
            1,
        );
    if let Some(v) = &t.source {
        list = list.item("Source", v.clone(), 1);
    }
    if let Some(v) = &t.created_by {
        list = list.item("Created by", v.clone(), 1);
    }
    if let Some(ts) = t.creation_date {
        list = list.item("Created", display::date(ts), 1);
    }
    if let Some(v) = &t.comment {
        list = list.item("Comment", v.clone(), 2);
    }
    let trackers: Vec<&str> = t.trackers().collect();
    if !trackers.is_empty() {
        list = list.item("Trackers", trackers.join("\n"), 2);
    }
    list
}
