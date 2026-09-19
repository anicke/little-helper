use anyhow::Result;
use lh_core::tools::{Discovery, Registry, ToolId};

/// The Tools panel, headless. Every operation that shells out logs the same facts this
/// prints, so a trader can tell what produced a file before they trust it (Principle 2).
pub(crate) fn cmd_tools() -> Result<bool> {
    let registry = Registry::discover();
    for (id, discovery) in registry.entries() {
        match discovery {
            Discovery::Found(t) => {
                println!(
                    "{:<10} {}  ({})",
                    id.name(),
                    t.path.display(),
                    t.source.label()
                );
                println!("{:<10} {}", "", t.version);
                println!("{:<10} sha256 {}", "", t.sha256);
            }
            Discovery::Unusable { path, reason } => {
                println!("{:<10} {} is unusable", id.name(), path.display());
                println!("{:<10} {reason}", "");
            }
            Discovery::NotFound { searched } => {
                let need = if id.is_required() {
                    "needed for"
                } else {
                    "only needed for"
                };
                println!("{:<10} not found — {need} {}", id.name(), id.purpose());
                println!("{:<10} looked in {}", "", searched.join(", "));
                println!(
                    "{:<10} point at your own with {}=/path/to/{}",
                    "",
                    id.env_var(),
                    id.name()
                );
            }
        }
    }

    let missing: Vec<ToolId> = registry.missing_required().collect();
    if missing.is_empty() {
        Ok(true)
    } else {
        println!();
        for id in missing {
            println!(
                "{} is required and was not found; {} will not run",
                id,
                id.purpose()
            );
        }
        Ok(false)
    }
}
