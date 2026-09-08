// SPDX-License-Identifier: MIT OR Apache-2.0

//! blue2th PC backend binary.
//!
//! Thin launcher: all routing/logic lives in the library crate (`lib.rs`) so it
//! can be exercised in-process by integration tests. See `docs/ARCHITECTURE.md`.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    blue2th_server::run().await
}
