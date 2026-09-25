//! Sample clips go through a reference MP3 encoder, `lame` or `ffmpeg` (docs/sample.md §3),
//! so each test runs once per encoder that is found. Like `convert.rs`, a test whose
//! encoder is absent skips and says so, rather than failing.

use lh_core::Error;
use lh_core::sample::{Clip, Mode, Request, encode};
use lh_core::tools::{Agent, Registry, Tool, ToolId};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn encoders() -> Vec<Tool> {
    [ToolId::Lame, ToolId::Ffmpeg]
        .into_iter()
        .filter_map(|id| match Registry::discover_one(id).require(id) {
            Ok(t) => Some(t.clone()),
            Err(e) => {
                eprintln!("skipping {id}: {e}");
                None
            }
        })
        .collect()
}

fn request(mode: Mode, max_bytes: Option<u64>) -> Request {
    Request {
        clip: Clip {
            start: 0.02,
            length: 0.05,
        },
        mode,
        max_bytes,
        overwrite: false,
    }
}

/// An MP3 opens with an ID3v2 tag or a frame sync.
fn looks_like_mp3(bytes: &[u8]) -> bool {
    bytes.starts_with(b"ID3") || (bytes.len() > 1 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0)
}

#[test]
fn every_source_format_encodes_with_every_encoder() {
    let dir = tempfile::tempdir().unwrap();
    for encoder in encoders() {
        for name in [
            "cdda-aligned.flac",
            "hires-24bit.flac",
            "cdda-aligned.wav",
            "hires-24bit.wav",
            "mono-48k.wav",
        ] {
            for mode in [Mode::Cbr(256), Mode::Vbr(2)] {
                let dst = dir.path().join(format!("{}-{name}-{mode}.mp3", encoder.id));
                let sample = encode(
                    &fixture(name),
                    &dst,
                    &encoder,
                    &request(mode, None),
                    &mut |_, _| true,
                )
                .unwrap_or_else(|e| panic!("{} on {name}, {mode}: {e}", encoder.id));

                let bytes = std::fs::read(&dst).unwrap();
                assert_eq!(sample.bytes, bytes.len() as u64);
                assert!(looks_like_mp3(&bytes), "{} on {name}", encoder.id);
                assert!((sample.clip.length - 0.05).abs() < 0.001, "{name}");
                match &sample.provenance.agent {
                    Agent::Tool { id, .. } => assert_eq!(*id, encoder.id),
                    other => panic!("expected {} in the provenance, got {other:?}", encoder.id),
                }
            }
        }
    }
}

#[test]
fn a_cbr_clip_over_the_limit_is_refused_before_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let Some(encoder) = encoders().into_iter().next() else {
        return;
    };
    let dst = dir.path().join("big.mp3");
    let mut req = request(Mode::Cbr(320), Some(1_000_000));
    req.clip.length = 60.0;
    let mut called = false;
    let err = encode(
        &fixture("cdda-aligned.flac"),
        &dst,
        &encoder,
        &req,
        &mut |_, _| {
            called = true;
            true
        },
    )
    .unwrap_err();
    assert!(matches!(err, Error::SampleTooLarge { .. }), "{err}");
    assert!(!called, "nothing should have been decoded");
    assert!(!dst.exists());
}

#[test]
fn a_vbr_clip_that_comes_out_over_the_limit_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    for encoder in encoders() {
        let dst = dir.path().join(format!("{}.mp3", encoder.id));
        let err = encode(
            &fixture("cdda-aligned.flac"),
            &dst,
            &encoder,
            &request(Mode::Vbr(0), Some(100)),
            &mut |_, _| true,
        )
        .unwrap_err();
        assert!(matches!(err, Error::SampleTooLarge { .. }), "{err}");
        assert!(!dst.exists(), "{} left an over-limit sample", encoder.id);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(leftovers.is_empty(), "a staged file was left behind");
    }
}

#[test]
fn a_clip_past_the_end_of_the_track_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let Some(encoder) = encoders().into_iter().next() else {
        return;
    };
    let mut req = request(Mode::DEFAULT, None);
    req.clip.start = 60.0;
    let err = encode(
        &fixture("cdda-aligned.flac"),
        &dir.path().join("late.mp3"),
        &encoder,
        &req,
        &mut |_, _| true,
    )
    .unwrap_err();
    assert!(err.to_string().contains("only"), "{err}");
}
