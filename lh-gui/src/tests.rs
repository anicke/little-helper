use super::*;
use lh_core::analysis;
use lh_core::scan;
use lh_core::tools::ToolId;

/// The real fixture corpus M1 already built (`lh-core/tests/fixtures`), scanned
/// through the exact function `App::scan` calls — proof the file table's data comes
/// from real files, not just that the widget tree type-checks.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../lh-core/tests/fixtures")
}

#[test]
fn scanning_the_fixture_corpus_finds_known_files_and_no_skips() {
    let set = scan::scan(&fixtures_dir(), false).expect("fixtures dir must scan");
    assert!(
        set.files.iter().any(|f| f.file_name() == "cdda-sbe.flac"),
        "expected cdda-sbe.flac among {:?}",
        set.files.iter().map(|f| f.file_name()).collect::<Vec<_>>()
    );
    assert!(
        set.skipped.is_empty(),
        "unexpected skips: {:?}",
        set.skipped
    );
}

#[test]
fn a_known_misaligned_fixture_reports_sbe_through_the_same_path_the_table_uses() {
    let set = scan::scan(&fixtures_dir(), false).expect("fixtures dir must scan");
    let sbe_file = set
        .files
        .iter()
        .find(|f| f.file_name() == "cdda-sbe.flac")
        .expect("cdda-sbe.flac must be in the fixture corpus");
    let label = sbe_label(&analysis::sbe(&sbe_file.stream_info));
    assert!(
        label.starts_with("misaligned"),
        "cdda-sbe.flac should report misaligned SBE, got {label:?}"
    );
}

/// S4 (`docs/gui-shell.md` §7/§9): builds the `iced::widget::table` over the real
/// fixture corpus, one file selected so the per-row checkbox closure's "checked" branch
/// runs too, not just the empty/unselected default every other test's fresh `App` has.
/// No widget tree inspection is possible from here (`Element` exposes nothing to
/// assert on), so this is the same bar S1's own notes name: proof it does not panic
/// with real data through every column's view closure, not proof of on-screen layout.
#[test]
fn file_table_builds_over_the_real_fixture_corpus_without_panicking() {
    let (mut app, _) = App::boot();
    app.scan(&fixtures_dir());
    let first = app
        .working_set
        .as_ref()
        .expect("fixtures dir must scan")
        .files
        .first()
        .expect("fixture corpus must not be empty")
        .path
        .clone();
    app.selected.insert(first);
    let _ = file_table(&app);
}

/// Drains `rx` into `app` until every one of `total` submitted jobs has a terminal
/// event (`Finished` or `Cancelled`) — the same loop `lh-cli`'s `run_batch` runs, minus
/// the printing, and run directly against the real `Queue<JobOutcome>` rather than
/// through Iced's `Subscription` machinery, which nothing outside a running window can
/// drive.
fn drain(
    app: &mut App,
    rx: &crossbeam_channel::Receiver<lh_core::job::Event<JobOutcome>>,
    total: usize,
) {
    let mut done = 0;
    while done < total {
        let event = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("queue closed with jobs still outstanding");
        let terminal = matches!(
            event,
            lh_core::job::Event::Finished { .. } | lh_core::job::Event::Cancelled { .. }
        );
        app.handle_job_event(event.into());
        if terminal {
            done += 1;
        }
    }
}

