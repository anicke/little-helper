use crate::*;
use anyhow::Result;
use lh_core::analysis::{Verification, verify};

pub(crate) fn cmd_verify(p: &Paths) -> Result<bool> {
    let (files, mut ok) = collect(p)?;
    let results = run_batch(&files, |f, _| verify(&f.path));
    for (f, result) in &results {
        match result {
            Some(Ok(Verification::Ok)) => println!("OK        {}", f.file_name()),
            Some(Ok(Verification::Md5Mismatch { stored, computed })) => {
                ok = false;
                println!(
                    "MISMATCH  {}\n            stored   {}\n            computed {}",
                    f.file_name(),
                    hex::encode(stored),
                    hex::encode(computed)
                );
            }
            Some(Ok(Verification::NoStoredMd5 { .. })) => {
                println!(
                    "NO MD5    {} (decoded cleanly, nothing to compare)",
                    f.file_name()
                )
            }
            Some(Err(e)) => {
                ok = false;
                println!("FAILED    {}: {e}", f.file_name());
            }
            None => {
                ok = false;
                println!("CANCELLED {}", f.file_name());
            }
        }
    }
    Ok(ok)
}
