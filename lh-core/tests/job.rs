use lh_core::job::{Event, Queue};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Drain exactly `n` terminal (Finished or Cancelled) events, keyed by job index, with a
/// generous timeout so a hang shows up as a failing test rather than a stuck CI job.
fn drain_terminal<T: Send>(queue: &Queue<T>, n: usize) -> BTreeMap<usize, Option<T>> {
    let mut out = BTreeMap::new();
    while out.len() < n {
        match queue.events().recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Finished { id, output, .. }) => {
                out.insert(id.index(), Some(output));
            }
            Ok(Event::Cancelled { id, .. }) => {
                out.insert(id.index(), None);
            }
            Ok(Event::Started { .. } | Event::Progress { .. }) => {}
            Err(e) => panic!("timed out waiting for {n} jobs, got {}: {e}", out.len()),
        }
    }
    out
}

#[test]
fn runs_every_submitted_job_and_reports_its_own_output() {
    let queue: Queue<i32> = Queue::with_workers(4);
    for i in 0..8 {
        queue.submit(format!("job {i}"), move |_| i * i);
    }
    let results = drain_terminal(&queue, 8);
    for (i, out) in &results {
        assert_eq!(*out, Some((*i as i32) * (*i as i32)));
    }
}

#[test]
fn wait_blocks_until_every_job_has_a_terminal_event() {
    let queue: Queue<()> = Queue::with_workers(2);
    let ran = Arc::new(AtomicUsize::new(0));
    for _ in 0..20 {
        let ran = ran.clone();
        queue.submit("count", move |_| {
            ran.fetch_add(1, Ordering::SeqCst);
        });
    }
    queue.wait();
    assert_eq!(ran.load(Ordering::SeqCst), 20);
}

#[test]
fn cancelling_before_a_job_starts_skips_it_without_running_it() {
    let queue: Queue<()> = Queue::with_workers(1);
    let ran = Arc::new(AtomicUsize::new(0));
    // Set once the first job's closure has actually started running, so the test can
    // wait for it rather than racing cancel() against the worker picking the job up.
    let started = Arc::new(AtomicBool::new(false));

    // One worker, so this first job occupies it while the rest queue up behind it.
    let gate_ran = ran.clone();
    let gate_started = started.clone();
    queue.submit("first", move |_| {
        gate_started.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
        gate_ran.fetch_add(1, Ordering::SeqCst);
    });
    for _ in 0..5 {
        let ran = ran.clone();
        queue.submit("later", move |_| {
            ran.fetch_add(1, Ordering::SeqCst);
        });
    }

    while !started.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    queue.cancel();
    let results = drain_terminal(&queue, 6);

    // The job already running when cancel() was called finishes normally.
    assert_eq!(results[&0], Some(()));
    // Nothing queued behind it was ever run.
    for id in 1..=5 {
        assert_eq!(results[&id], None, "job {id} should have been cancelled");
    }
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[test]
fn a_job_can_see_its_own_cancellation_mid_run() {
    let queue: Queue<bool> = Queue::with_workers(1);
    queue.submit("checks itself", |progress| {
        // Nothing has called cancel() yet.
        if progress.is_cancelled() {
            return false;
        }
        true
    });
    let results = drain_terminal(&queue, 1);
    assert_eq!(results[&0], Some(true));
}

#[test]
fn sub_item_progress_reaches_the_event_stream() {
    let queue: Queue<()> = Queue::with_workers(1);
    queue.submit("pieces", |progress| {
        for done in 1..=4 {
            progress.report(done, 4);
        }
    });

    let mut seen = Vec::new();
    loop {
        match queue.events().recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Progress { done, total, .. } => seen.push((done, total)),
            Event::Finished { .. } => break,
            Event::Started { .. } | Event::Cancelled { .. } => {}
        }
    }
    assert_eq!(seen, vec![(1, 4), (2, 4), (3, 4), (4, 4)]);
}