/// Real evidence the queue wiring — not just the widget tree — moves a real file
/// through `analysis::verify` and back into `App` state: `cdda-aligned.flac` decodes
/// clean and matches its own MD5, `wrong-md5.flac` decodes clean but does not, per the
/// fixture corpus `lh-core/tests/fixtures` already establishes for `lh verify` (see
/// `lh-cli`'s own `cmd_verify`, which these fixtures were built against).
#[test]
fn running_verify_through_the_real_queue_marks_ok_and_mismatch_files_correctly() {
    let (mut app, _) = App::boot();
    app.scan(&fixtures_dir());
    app.run_operation(Operation::Verify);

    let files: Vec<(String, PathBuf)> = app
        .working_set
        .as_ref()
        .expect("scan must populate a working set")
        .files
        .iter()
        .map(|f| (f.file_name(), f.path.clone()))
        .collect();
    let rx = app.queue.events();
    drain(&mut app, &rx, files.len());

    let status_for = |name: &str| {
        let path = &files.iter().find(|(n, _)| n == name).unwrap().1;
        let id = app.latest_job_by_path[path];
        status_label(&app.jobs[&id].status)
    };
    assert_eq!(status_for("cdda-aligned.flac"), "OK");
    let mismatch = status_for("wrong-md5.flac");
    assert!(
        mismatch.starts_with("FAILED: MISMATCH"),
        "wrong-md5.flac should report a verify mismatch, got {mismatch:?}"
    );
}

/// The real gap this milestone found: `Queue::submit` checks one `CancelToken` shared
/// for the queue's whole life, so a Cancel press with nothing in flight would otherwise
/// leave every *future* Run silently cancelling its jobs before they ever ran, since
/// `lh-gui` (unlike every `lh-cli` batch, which builds a fresh `Queue` per invocation)
/// keeps one `Queue` for the app's whole life (`docs/gui.md` §1). `run_operation`
/// resets the token before submitting; this proves that reset actually lets jobs run
/// rather than merely compiling.
#[test]
fn cancelling_with_nothing_in_flight_does_not_disable_the_next_run() {
    let (mut app, _) = App::boot();
    app.scan(&fixtures_dir());
    app.queue.cancel();
    app.run_operation(Operation::Sbe);

    let total = app.working_set.as_ref().unwrap().files.len();
    let rx = app.queue.events();
    drain(&mut app, &rx, total);

    for entry in app.jobs.values() {
        assert!(
            !matches!(entry.status, JobStatus::Cancelled),
            "{} was cancelled even though run_operation should have reset the token",
            entry.label
        );
    }
}

/// G3's real evidence: `Operation::Convert(ConvertTarget::Wav)` moves a real FLAC
/// fixture through the queue, `convert::to_wav`, and back into `App` state —
/// writing an actual `.wav` beside the source, reporting it "checked against source"
/// (the fixture carries a STREAMINFO MD5), and appending its `Provenance::render()`
/// text to `App::log`. Run against a copy in a tempdir rather than the fixtures dir
/// itself, since a real write must not touch the read-only checked-in corpus.
#[test]
fn running_convert_to_wav_through_the_real_queue_writes_a_checked_file_and_logs_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("cdda-aligned.flac");
    std::fs::copy(fixtures_dir().join("cdda-aligned.flac"), &src).unwrap();

    let (mut app, _) = App::boot();
    app.scan(dir.path());
    app.run_operation(Operation::Convert(ConvertTarget::Wav));

    let total = app.working_set.as_ref().unwrap().files.len();
    let rx = app.queue.events();
    drain(&mut app, &rx, total);

    let id = app.latest_job_by_path[&src];
    assert_eq!(
        status_label(&app.jobs[&id].status),
        "WROTE cdda-aligned.wav"
    );
    assert!(
        dir.path().join("cdda-aligned.wav").exists(),
        "convert should have written cdda-aligned.wav beside the source"
    );
    assert_eq!(
        app.log.len(),
        1,
        "one finished convert job should log one provenance entry, got {:?}",
        app.log
    );
    assert!(
        app.log[0].contains("FLAC → WAV"),
        "log entry should name the conversion, got {:?}",
        app.log[0]
    );
}

