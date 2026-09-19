use crate::*;
use anyhow::{Context, Result};
use lh_core::display;
use lh_core::job::{Event, Queue};
use lh_core::torrent::{
    Chosen, CreateOpts, Created, FileStatus, Metainfo, Origin, Passkeys, Tracker, TrackerList,
    Verdict, check, check_sizes, create, default_output, resolve,
};
use std::path::Path;

/// `m:ss.mmm`, the layout shntool uses — sub-second precision matters when the question
/// is whether a track sits on a sector boundary.
pub(crate) fn cmd_torrent_info(file: &Path, list_files: bool) -> Result<bool> {
    let t = Metainfo::read(file).with_context(|| format!("reading {}", file.display()))?;

    println!("{}", t.name);
    println!("  infohash     {}", t.info_hash_hex());
    println!(
        "  pieces       {} x {}",
        t.pieces.len(),
        display::bytes(t.piece_length)
    );
    println!(
        "  total        {} ({} bytes)",
        display::bytes(t.total_length),
        t.total_length
    );
    let real = t.real_files().count();
    let pad = t.files.len() - real;
    if pad > 0 {
        println!("  files        {real} ({pad} padding)");
    } else {
        println!("  files        {real}");
    }
    // A private torrent cannot be reseeded anywhere else, and the flag is inside the
    // infohash, so a trader is entitled to see it before they plan around this file.
    if t.private {
        println!("  private      yes (BEP 27; part of the infohash)");
    }
    if let Some(v) = &t.source {
        println!("  source       {v}");
    }
    if let Some(v) = &t.created_by {
        println!("  created by   {v}");
    }
    if let Some(ts) = t.creation_date {
        println!("  created      {}", display::date(ts));
    }
    if let Some(v) = &t.comment {
        // Trackers write multi-line comments; keep the column alignment intact.
        for (i, line) in v.lines().enumerate() {
            println!("  {:<12} {line}", if i == 0 { "comment" } else { "" });
        }
    }
    for (i, tracker) in t.trackers().enumerate() {
        println!("  {:<12} {tracker}", if i == 0 { "trackers" } else { "" });
    }

    if list_files {
        println!();
        for f in t.real_files() {
            println!("  {:>12}  {}", display::bytes(f.length), f.display_path());
        }
    }
    Ok(true)
}

