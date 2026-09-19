use crate::*;
use anyhow::Result;
use lh_core::analysis::{Sbe, sbe};
use lh_core::display;

pub(crate) fn cmd_info(p: &Paths) -> Result<bool> {
    let (files, clean) = collect(p)?;
    for f in &files {
        let si = &f.stream_info;
        let dur = si
            .duration_secs()
            .map(display::duration_precise)
            .unwrap_or_else(|| "?".into());
        println!(
            "{name}\n  {fmt}  {rate} Hz  {bits}-bit  {ch} ch  {dur}  {size} bytes",
            name = f.file_name(),
            fmt = f.format,
            rate = si.sample_rate,
            bits = si.bits_per_sample,
            ch = si.channels,
            size = f.file_size,
        );
        if let Some(enc) = &f.encoder {
            println!("  encoder: {enc}");
        }
        match sbe(si) {
            Sbe::Aligned => println!("  sbe: aligned"),
            Sbe::Misaligned { remainder_frames } => {
                println!("  sbe: MISALIGNED ({remainder_frames} frames past a sector boundary)")
            }
            Sbe::NotApplicable { reason } => println!("  sbe: n/a ({reason})"),
        }
    }
    Ok(clean)
}