/// The other direction, through the reference `flac` binary discovered from
/// `App::tools` — real evidence `run_operation`'s `flac_tool` plumbing actually reaches
/// `convert::to_flac`, not just that it compiles. Skips (rather than failing) when
/// `flac` is not installed, the same convention `lh-core/tests/convert.rs` uses.
#[test]
fn running_convert_to_flac_through_the_real_queue_writes_a_checked_file() {
    if Registry::discover_one(ToolId::Flac)
        .require(ToolId::Flac)
        .is_err()
    {
        eprintln!("skipping: flac not found");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("cdda-aligned.wav");
    std::fs::copy(fixtures_dir().join("cdda-aligned.wav"), &src).unwrap();

    let (mut app, _) = App::boot();
    app.scan(dir.path());
    app.run_operation(Operation::Convert(ConvertTarget::Flac));

    let total = app.working_set.as_ref().unwrap().files.len();
    let rx = app.queue.events();
    drain(&mut app, &rx, total);

    let id = app.latest_job_by_path[&src];
    let status = status_label(&app.jobs[&id].status);
    assert_eq!(status, "WROTE cdda-aligned.flac", "got {status:?}");
    assert!(dir.path().join("cdda-aligned.flac").exists());
    assert!(app.log.iter().any(|e| e.contains("WAV → FLAC")));
}

/// The gap `run_operation`'s pre-filter closes: `lh-cli`'s `cmd_convert` treats a file
/// already in the target format as a silent skip (`ConvertOutcome::Skipped`), not a
/// failure. Converting a working set that is *already* WAV to WAV must not submit a
/// job, and must not leave a FAILED row for a file nothing was wrong with.
#[test]
fn converting_to_the_format_a_file_is_already_in_submits_no_job() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("cdda-aligned.wav");
    std::fs::copy(fixtures_dir().join("cdda-aligned.wav"), &src).unwrap();

    let (mut app, _) = App::boot();
    app.scan(dir.path());
    app.run_operation(Operation::Convert(ConvertTarget::Wav));

    assert!(
        app.jobs.is_empty(),
        "no job should have been submitted for a file already in the target format, got {:?}",
        app.jobs.keys().collect::<Vec<_>>()
    );
}

/// S2's core behaviour change (`docs/gui-shell.md` §9): `run_operation` reads
/// `App::selected`, not every file in the working set. Untick one fixture, run Sbe
/// (cheap, needs no external tool) over the rest, and confirm the unticked file never
/// got a job at all — not a skipped one, the same "no job" treatment as a file already
/// in a convert's target format.
#[test]
fn run_operation_submits_no_job_for_an_unticked_file() {
    let (mut app, _) = App::boot();
    app.scan(&fixtures_dir());

    let untick = app
        .working_set
        .as_ref()
        .unwrap()
        .files
        .iter()
        .find(|f| f.file_name() == "cdda-aligned.flac")
        .expect("cdda-aligned.flac must be in the fixture corpus")
        .path
        .clone();
    app.selected.remove(&untick);

    let total_selected = app.selected.len();
    app.run_operation(Operation::Sbe);

    let rx = app.queue.events();
    drain(&mut app, &rx, total_selected);

    assert!(
        !app.latest_job_by_path.contains_key(&untick),
        "cdda-aligned.flac was unticked and should not have gotten a job"
    );
    assert!(
        app.latest_job_by_path.len() == total_selected,
        "expected exactly one job per ticked file, got {}",
        app.latest_job_by_path.len()
    );
}

#[test]
fn tool_discovery_renders_every_id_one_line_each() {
    let tools = Registry::discover();
    let lines: Vec<String> = tools.entries().map(|(id, d)| tool_line(id, d)).collect();
    assert_eq!(lines.len(), ToolId::ALL.len());
    for (line, id) in lines.iter().zip(ToolId::ALL) {
        assert!(
            line.starts_with(id.name()),
            "{line:?} should start with {}",
            id.name()
        );
    }
}

/// A folder with a couple of small synthetic files, not the read-only audio fixture
/// corpus — torrent create/check do not care what the bytes are, and a real write
/// (the `.torrent` itself) must not touch the checked-in corpus, same reasoning G3's
/// convert tests already established.
fn torrent_source_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("track1.bin"), vec![1u8; 40_000]).unwrap();
    std::fs::write(dir.path().join("track2.bin"), vec![2u8; 20_000]).unwrap();
    dir
}