pub(crate) fn cmd_torrent_create(args: &TorrentCreateArgs) -> Result<bool> {
    let source = args
        .path
        .canonicalize()
        .with_context(|| format!("reading {}", args.path.display()))?;

    // Ids become URLs here, and each --tracker becomes its own tier: clients pick at random
    // within a tier and fall through between them, so putting unrelated sites in one tier
    // is a coin flip over who hears about the seed.
    let list = TrackerList::load().context("reading the tracker list")?;
    let keys = Passkeys::load().context("reading the passkey list")?;
    let chosen = resolve(&args.trackers, &list, &keys)?;
    for warning in &chosen.warnings {
        eprintln!("warning: {warning}");
    }

    // The flags win over the table: a tracker entry can only ever add `private` or a
    // `source`, never take one away that the user asked for.
    let private = args.private || chosen.private;
    let source_tag = args.source.clone().or_else(|| chosen.source.clone());

    let opts = CreateOpts {
        announce: chosen.tiers.clone(),
        piece_length: args.piece_length,
        private,
        source: source_tag.clone(),
        comment: args.comment.clone(),
        include_all: args.include_all,
        overwrite: args.force,
        ..CreateOpts::default()
    };

    let dst = match &args.output {
        Some(o) => o.clone(),
        None => default_output(&source).ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no parent directory to write a torrent beside",
                source.display()
            )
        })?,
    };
    // Writing the .torrent inside the folder it describes adds a file to that folder, so
    // re-creating it later would produce a different infohash.
    if source.is_dir() && dst.parent().is_some_and(|p| p.starts_with(&source)) {
        eprintln!(
            "warning: writing the torrent inside {} means re-creating it later will not \
             produce the same infohash",
            source.display()
        );
    }

    // One job on a queue of one, so a large show's piece-hashing walk shows progress and
    // Ctrl-C can stop it between pieces (docs/job-queue.md §3) instead of only between
    // files, which is all a batch of independent files has to offer.
    let queue: Queue<lh_core::Result<Created>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();
    let _ = ctrlc::set_handler(move || cancel.cancel());

    let job_source = source.clone();
    let job_dst = dst.clone();
    let job_opts = opts.clone();
    queue.submit("torrent create", move |progress| {
        create(&job_source, &job_dst, &job_opts, &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        })
    });

    let outcome = loop {
        match queue.events().recv().expect("queue closed unexpectedly") {
            Event::Progress { done, total, .. } => {
                eprint!("\r  hashing piece {done} of {total}");
            }
            Event::Finished { output, .. } => break Some(output),
            Event::Cancelled { .. } => break None,
            Event::Started { .. } => {}
        }
    };
    eprintln!();
    let made = match outcome {
        Some(result) => {
            result.with_context(|| format!("creating a torrent for {}", source.display()))?
        }
        None => {
            println!("cancelled before writing a torrent");
            return Ok(false);
        }
    };

    println!("{}", made.name);
    println!(
        "  {} files   {}   {} {} of {}",
        made.files.len(),
        display::bytes(made.total_length),
        made.pieces,
        if made.pieces == 1 { "piece" } else { "pieces" },
        display::bytes(made.piece_length),
    );
    for (path, why) in &made.excluded {
        let shown = path.strip_prefix(&source).unwrap_or(path);
        println!("  excluded   {} ({})", shown.display(), why.reason());
    }
    println!("  infohash   {}", made.info_hash_hex());
    // The flag is inside the info dictionary, so a table we ship silently decided part of
    // this torrent's identity. Say which entry did it.
    if private {
        let by = if args.private {
            "--private".to_string()
        } else {
            let names: Vec<&str> = chosen
                .chosen
                .iter()
                .filter(|c| c.tracker.as_ref().is_some_and(|t| t.private))
                .map(Chosen::name)
                .collect();
            names.join(", ")
        };
        println!("  private    yes (BEP 27; part of the infohash — set by {by})");
    }
    if let Some(tag) = &source_tag {
        println!("  source     {tag} (part of the infohash)");
    }
    for (i, c) in chosen.chosen.iter().enumerate() {
        let label = if i == 0 { "tracker" } else { "" };
        match &c.tracker {
            Some(t) => println!(
                "  {label:<10} {}  {}  ({})",
                t.name,
                c.announce,
                confirmation(t)
            ),
            None => println!(
                "  {label:<10} {}  (given as a URL, used verbatim)",
                c.announce
            ),
        }
    }
    if chosen.chosen.is_empty() {
        println!("  tracker    none (a trackerless torrent)");
    }
    println!("  wrote      {}", made.path.display());
    Ok(true)
}

/// What we know about an entry, in one parenthesis. Never just a status word: a status
/// with no date is the thing that let TLH recommend a dead tracker for years.
fn confirmation(t: &Tracker) -> String {
    match &t.checked {
        Some(date) => format!("{} — checked {date}", t.health.label()),
        None => format!("{} — from your own list", t.health.label()),
    }
}

