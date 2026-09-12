//! Planning and executing a batch rename to the etree track-name standard
//! (docs/tagging.md §4, N3).
//!
//! Mirrors `analysis::sbe_fix`'s split, the repo's established shape for a whole-set
//! operation: [`plan_rename`] is pure arithmetic over names already in memory — no I/O,
//! so it can be recomputed on every keystroke of a live preview the way the SBE fix plan
//! already is (`lh-tui/src/main.rs:1135`) — and [`execute_rename`] is the only part that
//! touches disk.

use crate::error::{Error, Result};
use crate::etree::{ShowDate, TrackName, YearForm};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What a rename should turn a set into. One value for the whole run — docs/tagging.md
/// §8 Q3 records the multi-disc limitation that follows from that: a flat folder holding
/// two discs gets one `disc` value applied to every file in it.
#[derive(Debug, Clone)]
pub struct NameSpec {
    pub band: String,
    pub date: ShowDate,
    /// `77-05-08` instead of `1977-05-08` — the wiki's other year form (docs/tagging.md
    /// §0).
    pub short_year: bool,
    pub disc: Option<u32>,
    /// Carry a file's existing title suffix (`bertha` in `gd...t01bertha.shn`) forward.
    /// A file whose current name is not an etree name has no suffix to carry either way.
    pub keep_suffix: bool,
}

impl NameSpec {
    fn year_form(&self) -> YearForm {
        if self.short_year {
            YearForm::Short
        } else {
            YearForm::Long
        }
    }
}

/// What would happen to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameStatus {
    /// The computed name is the name it already has.
    Unchanged,
    Changed,
    /// Another file in this same plan computed to the identical name.
    /// [`execute_rename`] refuses the whole plan rather than picking between them.
    Collision,
}

/// One file's row in a plan: where it is, where it would go, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameEntry {
    pub from: PathBuf,
    pub to: PathBuf,
    pub status: RenameStatus,
}

/// A whole set's plan, in the order it was given.
#[derive(Debug, Clone, Default)]
pub struct RenamePlan {
    pub entries: Vec<RenameEntry>,
}

impl RenamePlan {
    pub fn has_collisions(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.status == RenameStatus::Collision)
    }
}

/// Compute a [`RenamePlan`] for `files`, taken in the order given — `scan`'s filename
/// order (`lh-core/src/scan.rs:21`), the order every other whole-set operation here
/// already trusts. The track number is always `1 + position`; nothing about a file's
/// current name is consulted for it (docs/tagging.md §5: "always derived from position,
/// never typed").
pub fn plan_rename(files: &[PathBuf], spec: &NameSpec) -> RenamePlan {
    let mut entries: Vec<RenameEntry> = files
        .iter()
        .enumerate()
        .map(|(i, from)| RenameEntry {
            from: from.clone(),
            to: target_path(from, spec, i as u32 + 1),
            status: RenameStatus::Unchanged,
        })
        .collect();

    let mut targets: HashMap<PathBuf, u32> = HashMap::with_capacity(entries.len());
    for e in &entries {
        *targets.entry(e.to.clone()).or_insert(0) += 1;
    }
    for e in &mut entries {
        e.status = if targets[&e.to] > 1 {
            RenameStatus::Collision
        } else if e.to == e.from {
            RenameStatus::Unchanged
        } else {
            RenameStatus::Changed
        };
    }
    RenamePlan { entries }
}

/// Rename touches files only (docs/tagging.md §2): the target sits beside the source,
/// in the same directory, with the source's own extension. Only the file name changes.
fn target_path(from: &Path, spec: &NameSpec, track: u32) -> PathBuf {
    let file_name = from.file_name().unwrap_or_default().to_string_lossy();
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_string())
        .unwrap_or_default();
    let suffix = spec
        .keep_suffix
        .then(|| TrackName::parse(file_name.as_ref()).and_then(|t| t.suffix))
        .flatten();
    let name = TrackName {
        band: spec.band.clone(),
        date: spec.date,
        year_form: spec.year_form(),
        disc: spec.disc,
        track,
        suffix,
        ext,
    };
    from.with_file_name(name.render())
}