/// G4's real evidence for C5: `App::run_torrent_create` moves a real folder through
/// `torrent::create` and back into `App` state, producing an actual `.torrent`
/// beside the source with the piece count/length the payload implies.
#[test]
fn running_torrent_create_through_the_real_queue_writes_a_torrent() {
    let dir = torrent_source_dir();

    let (mut app, _) = App::boot();
    app.scan(dir.path());
    assert_eq!(
        app.working_root.as_deref(),
        Some(dir.path()),
        "scanning must record the root torrent create builds from"
    );
    app.run_torrent_create();

    let rx = app.queue.events();
    drain(&mut app, &rx, 1);

    assert_eq!(app.jobs.len(), 1);
    let status = status_label(&app.jobs.values().next().unwrap().status);
    assert!(
        status.starts_with("WROTE") && status.contains("2 files"),
        "got {status:?}"
    );
    let source = dir.path().canonicalize().unwrap();
    let torrent_path = lh_core::torrent::default_output(&source).unwrap();
    assert!(
        torrent_path.exists(),
        "expected a .torrent written at {}",
        torrent_path.display()
    );
}

/// S2's other half (`docs/gui-shell.md` §4, §9): Torrent → Create describes the folder
/// as it exists on disk and must ignore the selection entirely — filtering by ticked
/// rows would produce a `.torrent` whose file list did not match its directory, which is
/// a broken torrent, not a subset. Deselect everything and confirm the torrent still
/// names all real files.
#[test]
fn run_torrent_create_ignores_the_selection_entirely() {
    let dir = torrent_source_dir();

    let (mut app, _) = App::boot();
    app.scan(dir.path());
    app.selected.clear();
    app.run_torrent_create();

    let rx = app.queue.events();
    drain(&mut app, &rx, 1);

    let status = status_label(&app.jobs.values().next().unwrap().status);
    assert!(
        status.starts_with("WROTE") && status.contains("2 files"),
        "torrent create should have used every file on disk regardless of ticking, got {status:?}"
    );
}

/// G4's real evidence for T4: create a real torrent, then check it against its own
/// source folder through `App::run_torrent_check` and the real queue — `Verdict::Complete`
/// end to end, plus the per-file table `torrent_check_rows` feeds the results panel.
#[test]
fn running_torrent_check_through_the_real_queue_reports_complete_and_fills_the_table() {
    let dir = torrent_source_dir();
    let source = dir.path().canonicalize().unwrap();
    let torrent_path = lh_core::torrent::default_output(&source).unwrap();

    let (mut app, _) = App::boot();
    app.scan(dir.path());
    app.run_torrent_create();
    let rx = app.queue.events();
    drain(&mut app, &rx, 1);
    assert!(
        torrent_path.exists(),
        "setup: torrent create should have written {}",
        torrent_path.display()
    );

    app.torrent_check_path = Some(torrent_path);
    app.torrent_check_against = dir.path().display().to_string();
    app.run_torrent_check();
    drain(&mut app, &rx, 1);

    let check_status = status_label(&app.jobs.values().nth(1).unwrap().status);
    assert_eq!(check_status, "OK", "got {check_status:?}");
    assert_eq!(app.torrent_check_rows.len(), 2, "one row per real file");
    for row in &app.torrent_check_rows {
        assert_eq!(row.label, "OK", "{}: {}", row.path, row.detail);
    }
}