/// The list, with what we saw and when. Everything a user needs to decide whether to
/// believe us or go and look — in two lines per entry, not four: only the trackers that
/// answered when checked are in here at all, so there is no dead weight left to explain.
pub(crate) fn cmd_torrent_trackers() -> Result<bool> {
    let list = TrackerList::load().context("reading the tracker list")?;
    let keys = Passkeys::load().context("reading the passkey list")?;

    for t in list.all().filter(|t| t.health.responds()) {
        let origin = match t.origin {
            Origin::Bundled => String::new(),
            Origin::User => "  (yours)".to_string(),
            Origin::Overridden => "  (replaced by your own list)".to_string(),
        };
        println!("{:<14}{}{origin}  —  {}", t.id, t.name, confirmation(t));

        let mut second = t.announce.clone();
        if let Some(saw) = &t.evidence {
            second.push_str(" — ");
            second.push_str(saw);
        }
        println!("{:<14}{second}", "");

        let mut flags = Vec::new();
        if t.private {
            flags.push("sets private: 1 (changes the infohash)".to_string());
        }
        if let Some(tag) = &t.source {
            flags.push(format!("sets info.source {tag:?} (changes the infohash)"));
        }
        if t.needs_passkey() {
            flags.push(match keys.get(&t.id) {
                Some(_) => "passkey configured".to_string(),
                None => "needs a passkey, and none is configured".to_string(),
            });
        }
        if !flags.is_empty() {
            println!("{:<14}{}", "", flags.join("; "));
        }
    }

    let usable = list
        .iter()
        .filter(|t| t.health.responds() && t.health.usable())
        .count();
    let total = list.iter().filter(|t| t.health.responds()).count();
    println!("{usable} of {total} entries can be used as they stand.");
    match &list.user_list {
        Some(path) if path.exists() => println!("your own list: {}", path.display()),
        Some(path) => {
            println!("no list of your own yet; put one at {}", path.display());
            println!(
                "  one `Display Name|announce URL` per line — the format Trader's Little \
                 Helper used, so an existing tracker.lst can be copied straight in"
            );
        }
        None => {}
    }
    Ok(true)
}

pub(crate) fn cmd_torrent_check(file: &Path, path: &Path, quick: bool) -> Result<bool> {
    let meta = Metainfo::read(file).with_context(|| format!("reading {}", file.display()))?;
    let report = if quick {
        check_sizes(&meta, file, path)
    } else {
        check(&meta, file, path, &mut |_, _| true)
    }
    .with_context(|| format!("checking against {}", path.display()))?;

    println!("{}", report.name);
    println!(
        "  {}  {} files  {}",
        hex::encode(report.info_hash),
        meta.real_files().count(),
        display::bytes(meta.total_length)
    );
    println!("  root {}", report.root.display());
    println!();

    for outcome in &report.files {
        if outcome.status == FileStatus::Padding {
            continue;
        }
        let name = meta.files[outcome.index].display_path();
        let label = outcome.status.label();
        match &outcome.status {
            FileStatus::WrongSize { expected, actual } => {
                println!("{label:<11} {name}  (expected {expected} bytes, found {actual})")
            }
            FileStatus::Unreadable { reason } => println!("{label:<11} {name}  ({reason})"),
            FileStatus::Corrupt { bad_pieces } => {
                println!(
                    "{label:<11} {name}  ({})",
                    display::pieces_phrase(bad_pieces)
                )
            }
            FileStatus::Suspect { piece, shared_with } => {
                let others: Vec<String> = shared_with
                    .iter()
                    .map(|i| meta.files[*i].display_path())
                    .collect();
                println!(
                    "{label:<11} {name}  (piece {piece} is shared with {}; either could be at fault)",
                    others.join(", ")
                );
            }
            FileStatus::Partial {
                verified,
                unverifiable,
            } => println!(
                "{label:<11} {name}  ({verified} pieces verified, {unverifiable} unreadable \
                 because a neighbouring file is bad)"
            ),
            _ => println!("{label:<11} {name}"),
        }
    }
    for extra in &report.extra_local {
        let shown = extra.strip_prefix(&report.root).unwrap_or(extra);
        println!("{:<11} {}", "EXTRA", shown.display());
    }

    println!();
    let total = meta.real_files().count();
    if let Some(p) = report.pieces {
        print!("{} of {} pieces verified", p.ok, p.total);
        if p.failed > 0 {
            print!(", {} failed", p.failed);
        }
        if p.unverifiable > 0 {
            print!(", {} unverifiable", p.unverifiable);
        }
        println!();
    }
    match report.verdict() {
        Verdict::Incomplete => {
            let n = report.needs_attention().count();
            let plural = if n == 1 { "file needs" } else { "files need" };
            println!("{n} of {total} {plural} attention");
        }
        Verdict::SizesMatch => println!("all {total} files match by size (contents not read)"),
        Verdict::Complete => println!("all {total} files verified"),
    }
    Ok(report.verdict() != Verdict::Incomplete)
}