/// Execute a [`RenamePlan`], all at once or not at all — the same guarantee
/// [`crate::output::commit_all`] gives a repair.
///
/// Refuses outright if any entry is [`RenameStatus::Collision`]: nothing is renamed in
/// that case, not even the entries that were fine.
///
/// **Two-phase**: every [`RenameStatus::Changed`] file is first renamed to a unique
/// temporary name beside it, then every temporary name is renamed to its real target.
/// One phase would break on two shapes a plan can legitimately have: a permutation
/// (`t01`↔`t02`, where a direct rename would clobber one file mid-swap) and a
/// case-only rename (`T01`→`t01`), which a case-insensitive filesystem — the default on
/// macOS and Windows — refuses as a no-op on the file already sitting there
/// (`PLAN.md` §9 already tracks Windows path handling as a risk).
///
/// A failure at either phase rolls every rename already completed *in this call* back to
/// its original name before the error is returned — the same all-or-nothing shape
/// `output::commit_all` gives `sbe fix`.
pub fn execute_rename(plan: &RenamePlan) -> Result<Vec<PathBuf>> {
    if plan.has_collisions() {
        return Err(Error::malformed(
            "<set>",
            "rename plan has collisions; refusing to rename any file",
        ));
    }

    let changed: Vec<&RenameEntry> = plan
        .entries
        .iter()
        .filter(|e| e.status == RenameStatus::Changed)
        .collect();

    let pid = std::process::id();
    let temps: Vec<PathBuf> = changed
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let name = e.to.file_name().unwrap_or_default().to_string_lossy();
            e.to.with_file_name(format!(".{name}.lh-rename-{pid}-{i}.tmp"))
        })
        .collect();

    // Phase 1: `from` -> temp, so nothing below operates on a name anything else in this
    // plan is still using.
    for done in 0..changed.len() {
        if let Err(source) = std::fs::rename(&changed[done].from, &temps[done]) {
            for i in (0..done).rev() {
                let _ = std::fs::rename(&temps[i], &changed[i].from);
            }
            return Err(Error::io(&changed[done].from, source));
        }
    }

    // Phase 2: temp -> the real target.
    for done in 0..changed.len() {
        if let Err(source) = std::fs::rename(&temps[done], &changed[done].to) {
            // Put back what phase 2 already moved into place, then unwind phase 1 for
            // the whole batch — every changed file must end this call back at `from`.
            for i in (0..done).rev() {
                let _ = std::fs::rename(&changed[i].to, &temps[i]);
            }
            for i in 0..changed.len() {
                let _ = std::fs::rename(&temps[i], &changed[i].from);
            }
            return Err(Error::io(&changed[done].to, source));
        }
    }

    Ok(plan.entries.iter().map(|e| e.to.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> NameSpec {
        NameSpec {
            band: "gd".to_string(),
            date: ShowDate::new(1977, 5, 8).unwrap(),
            short_year: false,
            disc: None,
            keep_suffix: false,
        }
    }

    #[test]
    fn tracks_are_numbered_by_position_and_zero_padded() {
        let files = [
            PathBuf::from("show/a.flac"),
            PathBuf::from("show/b.flac"),
            PathBuf::from("show/c.flac"),
        ];
        let plan = plan_rename(&files, &spec());
        assert_eq!(
            plan.entries.iter().map(|e| &e.to).collect::<Vec<_>>(),
            [
                &PathBuf::from("show/gd1977-05-08t01.flac"),
                &PathBuf::from("show/gd1977-05-08t02.flac"),
                &PathBuf::from("show/gd1977-05-08t03.flac"),
            ]
        );
        assert!(
            plan.entries
                .iter()
                .all(|e| e.status == RenameStatus::Changed)
        );
    }

    #[test]
    fn a_file_already_named_correctly_is_unchanged() {
        let files = [PathBuf::from("show/gd1977-05-08t01.flac")];
        let plan = plan_rename(&files, &spec());
        assert_eq!(plan.entries[0].status, RenameStatus::Unchanged);
        assert_eq!(plan.entries[0].to, plan.entries[0].from);
    }

    #[test]
    fn short_year_and_disc_render_into_the_target() {
        let files = [PathBuf::from("show/a.shn")];
        let mut s = spec();
        s.short_year = true;
        s.disc = Some(2);
        let plan = plan_rename(&files, &s);
        assert_eq!(
            plan.entries[0].to,
            PathBuf::from("show/gd77-05-08d2t01.shn")
        );
    }

    #[test]
    fn keep_suffix_carries_an_existing_title_forward() {
        let files = [PathBuf::from("show/gd1973-02-09d1t01bertha.shn")];
        let mut s = spec();
        s.keep_suffix = true;
        s.disc = Some(1);
        s.date = ShowDate::new(1973, 2, 9).unwrap();
        let plan = plan_rename(&files, &s);
        assert_eq!(
            plan.entries[0].to,
            PathBuf::from("show/gd1973-02-09d1t01bertha.shn")
        );
        assert_eq!(plan.entries[0].status, RenameStatus::Unchanged);
    }

    /// Without `keep_suffix`, an existing title suffix is dropped, not carried.
    #[test]
    fn suffix_is_dropped_unless_keep_suffix_is_set() {
        let files = [PathBuf::from("show/gd1973-02-09d1t01bertha.shn")];
        let mut s = spec();
        s.disc = Some(1);
        s.date = ShowDate::new(1973, 2, 9).unwrap();
        let plan = plan_rename(&files, &s);
        assert_eq!(
            plan.entries[0].to,
            PathBuf::from("show/gd1973-02-09d1t01.shn")
        );
        assert_eq!(plan.entries[0].status, RenameStatus::Changed);
    }

    #[test]
    fn a_non_etree_name_has_no_suffix_to_keep() {
        let files = [PathBuf::from("show/01 - Bertha.flac")];
        let mut s = spec();
        s.keep_suffix = true;
        let plan = plan_rename(&files, &s);
        assert_eq!(
            plan.entries[0].to,
            PathBuf::from("show/gd1977-05-08t01.flac")
        );
    }

    /// `execute_rename` refuses a colliding plan outright, and touches nothing — not even
    /// the entry that would have been fine. Built by hand: `plan_rename` cannot itself
    /// produce one, since distinct positions always carry distinct track numbers and so
    /// always render distinct names; the check exists as a safety net regardless
    /// (docs/tagging.md §4).
    #[test]
    fn execute_rename_refuses_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.flac");
        let b = dir.path().join("b.flac");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let target = dir.path().join("gd1977-05-08t01.flac");

        let plan = RenamePlan {
            entries: vec![
                RenameEntry {
                    from: a.clone(),
                    to: target.clone(),
                    status: RenameStatus::Collision,
                },
                RenameEntry {
                    from: b.clone(),
                    to: target,
                    status: RenameStatus::Collision,
                },
            ],
        };
        let err = execute_rename(&plan).unwrap_err();
        assert!(err.to_string().contains("collision"), "{err}");
        assert!(a.exists());
        assert!(b.exists());
    }

    /// A permutation (`t01`<->`t02`) is exactly what the two-phase approach exists for: a
    /// direct rename would clobber one file mid-swap.
    #[test]
    fn execute_rename_handles_a_full_permutation() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("gd1977-05-08t02.flac"); // holds what should become t01
        let second = dir.path().join("gd1977-05-08t01.flac"); // holds what should become t02
        std::fs::write(&first, b"track one content").unwrap();
        std::fs::write(&second, b"track two content").unwrap();

        // Position order is [first, second], so plan_rename wants first -> t01, second -> t02.
        let files = [first.clone(), second.clone()];
        let plan = plan_rename(&files, &spec());
        assert_eq!(plan.entries[0].status, RenameStatus::Changed);
        assert_eq!(plan.entries[1].status, RenameStatus::Changed);

        let result = execute_rename(&plan).unwrap();
        assert_eq!(result, vec![second.clone(), first.clone()]);
        assert_eq!(std::fs::read(&second).unwrap(), b"track one content");
        assert_eq!(std::fs::read(&first).unwrap(), b"track two content");
    }

    /// A case-only rename (`T01` -> `t01`) is a no-op direct `rename()` on a
    /// case-insensitive filesystem; going through a temp name first is what makes it a
    /// real rename everywhere.
    #[test]
    fn execute_rename_handles_a_case_only_rename() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("GD1977-05-08T01.flac");
        let to = dir.path().join("gd1977-05-08t01.flac");
        std::fs::write(&from, b"audio").unwrap();

        let plan = RenamePlan {
            entries: vec![RenameEntry {
                from: from.clone(),
                to: to.clone(),
                status: RenameStatus::Changed,
            }],
        };
        let result = execute_rename(&plan).unwrap();
        assert_eq!(result, vec![to.clone()]);
        assert_eq!(std::fs::read(&to).unwrap(), b"audio");
    }

    /// A failure partway through phase 2 must leave every file back where it started —
    /// the third target is blocked by a pre-existing directory, forcing that rename to
    /// fail after the first two have already succeeded.
    #[test]
    fn a_failure_rolls_back_every_completed_rename() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        std::fs::write(&c, b"c").unwrap();
        let (to_a, to_b, to_c) = (
            dir.path().join("a-new"),
            dir.path().join("b-new"),
            dir.path().join("c-new"),
        );
        std::fs::create_dir(&to_c).unwrap(); // blocks the third rename in phase 2

        let plan = RenamePlan {
            entries: vec![
                RenameEntry {
                    from: a.clone(),
                    to: to_a.clone(),
                    status: RenameStatus::Changed,
                },
                RenameEntry {
                    from: b.clone(),
                    to: to_b.clone(),
                    status: RenameStatus::Changed,
                },
                RenameEntry {
                    from: c.clone(),
                    to: to_c.clone(),
                    status: RenameStatus::Changed,
                },
            ],
        };

        let err = execute_rename(&plan).unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err}");
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        assert_eq!(std::fs::read(&c).unwrap(), b"c");
        assert!(!to_a.exists());
        assert!(!to_b.exists());
        assert!(to_c.is_dir()); // untouched, never renamed onto
    }
}