/// The synchronous check `run_torrent_create` does before submitting anything: an
/// unresolvable tracker spec must fail up front, exactly like `lh-cli`'s own
/// `cmd_torrent_create`, not become a job the queue has to fail instead.
#[test]
fn an_unresolvable_tracker_spec_is_rejected_before_any_job_is_submitted() {
    let dir = torrent_source_dir();
    let (mut app, _) = App::boot();
    app.scan(dir.path());
    app.torrent_tracker_input = "not-a-real-tracker-id".to_string();
    app.run_torrent_create();

    assert!(app.error.is_some(), "expected an error, got none");
    assert!(
        app.jobs.is_empty(),
        "no job should have been submitted for an unresolvable tracker"
    );
}

/// `Message::PathDropped` routes by extension (`update`'s own match arm) — a dropped
/// `.torrent` must reach the check panel, not be handed to `App::scan` as though it
/// were an audio folder.
#[test]
fn dropping_a_dot_torrent_file_is_routed_to_the_check_panel() {
    let torrent_path = PathBuf::from("/tmp/does-not-need-to-exist/show.torrent");
    let (mut app, _) = App::boot();
    // `pick_torrent` will fail to read it and set `app.error`; the routing itself is
    // what this test checks, not a successful parse.
    let _ = update(&mut app, Message::PathDropped(torrent_path.clone()));

    assert_eq!(app.torrent_check_path, Some(torrent_path));
    assert!(
        app.working_set.is_none(),
        "must not have been treated as a folder to scan"
    );
}

/// S3's real evidence for Checksum → Create (`docs/gui-shell.md` §6): select just the
/// four fixtures `reference.ffp` has entries for, run Checksum → Create at FFP through
/// the real queue, and confirm the written file's entries match the reference tool's
/// own output exactly — the same oracle `lh-core/tests/corpus.rs`'s
/// `ffp_matches_the_reference_tools` uses, one layer up through `App` state instead of
/// calling `checksum::ffp` directly.
#[test]
fn running_checksum_create_through_the_real_queue_writes_a_checksum_file_matching_the_reference_ffp()
 {
    let reference = ChecksumFile::read(ChecksumKind::Ffp, &fixtures_dir().join("reference.ffp"))
        .expect("reference.ffp should parse");
    let reference_names: HashSet<String> = reference
        .entries
        .iter()
        .map(|e| e.file_name.clone())
        .collect();

    let (mut app, _) = App::boot();
    app.scan(&fixtures_dir());
    app.selected = app
        .working_set
        .as_ref()
        .unwrap()
        .files
        .iter()
        .filter(|f| reference_names.contains(&f.file_name()))
        .map(|f| f.path.clone())
        .collect();
    assert_eq!(
        app.selected.len(),
        reference.entries.len(),
        "every reference.ffp name must be a real fixture"
    );

    let out_dir = tempfile::tempdir().unwrap();
    let output = out_dir.path().join("out.ffp");
    app.checksum_kind = ChecksumKind::Ffp;
    app.checksum_output = output.display().to_string();
    app.run_checksum_create();

    let total = app.selected.len();
    let rx = app.queue.events();
    drain(&mut app, &rx, total);

    let written = ChecksumFile::read(ChecksumKind::Ffp, &output).expect("output must parse");
    let written_map: HashMap<String, [u8; 16]> = written
        .entries
        .iter()
        .map(|e| (e.file_name.clone(), e.digest))
        .collect();
    let reference_map: HashMap<String, [u8; 16]> = reference
        .entries
        .iter()
        .map(|e| (e.file_name.clone(), e.digest))
        .collect();
    assert_eq!(written_map, reference_map);
}

/// S3's real evidence for Checksum → Check: the checked-in `reference.ffp`
/// (`lh-core/tests/fixtures`) names real fixtures, so checking it against them end to
/// end through the real queue must report OK for every entry.
#[test]
fn running_checksum_check_through_the_real_queue_against_the_reference_ffp_reports_ok_for_every_entry()
 {
    let (mut app, _) = App::boot();
    app.pick_checksum_file(fixtures_dir().join("reference.ffp"));
    assert_eq!(app.checksum_check_kind, Some(ChecksumKind::Ffp));
    let total = app.checksum_check_file.as_ref().unwrap().entries.len();
    assert!(total > 0, "reference.ffp is empty");

    app.run_checksum_check();
    let rx = app.queue.events();
    drain(&mut app, &rx, total);

    assert_eq!(app.checksum_check_rows.len(), total);
    for row in &app.checksum_check_rows {
        assert_eq!(row.label, "OK", "{}: {}", row.path, row.detail);
    }
}

