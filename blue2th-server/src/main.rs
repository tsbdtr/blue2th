// SPDX-License-Identifier: MIT OR Apache-2.0

//! blue2th PC backend binary.
//!
//! Thin launcher: all routing/logic lives in the library crate (`lib.rs`) so it
//! can be exercised in-process by integration tests. See `docs/ARCHITECTURE.md`.

use clap::Parser;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parsed here rather than returned: clap prints its own message (or the
    // help) and exits with its own code, where a returned `clap::Error` would
    // exit 1 with its `Debug` dump.
    let cli = blue2th_server::Cli::parse();
    blue2th_server::run(cli).await
}
