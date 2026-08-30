//! Running a reference tool, and the record it leaves behind.
//!
//! Principle 2: provenance is a feature, not plumbing. Every operation records which tool
//! ran, its version, and the exact argv — and that includes the in-process operations,
//! because "nothing external touched this file" is itself an answer worth recording.

use super::{Tool, ToolId};
use crate::error::{Error, Result};
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Who did the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    /// Pure Rust, inside `lh-core`. Nobody cares what *read* a file (Principle 3), but
    /// the audit trail should still say that nothing else did.
    InProcess { crate_version: &'static str },
    /// A reference binary, recorded exactly enough to be re-run and audited.
    Tool {
        id: ToolId,
        path: PathBuf,
        /// The tool's own version line, verbatim.
        version: String,
        sha256: String,
        /// The exact argv, argv[0] included.
        argv: Vec<String>,
    },
}

impl Agent {
    /// The in-process agent, stamped with this build of `lh-core`.
    pub fn in_process() -> Self {
        Agent::InProcess {
            crate_version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// What happened to one file, and who made it happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Human-readable, e.g. "FLAC → WAV".
    pub operation: String,
    pub agent: Agent,
    pub input: PathBuf,
    pub output: PathBuf,
}

impl Provenance {
    /// The audit-log rendering: the operation, then who did it, then the argv if there
    /// was one. Indented as a block so it reads under a per-file heading.
    pub fn render(&self) -> String {
        let mut s = format!(
            "{}: {} -> {}\n",
            self.operation,
            self.input.display(),
            self.output.display()
        );
        match &self.agent {
            Agent::InProcess { crate_version } => {
                s.push_str(&format!("  in-process (lh-core {crate_version})\n"));
            }
            Agent::Tool {
                path,
                version,
                sha256,
                argv,
                ..
            } => {
                s.push_str(&format!("  {version}\n"));
                s.push_str(&format!("  {} (sha256 {sha256})\n", path.display()));
                s.push_str(&format!("  argv: {}\n", argv.join(" ")));
            }
        }
        s
    }
}

/// Run a reference tool to completion and return the record of what ran.
///
/// A non-zero exit is an error carrying the tool's own stderr: when `flac` refuses a file,
/// the user should read what `flac` said, not a paraphrase of it (Principle 5).
pub fn run(tool: &Tool, args: &[OsString]) -> Result<Agent> {
    run_cancellable(tool, args, &mut || true)
}

/// Like [`run`], but polls `should_continue` while the child is running and kills it the
/// moment that returns `false`, rather than blocking until it exits on its own —
/// `Command::output()` (`run`'s old body) has no such door in. This is what makes a
/// WAV → FLAC job actually stoppable mid-run (docs/job-queue.md §8); `run` is this with a
/// `should_continue` that never says stop.
///
/// A killed child is reported as [`Error::Cancelled`], the same as a job the queue never
/// started — from the caller's side, "asked to stop, and did" should look the same either
/// way it happened.
pub fn run_cancellable(
    tool: &Tool,
    args: &[OsString],
    should_continue: &mut dyn FnMut() -> bool,
) -> Result<Agent> {
    let mut argv = vec![tool.path.display().to_string()];
    argv.extend(args.iter().map(|a| a.to_string_lossy().into_owned()));

    let mut child = Command::new(&tool.path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::ToolFailed {
            tool: tool.id.name(),
            argv: argv.join(" "),
            status: "could not be started".into(),
            detail: e.to_string(),
        })?;

    // Drained on their own threads rather than after the wait loop below: polling
    // `try_wait()` can leave the child sitting between checks, and a full pipe would
    // block it. `flac`'s own output is small in practice, but nothing here should rely
    // on that staying true.
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if !should_continue() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(Error::ToolFailed {
                    tool: tool.id.name(),
                    argv: argv.join(" "),
                    status: "could not be waited on".into(),
                    detail: e.to_string(),
                });
            }
        }
    };

    let stderr = stderr_thread.join().unwrap_or_default();
    let _stdout = stdout_thread.join().unwrap_or_default();

    if !status.success() {
        return Err(Error::ToolFailed {
            tool: tool.id.name(),
            argv: argv.join(" "),
            status: match status.code() {
                Some(c) => format!("exit status {c}"),
                None => "killed by a signal".into(),
            },
            detail: last_lines(&stderr, 5),
        });
    }

    Ok(Agent::Tool {
        id: tool.id,
        path: tool.path.clone(),
        version: tool.version.clone(),
        sha256: tool.sha256.clone(),
        argv,
    })
}

/// Tools are chatty on stderr — `flac` draws a progress display there — so quote the tail,
/// which is where the complaint is, rather than the whole transcript.
fn last_lines(bytes: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(n);
    if lines.is_empty() {
        "it said nothing".to_string()
    } else {
        lines[start..].join("; ")
    }
}
