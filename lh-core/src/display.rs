//! Plain-string display formatters shared by every front end — no terminal or widget
//! types, just the `String` each one already agreed on before this module existed.

/// "piece 3" for one, "pieces 3, 5, 9" for several — how a bad-pieces list reads in
/// `lh torrent check` and the report tables behind it.
pub fn pieces_phrase(pieces: &[u32]) -> String {
    if pieces.len() == 1 {
        format!("piece {}", pieces[0])
    } else {
        let list: Vec<String> = pieces.iter().map(u32::to_string).collect();
        format!("pieces {}", list.join(", "))
    }
}

/// A byte count as `1.5 MiB`-style text, binary units throughout.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Torrent creation dates matter for identifying an old seed, so show a date rather than
/// an epoch. Civil-from-days, so this needs no date library.
pub fn date(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `m:ss.mmm` — `lh info`'s duration column.
pub fn duration_precise(secs: f64) -> String {
    let millis = (secs * 1000.0).round() as u64;
    let (m, rem) = (millis / 60_000, millis % 60_000);
    format!("{m}:{s:02}.{ms:03}", s = rem / 1000, ms = rem % 1000)
}

/// `m:ss` — the GUI's file-table duration column, coarser than [`duration_precise`]
/// because there is no room in the table for milliseconds.
pub fn duration_short(secs: f64) -> String {
    let total = secs.round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_short_matches_mm_ss() {
        assert_eq!(duration_short(0.4), "0:00");
        assert_eq!(duration_short(65.6), "1:06");
    }

    #[test]
    fn bytes_matches_kib_mib() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(16 * 1024), "16.0 KiB");
        assert_eq!(bytes(16 * 1024 * 1024), "16.0 MiB");
    }
}
