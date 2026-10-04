// SPDX-License-Identifier: MIT OR Apache-2.0

//! Library root — re-exports modules for integration testing.

pub mod backend;
pub mod deep_link;
pub mod discovery;
pub mod jni_util;
#[cfg(not(target_arch = "wasm32"))]
pub mod lifecycle;
pub mod settings;
pub mod timer;
