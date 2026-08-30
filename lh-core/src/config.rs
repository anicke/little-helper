//! Where Little Helper keeps the few things a user configures.
//!
//! This is not the config module PLAN.md §3 describes — there is no serde and no `toml`
//! here yet, because the only thing that needs configuring so far is the tracker list, and
//! that has a format already: the `Name|URL` one Trader's Little Helper shipped. A settings
//! file earns serde when there are settings; one list file does not.
//!
//! TLH moved its own `tracker.lst` out of the program directory into `%APPDATA%` so an
//! edited list could be saved without administrative rights. Same idea, per-platform:
//!
//! * Linux — `~/.config/little-helper/`
//! * macOS — `~/Library/Application Support/little-helper/`
//! * Windows — `%APPDATA%\little-helper\config\`
//!
//! `LH_CONFIG_DIR` overrides all of it. That is how the tests get a directory of their own,
//! and it matches the `LH_FLAC` idiom the tool registry already uses.

use std::path::PathBuf;

/// The environment variable that overrides the whole search, the way `LH_FLAC` does for
/// the tool registry: if it is set, we look there and nowhere else.
pub const CONFIG_DIR_ENV: &str = "LH_CONFIG_DIR";

/// Where a config file would live. `None` when the platform will not tell us — a headless
/// build with no home directory, say — which is not an error: everything that reads from
/// here has a bundled default.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "little-helper")
        .map(|dirs| dirs.config_dir().to_path_buf())
}

/// The full path of a config file, whether or not it exists.
pub fn config_path(file_name: &str) -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(file_name))
}
