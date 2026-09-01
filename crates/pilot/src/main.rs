//! pilot — the Harbor client, as its own name.
//!
//! The code lives in `harbor::repl`; this shim keeps the standalone
//! binary (and the muscle memory) alive. One crate, one version: the
//! two binaries can never disagree about the protocol again.

use std::process::ExitCode;

fn main() -> ExitCode {
    harbor::repl::cli_main(std::env::args().skip(1))
}