/// Same, against `reference.st5` — a different kind (decoded-audio MD5, not the
/// STREAMINFO one) and a different oracle (real shntool, `lh-core/tests/corpus.rs`'s
/// own note on why this file exists), through the same Checksum → Check path.
#[test]
fn running_checksum_check_through_the_real_queue_against_the_reference_st5_reports_ok_for_every_entry()
 {
    let (mut app, _) = App::boot();
    app.pick_checksum_file(fixtures_dir().join("reference.st5"));
    assert_eq!(app.checksum_check_kind, Some(ChecksumKind::St5));
    let total = app.checksum_check_file.as_ref().unwrap().entries.len();
    assert!(total > 0, "reference.st5 is empty");

    app.run_checksum_check();
    let rx = app.queue.events();
    drain(&mut app, &rx, total);

    assert_eq!(app.checksum_check_rows.len(), total);
    for row in &app.checksum_check_rows {
        assert_eq!(row.label, "OK", "{}: {}", row.path, row.detail);
    }
}

/// The two failure paths `cmd_check` prints as `MISSING`/`MISMATCH`
/// (`lh-cli/src/main.rs`) — a synthetic checksum file naming one file that does not
/// exist beside it and one whose digest does not match the real file that does.
#[test]
fn checksum_check_reports_missing_and_mismatch_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.bin"), b"hello").unwrap();
    let content = format!(
        "{}  a.bin\n{}  missing.bin\n",
        "0".repeat(32),
        "f".repeat(32)
    );
    let checksum_path = dir.path().join("check.md5");
    std::fs::write(&checksum_path, &content).unwrap();

    let (mut app, _) = App::boot();
    app.pick_checksum_file(checksum_path);
    assert_eq!(app.checksum_check_kind, Some(ChecksumKind::Md5));
    app.run_checksum_check();

    let rx = app.queue.events();
    drain(&mut app, &rx, 2);

    let mut by_path: HashMap<String, &'static str> = app
        .checksum_check_rows
        .iter()
        .map(|r| (r.path.clone(), r.label))
        .collect();
    assert_eq!(by_path.remove("a.bin"), Some("MISMATCH"));
    assert_eq!(by_path.remove("missing.bin"), Some("MISSING"));
}

/// The extension-inference gap `docs/gui-shell.md` §10 Q4 leaves open: an extension
/// naming none of `.ffp`/`.md5`/`.st5` is an error, matching `cmd_check`'s own `bail!`,
/// not a silent guess.
#[test]
fn picking_a_checksum_file_with_an_unrecognized_extension_sets_an_error() {
    let (mut app, _) = App::boot();
    app.pick_checksum_file(PathBuf::from("/tmp/does-not-need-to-exist/show.txt"));

    assert!(app.error.is_some(), "expected an error, got none");
    assert_eq!(app.checksum_check_kind, None);
    assert!(app.checksum_check_file.is_none());
}

/// `Message::PathDropped` routes a `.ffp`/`.md5`/`.st5` to the checksum check panel,
/// the same way it already routes a `.torrent` to the torrent check panel.
#[test]
fn dropping_a_checksum_file_is_routed_to_the_checksum_check_panel() {
    let path = PathBuf::from("/tmp/does-not-need-to-exist/reference.ffp");
    let (mut app, _) = App::boot();
    let _ = update(&mut app, Message::PathDropped(path.clone()));

    assert_eq!(app.checksum_check_path, Some(path));
    assert!(
        app.working_set.is_none(),
        "must not have been treated as a folder to scan"
    );
}
