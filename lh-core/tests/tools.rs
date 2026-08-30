//! The registry is the traceability story (Principle 2), so most of what matters is what
//! it *says*: a missing tool has to name itself, say what it was for, and list where we
//! looked (Principle 5).

use lh_core::tools::{Agent, Discovery, Provenance, Registry, ToolId, ToolPaths};
use std::path::PathBuf;

#[test]
fn a_configured_path_that_is_not_there_is_an_error_not_a_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let nowhere = dir.path().join("flac-that-does-not-exist");
    let registry = Registry::discover_with(&ToolPaths::from([(ToolId::Flac, nowhere.clone())]));

    let err = registry
        .require(ToolId::Flac)
        .expect_err("a named binary that is absent must fail");
    let message = err.to_string();
    assert!(message.contains("flac"), "{message}");
    assert!(
        message.contains(&nowhere.display().to_string()),
        "{message}"
    );
    // The whole point: it must not have quietly used some other flac instead.
    assert!(
        !message.contains("on PATH"),
        "a configured path should be the whole search: {message}"
    );
}

#[test]
fn a_file_that_is_not_the_tool_is_unusable_rather_than_missing() {
    let dir = tempfile::tempdir().unwrap();
    let impostor = dir.path().join("flac.txt");
    std::fs::write(&impostor, b"this is not a binary").unwrap();

    let registry = Registry::discover_with(&ToolPaths::from([(ToolId::Flac, impostor.clone())]));
    match registry.require(ToolId::Flac) {
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.contains(&impostor.display().to_string()),
                "{message}"
            );
            assert!(message.contains("cannot be used"), "{message}");
        }
        Ok(t) => panic!("a text file reported itself as {}", t.version),
    }
}

#[test]
fn a_missing_tool_names_itself_its_purpose_and_where_we_looked() {
    let registry = Registry::discover_one(ToolId::Shntool);
    match registry.require(ToolId::Shntool) {
        Err(e) => {
            let message = e.to_string();
            assert!(message.contains("shntool"), "{message}");
            assert!(message.contains("SHN support"), "{message}");
            assert!(message.contains("on PATH"), "{message}");
        }
        // shntool is installed on this machine, which is allowed; then the record of it
        // has to be complete instead.
        Ok(tool) => assert_complete(tool),
    }
}

/// Deferred features must not make an install look broken: only `flac` is required by
/// v0.1, so only `flac` can ever appear in `missing_required`.
#[test]
fn only_flac_is_required() {
    for id in ToolId::ALL {
        assert_eq!(id.is_required(), id == ToolId::Flac, "{id}");
    }
    assert!(
        Registry::discover()
            .missing_required()
            .all(|id| id == ToolId::Flac)
    );
}

#[test]
fn every_discovered_tool_carries_a_version_and_a_full_sha256() {
    for (_, discovery) in Registry::discover().entries() {
        if let Discovery::Found(tool) = discovery {
            assert_complete(tool);
        }
    }
}

fn assert_complete(tool: &lh_core::tools::Tool) {
    assert!(!tool.version.is_empty(), "{} reported no version", tool.id);
    assert_eq!(tool.sha256.len(), 64, "{} sha256 is not 32 bytes", tool.id);
    assert!(
        tool.sha256.chars().all(|c| c.is_ascii_hexdigit()),
        "{} sha256 is not hex: {}",
        tool.id,
        tool.sha256
    );
}

/// The in-process path leaves a record too — "nothing external touched this file" is an
/// answer, not the absence of one.
#[test]
fn in_process_work_is_still_recorded() {
    let p = Provenance {
        operation: "FLAC → WAV".into(),
        agent: Agent::in_process(),
        input: PathBuf::from("d1t01.flac"),
        output: PathBuf::from("d1t01.wav"),
    };
    let rendered = p.render();
    assert!(rendered.contains("FLAC → WAV"), "{rendered}");
    assert!(rendered.contains("in-process"), "{rendered}");
    assert!(rendered.contains("lh-core"), "{rendered}");
}
