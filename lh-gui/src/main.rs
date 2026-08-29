//! Little Helper desktop application.
//!
//! Not built yet — milestone M3 in PLAN.md. The engine lands first and the CLI proves it
//! is right, so this crate stays a placeholder until `lh-core` and `lh` are done.

fn main() {
    eprintln!(
        "The Little Helper GUI is not implemented yet (milestone M3).\n\
         Use the `lh` command line in the meantime: `lh --help`."
    );
    std::process::exit(2);
}
